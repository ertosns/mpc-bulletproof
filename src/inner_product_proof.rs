#![allow(non_snake_case)]
#![doc = include_str!("../docs/inner-product-protocol.md")]

extern crate alloc;

use alloc::borrow::Borrow;
use alloc::vec::Vec;
use itertools::Itertools;
use mpc_stark::algebra::scalar::{Scalar, SCALAR_BYTES};
use mpc_stark::algebra::stark_curve::{StarkPoint, STARK_POINT_BYTES};
use rayon::prelude::*;
use unzip_n::unzip_n;

use core::iter;
use merlin::HashChainTranscript as Transcript;

use crate::errors::ProofError;
use crate::transcript::TranscriptProtocol;

unzip_n!(4);

/// The size of the inner product proof above which we execute folding operations
/// in parallel
///
/// Copied from `mpc-stark`
const PARALLELISM_THRESHOLD: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InnerProductProof {
    pub L_vec: Vec<StarkPoint>,
    pub R_vec: Vec<StarkPoint>,
    pub a: Scalar,
    pub b: Scalar,
}

#[allow(clippy::too_many_arguments)]
impl InnerProductProof {
    /// Create an inner-product proof.
    ///
    /// The proof is created with respect to the bases \\(G\\), \\(H'\\),
    /// where \\(H'\_i = H\_i \cdot \texttt{Hprime\\_factors}\_i\\).
    ///
    /// The `verifier` is passed in as a parameter so that the
    /// challenges depend on the *entire* transcript (including parent
    /// protocols).
    ///
    /// The lengths of the vectors must all be the same, and must all be
    /// either 0 or a power of 2.
    pub fn create(
        transcript: &mut Transcript,
        Q: &StarkPoint,
        G_factors: &[Scalar],
        H_factors: &[Scalar],
        mut G_vec: Vec<StarkPoint>,
        mut H_vec: Vec<StarkPoint>,
        mut a_vec: Vec<Scalar>,
        mut b_vec: Vec<Scalar>,
    ) -> InnerProductProof {
        let mut n = G_vec.len();

        // All of the input vectors must have the same length.
        assert_eq!(G_vec.len(), n);
        assert_eq!(H_vec.len(), n);
        assert_eq!(a_vec.len(), n);
        assert_eq!(b_vec.len(), n);
        assert_eq!(G_factors.len(), n);
        assert_eq!(H_factors.len(), n);

        // All of the input vectors must have a length that is a power of two.
        assert!(n.is_power_of_two());

        transcript.innerproduct_domain_sep(n as u64);

        let lg_n = n.next_power_of_two().trailing_zeros() as usize;
        let mut L_vec = Vec::with_capacity(lg_n);
        let mut R_vec = Vec::with_capacity(lg_n);

        // If it's the first iteration, unroll the Hprime = H*y_inv scalar mults
        // into multiscalar muls, for performance.
        if n != 1 {
            n /= 2;
            let (a_L, a_R) = a_vec.split_at_mut(n);
            let (b_L, b_R) = b_vec.split_at_mut(n);
            let (G_L, G_R) = G_vec.split_at_mut(n);
            let (H_L, H_R) = H_vec.split_at_mut(n);

            let c_L = inner_product(a_L, b_R);
            let c_R = inner_product(a_R, b_L);

            let L = StarkPoint::msm_iter(
                a_L.iter()
                    .zip(G_factors[n..2 * n].iter())
                    .map(|(a_L_i, g)| a_L_i * g)
                    .chain(
                        b_R.iter()
                            .zip(H_factors[0..n].iter())
                            .map(|(b_R_i, h)| b_R_i * h),
                    )
                    .chain(iter::once(c_L)),
                G_R.iter().chain(H_L.iter()).chain(iter::once(Q)).copied(),
            );

            let R = StarkPoint::msm_iter(
                a_R.iter()
                    .zip(G_factors[0..n].iter())
                    .map(|(a_R_i, g)| a_R_i * g)
                    .chain(
                        b_L.iter()
                            .zip(H_factors[n..2 * n].iter())
                            .map(|(b_L_i, h)| b_L_i * h),
                    )
                    .chain(iter::once(c_R)),
                G_L.iter().chain(H_R.iter()).chain(iter::once(Q)).copied(),
            );

            L_vec.push(L);
            R_vec.push(R);

            transcript.append_point(b"L", &L);
            transcript.append_point(b"R", &R);

            let u = transcript.challenge_scalar(b"u");
            let u_inv = u.inverse();

            let G = G_factors
                .iter()
                .zip(G_vec.into_iter())
                .map(|(g, G_i)| g * G_i)
                .collect_vec();
            let H = H_factors
                .iter()
                .zip(H_vec.into_iter())
                .map(|(h, H_i)| h * H_i)
                .collect_vec();
            (a_vec, b_vec, G_vec, H_vec) = Self::fold_witness(
                u,
                u_inv,
                a_L,
                a_R,
                b_L,
                b_R,
                &G[..n],
                &G[n..],
                &H[..n],
                &H[n..],
            );
        }

        while n != 1 {
            n /= 2;
            let (a_L, a_R) = a_vec.split_at_mut(n);
            let (b_L, b_R) = b_vec.split_at_mut(n);
            let (G_L, G_R) = G_vec.split_at_mut(n);
            let (H_L, H_R) = H_vec.split_at_mut(n);

            let c_L = inner_product(a_L, b_R);
            let c_R = inner_product(a_R, b_L);

            let L = StarkPoint::msm_iter(
                a_L.iter()
                    .chain(b_R.iter())
                    .chain(iter::once(&c_L))
                    .copied(),
                G_R.iter().chain(H_L.iter()).chain(iter::once(Q)).copied(),
            );
            let R = StarkPoint::msm_iter(
                a_R.iter()
                    .chain(b_L.iter())
                    .chain(iter::once(&c_R))
                    .copied(),
                G_L.iter().chain(H_R.iter()).chain(iter::once(Q)).copied(),
            );

            L_vec.push(L);
            R_vec.push(R);

            transcript.append_point(b"L", &L);
            transcript.append_point(b"R", &R);

            let u = transcript.challenge_scalar(b"u");
            let u_inv = u.inverse();

            (a_vec, b_vec, G_vec, H_vec) =
                Self::fold_witness(u, u_inv, a_L, a_R, b_L, b_R, G_L, G_R, H_L, H_R);
        }

        InnerProductProof {
            L_vec,
            R_vec,
            a: a_vec[0],
            b: b_vec[0],
        }
    }

    /// Reduces the inner product proof witness in half by folding the elements via
    /// a linear combination with multiplicative inverses
    ///
    /// See equation (4) of the Bulletproof paper:
    /// https://eprint.iacr.org/2017/1066.pdf
    ///
    /// Returns the new values of a, b, G, H
    fn fold_witness(
        u: Scalar,
        u_inv: Scalar,
        a_L: &[Scalar],
        a_R: &[Scalar],
        b_L: &[Scalar],
        b_R: &[Scalar],
        G_L: &[StarkPoint],
        G_R: &[StarkPoint],
        H_L: &[StarkPoint],
        H_R: &[StarkPoint],
    ) -> (Vec<Scalar>, Vec<Scalar>, Vec<StarkPoint>, Vec<StarkPoint>) {
        let n = a_L.len();

        // For small proofs, compute serially to avoid parallelism overhead
        if n < PARALLELISM_THRESHOLD {
            let mut a_res = Vec::with_capacity(n / 2);
            let mut b_res = Vec::with_capacity(n / 2);
            let mut G_res = Vec::with_capacity(n / 2);
            let mut H_res = Vec::with_capacity(n / 2);

            for i in 0..n {
                a_res.push(a_L[i] * u + u_inv * a_R[i]);
                b_res.push(b_L[i] * u_inv + u * b_R[i]);
                G_res.push(StarkPoint::msm(&[u_inv, u], &[G_L[i], G_R[i]]));
                H_res.push(StarkPoint::msm(&[u, u_inv], &[H_L[i], H_R[i]]));
            }

            return (a_res, b_res, G_res, H_res);
        }

        // Parallel implementation
        let mut res = Vec::with_capacity(n);
        (0..n)
            .into_par_iter()
            .map(|i| {
                (
                    a_L[i] * u + u_inv * a_R[i],
                    b_L[i] * u_inv + u * b_R[i],
                    StarkPoint::msm(&[u_inv, u], &[G_L[i], G_R[i]]),
                    StarkPoint::msm(&[u, u_inv], &[H_L[i], H_R[i]]),
                )
            })
            .collect_into_vec(&mut res);

        res.into_iter().unzip_n_vec()
    }

    /// Computes three vectors of verification scalars \\([u\_{i}^{2}]\\), \\([u\_{i}^{-2}]\\) and \\([s\_{i}]\\) for combined multiscalar multiplication
    /// in a parent protocol. See [inner product protocol notes](index.html#verification-equation) for details.
    /// The verifier must provide the input length \\(n\\) explicitly to avoid unbounded allocation within the inner product proof.
    #[allow(clippy::type_complexity)]
    pub fn verification_scalars(
        &self,
        n: usize,
        transcript: &mut Transcript,
    ) -> Result<(Vec<Scalar>, Vec<Scalar>, Vec<Scalar>), ProofError> {
        let lg_n = self.L_vec.len();
        if lg_n >= 32 {
            // 4 billion multiplications should be enough for anyone
            // and this check prevents overflow in 1<<lg_n below.
            return Err(ProofError::VerificationError);
        }
        if n != (1 << lg_n) {
            return Err(ProofError::VerificationError);
        }

        transcript.innerproduct_domain_sep(n as u64);

        // 1. Recompute x_k,...,x_1 based on the proof transcript

        let mut challenges = Vec::with_capacity(lg_n);
        for (L, R) in self.L_vec.iter().zip(self.R_vec.iter()) {
            transcript.validate_and_append_point(b"L", L)?;
            transcript.validate_and_append_point(b"R", R)?;
            challenges.push(transcript.challenge_scalar(b"u"));
        }

        // 2. Compute 1/(u_k...u_1) and 1/u_k, ..., 1/u_1

        let mut challenges_inv = challenges.clone();
        Scalar::batch_inverse(&mut challenges_inv);
        let allinv = challenges_inv.iter().copied().product();

        // 3. Compute u_i^2 and (1/u_i)^2

        for i in 0..lg_n {
            // XXX missing square fn upstream
            challenges[i] = challenges[i] * challenges[i];
            challenges_inv[i] = challenges_inv[i] * challenges_inv[i];
        }
        let challenges_sq = challenges;
        let challenges_inv_sq = challenges_inv;

        // 4. Compute s values inductively.

        let mut s = Vec::with_capacity(n);
        s.push(allinv);
        for i in 1..n {
            let lg_i = (32 - 1 - (i as u32).leading_zeros()) as usize;
            let k = 1 << lg_i;
            // The challenges are stored in "creation order" as [u_k,...,u_1],
            // so u_{lg(i)+1} = is indexed by (lg_n-1) - lg_i
            let u_lg_i_sq = challenges_sq[(lg_n - 1) - lg_i];
            s.push(s[i - k] * u_lg_i_sq);
        }

        Ok((challenges_sq, challenges_inv_sq, s))
    }

    /// This method is for testing that proof generation work,
    /// but for efficiency the actual protocols would use `verification_scalars`
    /// method to combine inner product verification with other checks
    /// in a single multiscalar multiplication.
    #[allow(dead_code)]
    pub fn verify<IG, IH>(
        &self,
        n: usize,
        transcript: &mut Transcript,
        G_factors: IG,
        H_factors: IH,
        P: &StarkPoint,
        Q: &StarkPoint,
        G: &[StarkPoint],
        H: &[StarkPoint],
    ) -> Result<(), ProofError>
    where
        IG: IntoIterator,
        IG::Item: Borrow<Scalar>,
        IH: IntoIterator,
        IH::Item: Borrow<Scalar>,
    {
        let (u_sq, u_inv_sq, s) = self.verification_scalars(n, transcript)?;

        let g_times_a_times_s = G_factors
            .into_iter()
            .zip(s.iter())
            .map(|(g_i, s_i)| (self.a * s_i) * g_i.borrow())
            .take(G.len());

        // 1/s[i] is s[!i], and !i runs from n-1 to 0 as i runs from 0 to n-1
        let inv_s = s.iter().rev();

        let h_times_b_div_s = H_factors
            .into_iter()
            .zip(inv_s)
            .map(|(h_i, s_i_inv)| (self.b * s_i_inv) * h_i.borrow());

        let neg_u_sq = u_sq.iter().map(|ui| -(*ui));
        let neg_u_inv_sq = u_inv_sq.iter().map(|ui| -(*ui));

        let expect_P = StarkPoint::msm_iter(
            iter::once(self.a * self.b)
                .chain(g_times_a_times_s)
                .chain(h_times_b_div_s)
                .chain(neg_u_sq)
                .chain(neg_u_inv_sq),
            iter::once(Q)
                .chain(G.iter())
                .chain(H.iter())
                .chain(self.L_vec.iter())
                .chain(self.R_vec.iter())
                .copied(),
        );

        if expect_P == *P {
            Ok(())
        } else {
            Err(ProofError::VerificationError)
        }
    }

    /// Returns the size in bytes required to serialize the inner
    /// product proof.
    ///
    /// For vectors of length `n` the proof size is
    /// \\(32 \cdot (2\lg n+2)\\) bytes.
    pub fn serialized_size(&self) -> usize {
        (self.L_vec.len() * 2) * STARK_POINT_BYTES + 2 * SCALAR_BYTES
    }

    /// Serializes the proof into a byte array of \\(2n+2\\) 32-byte elements.
    /// The layout of the inner product proof is:
    /// * \\(n\\) pairs of compressed Ristretto points \\(L_0, R_0 \dots, L_{n-1}, R_{n-1}\\),
    /// * two scalars \\(a, b\\).
    #[allow(dead_code)]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.serialized_size());
        for (l, r) in self.L_vec.iter().zip(self.R_vec.iter()) {
            buf.extend_from_slice(&l.to_bytes());
            buf.extend_from_slice(&r.to_bytes());
        }
        buf.extend_from_slice(&self.a.to_bytes_be());
        buf.extend_from_slice(&self.b.to_bytes_be());
        buf
    }

    /// Converts the proof into a byte iterator over serialized view of the proof.
    /// The layout of the inner product proof is:
    /// * \\(n\\) pairs of compressed Ristretto points \\(L_0, R_0 \dots, L_{n-1}, R_{n-1}\\),
    /// * two scalars \\(a, b\\).
    #[inline]
    pub(crate) fn to_bytes_iter(&self) -> impl Iterator<Item = u8> + '_ {
        self.L_vec
            .iter()
            .zip(self.R_vec.iter())
            .flat_map(|(l, r)| l.to_bytes().into_iter().chain(r.to_bytes().into_iter()))
            .chain(self.a.to_bytes_be().into_iter())
            .chain(self.b.to_bytes_be().into_iter())
    }

    /// Deserializes the proof from a byte slice.
    /// Returns an error in the following cases:
    /// * the slice does not have \\(2n+2\\) 32-byte elements,
    /// * \\(n\\) is larger or equal to 32 (proof is too big),
    /// * any of \\(2n\\) points are not valid compressed Ristretto points,
    /// * any of 2 scalars are not canonical scalars modulo Ristretto group order.
    pub fn from_bytes(slice: &[u8]) -> Result<InnerProductProof, ProofError> {
        let b = slice.len();

        // Two scalars (`a` and `b`) and then `log(n)` point pairs
        let num_points = (b - 2 * SCALAR_BYTES) / STARK_POINT_BYTES;
        let num_elements = num_points + 2;
        if num_elements < 2 {
            return Err(ProofError::FormatError);
        }
        if (num_elements - 2) % 2 != 0 {
            return Err(ProofError::FormatError);
        }
        let lg_n = (num_elements - 2) / 2;
        if lg_n >= 32 {
            return Err(ProofError::FormatError);
        }

        let mut L_vec: Vec<StarkPoint> = Vec::with_capacity(lg_n);
        let mut R_vec: Vec<StarkPoint> = Vec::with_capacity(lg_n);
        for i in 0..lg_n {
            let pos = 2 * i * STARK_POINT_BYTES;
            let l_point = StarkPoint::from_bytes(&slice[pos..pos + STARK_POINT_BYTES])
                .map_err(|_| ProofError::FormatError)?;
            let r_point = StarkPoint::from_bytes(
                &slice[pos + STARK_POINT_BYTES..pos + 2 * STARK_POINT_BYTES],
            )
            .map_err(|_| ProofError::FormatError)?;
            L_vec.push(l_point);
            R_vec.push(r_point);
        }

        let pos = 2 * lg_n * STARK_POINT_BYTES;
        let a = Scalar::from_be_bytes_mod_order(&slice[pos..pos + SCALAR_BYTES]);
        let b = Scalar::from_be_bytes_mod_order(&slice[pos + SCALAR_BYTES..]);

        Ok(InnerProductProof { L_vec, R_vec, a, b })
    }
}

/// Computes an inner product of two vectors
/// \\[
///    {\langle {\mathbf{a}}, {\mathbf{b}} \rangle} = \sum\_{i=0}^{n-1} a\_i \cdot b\_i.
/// \\]
/// Panics if the lengths of \\(\mathbf{a}\\) and \\(\mathbf{b}\\) are not equal.
pub fn inner_product(a: &[Scalar], b: &[Scalar]) -> Scalar {
    let mut out = Scalar::from(0);
    if a.len() != b.len() {
        panic!("inner_product(a,b): lengths of vectors do not match");
    }
    for i in 0..a.len() {
        out += a[i] * b[i];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::util;
    use mpc_stark::{algebra::stark_curve::StarkPoint, random_point};
    use rand::thread_rng;

    fn create_proof(n: usize) -> InnerProductProof {
        let mut rng = thread_rng();

        use crate::generators::BulletproofGens;
        let bp_gens = BulletproofGens::new(n, 1);
        let G: Vec<StarkPoint> = bp_gens.share(0).G(n).cloned().collect();
        let H: Vec<StarkPoint> = bp_gens.share(0).H(n).cloned().collect();

        // Q would be determined upstream in the protocol, so we pick a random one.
        let Q = random_point();

        // a and b are the vectors for which we want to prove c = <a,b>
        let a: Vec<_> = (0..n).map(|_| Scalar::random(&mut rng)).collect();
        let b: Vec<_> = (0..n).map(|_| Scalar::random(&mut rng)).collect();

        let G_factors: Vec<Scalar> = iter::repeat(Scalar::from(1)).take(n).collect();

        // y_inv is (the inverse of) a random challenge
        let y_inv = Scalar::random(&mut rng);
        let H_factors: Vec<Scalar> = util::exp_iter(y_inv).take(n).collect();

        let mut verifier = Transcript::new(b"innerproducttest");
        InnerProductProof::create(&mut verifier, &Q, &G_factors, &H_factors, G, H, a, b)
    }

    fn test_helper_create(n: usize) {
        let mut rng = thread_rng();

        use crate::generators::BulletproofGens;
        let bp_gens = BulletproofGens::new(n, 1);
        let G: Vec<StarkPoint> = bp_gens.share(0).G(n).cloned().collect();
        let H: Vec<StarkPoint> = bp_gens.share(0).H(n).cloned().collect();

        // Q would be determined upstream in the protocol, so we pick a random one.
        let Q = random_point();

        // a and b are the vectors for which we want to prove c = <a,b>
        let a: Vec<_> = (0..n).map(|_| Scalar::random(&mut rng)).collect();
        let b: Vec<_> = (0..n).map(|_| Scalar::random(&mut rng)).collect();
        let c = inner_product(&a, &b);

        let G_factors: Vec<Scalar> = iter::repeat(Scalar::from(1)).take(n).collect();

        // y_inv is (the inverse of) a random challenge
        let y_inv = Scalar::random(&mut rng);
        let H_factors: Vec<Scalar> = util::exp_iter(y_inv).take(n).collect();

        // P would be determined upstream, but we need a correct P to check the proof.
        //
        // To generate P = <a,G> + <b,H'> + <a,b> Q, compute
        //             P = <a,G> + <b',H> + <a,b> Q,
        // where b' = b \circ y^(-n)
        let b_prime = b.iter().zip(util::exp_iter(y_inv)).map(|(bi, yi)| bi * yi);
        // a.iter() has Item=&Scalar, need Item=Scalar to chain with b_prim
        let a_prime = a.iter().cloned();

        let P = StarkPoint::msm_iter(
            a_prime.chain(b_prime).chain(iter::once(c)),
            G.iter().chain(H.iter()).chain(iter::once(&Q)).copied(),
        );

        let mut verifier = Transcript::new(b"innerproducttest");
        let proof = InnerProductProof::create(
            &mut verifier,
            &Q,
            &G_factors,
            &H_factors,
            G.clone(),
            H.clone(),
            a.clone(),
            b.clone(),
        );

        let mut verifier = Transcript::new(b"innerproducttest");
        assert!(proof
            .verify(
                n,
                &mut verifier,
                iter::repeat(Scalar::from(1)).take(n),
                util::exp_iter(y_inv).take(n),
                &P,
                &Q,
                &G,
                &H
            )
            .is_ok());

        let proof = InnerProductProof::from_bytes(proof.to_bytes().as_slice()).unwrap();
        let mut verifier = Transcript::new(b"innerproducttest");
        assert!(proof
            .verify(
                n,
                &mut verifier,
                iter::repeat(Scalar::from(1)).take(n),
                util::exp_iter(y_inv).take(n),
                &P,
                &Q,
                &G,
                &H
            )
            .is_ok());
    }

    /// Test serializing an inner product proof to bytes and then deserializing it
    #[test]
    fn test_proof_to_from_bytes() {
        let proof = create_proof(2);
        let proof_bytes = proof.to_bytes();
        let reconstructed_proof = InnerProductProof::from_bytes(&proof_bytes).unwrap();

        assert_eq!(proof, reconstructed_proof);
    }

    #[test]
    fn make_ipp_1() {
        test_helper_create(1);
    }

    #[test]
    fn make_ipp_2() {
        test_helper_create(2);
    }

    #[test]
    fn make_ipp_4() {
        test_helper_create(4);
    }

    #[test]
    fn make_ipp_32() {
        test_helper_create(32);
    }

    #[test]
    fn make_ipp_64() {
        test_helper_create(64);
    }

    #[test]
    fn test_inner_product() {
        let a = vec![
            Scalar::from(1u64),
            Scalar::from(2u64),
            Scalar::from(3u64),
            Scalar::from(4u64),
        ];
        let b = vec![
            Scalar::from(2u64),
            Scalar::from(3u64),
            Scalar::from(4u64),
            Scalar::from(5u64),
        ];
        assert_eq!(Scalar::from(40u64), inner_product(&a, &b));
    }
}
