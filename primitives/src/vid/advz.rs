// Copyright (c) 2022 Espresso Systems (espressosys.com)
// This file is part of the Jellyfish library.

// You should have received a copy of the MIT License
// along with the Jellyfish library. If not, see <https://mit-license.org/>.

//! Implementation of Verifiable Information Dispersal (VID) from <https://eprint.iacr.org/2021/1500>.
//!
//! `advz` named for the authors Alhaddad-Duan-Varia-Zhang.

use super::{vid, VidDisperse, VidError, VidResult, VidScheme};
#[cfg(feature = "gpu-vid")]
use crate::icicle_deps::*;
use crate::{
    alloc::string::ToString,
    merkle_tree::{
        hasher::{HasherDigest, HasherMerkleTree, HasherNode},
        MerkleCommitment, MerkleTreeScheme,
    },
    pcs::{
        prelude::{UnivariateKzgPCS, UnivariateKzgProof},
        PolynomialCommitmentScheme, StructuredReferenceString, UnivariatePCS,
    },
    reed_solomon_code::reed_solomon_erasure_decode_rou,
};
use ark_ec::{pairing::Pairing, AffineRepr};
use ark_ff::{Field, PrimeField};
use ark_poly::{
    univariate::DensePolynomial, DenseUVPolynomial, EvaluationDomain, Radix2EvaluationDomain,
};
use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use ark_std::{
    borrow::Borrow,
    end_timer,
    fmt::Debug,
    format,
    marker::PhantomData,
    ops::{Add, Mul},
    start_timer, vec,
    vec::Vec,
    Zero,
};
use bytes_to_field::{bytes_to_field, field_to_bytes};
use core::mem;
use derivative::Derivative;
use digest::crypto_common::Output;
use jf_utils::{
    canonical,
    par_utils::{parallelizable_chunks, parallelizable_slice_iter},
};
#[cfg(feature = "parallel")]
use rayon::prelude::ParallelIterator;
use serde::{Deserialize, Serialize};

mod bytes_to_field;
pub mod payload_prover;
pub mod precomputable;

/// Normal Advz VID that's only using CPU
pub type Advz<E, H> = AdvzInternal<E, H, ()>;
/// Advz with GPU support
#[cfg(feature = "gpu-vid")]
pub type AdvzGPU<'srs, E, H> = AdvzInternal<
    E,
    H,
    (
        HostOrDeviceSlice<'srs, IcicleAffine<<UnivariateKzgPCS<E> as GPUCommittable<E>>::IC>>,
        CudaStream,
    ),
>;

/// The [ADVZ VID scheme](https://eprint.iacr.org/2021/1500), a concrete impl for [`VidScheme`].
/// Consider using either [`Advz`] or `AdvzGPU` (enabled via `gpu-vid` feature).
///
/// - `E` is any [`Pairing`]
/// - `H` is a [`digest::Digest`]-compatible hash function.
/// - `T` is a reference to GPU memory that's storing SRS
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdvzInternal<E, H, T>
where
    E: Pairing,
{
    recovery_threshold: u32,
    num_storage_nodes: u32,
    multiplicity: u32,
    ck: KzgProverParam<E>,
    vk: KzgVerifierParam<E>,
    multi_open_domain: Radix2EvaluationDomain<KzgPoint<E>>,

    // TODO might be able to eliminate this field and instead use
    // `EvaluationDomain::reindex_by_subdomain()` on `multi_open_domain`
    // but that method consumes `other` and its doc is unclear.
    eval_domain: Radix2EvaluationDomain<KzgPoint<E>>,

    // tuple of
    // - reference to the SRS/ProverParam loaded to GPU
    // - cuda stream handle
    srs_on_gpu_and_cuda_stream: Option<T>,
    _pd: (PhantomData<H>, PhantomData<T>),
}

// [Nested associated type projection is overly conservative · Issue #38078 · rust-lang/rust](https://github.com/rust-lang/rust/issues/38078)
// I want to do this but I can't:
// type Kzg<E> = <UnivariateKzgPCS<E> as PolynomialCommitmentScheme>;
// So instead I do this:
type KzgPolynomial<E> = <UnivariateKzgPCS<E> as PolynomialCommitmentScheme>::Polynomial;
type KzgCommit<E> = <UnivariateKzgPCS<E> as PolynomialCommitmentScheme>::Commitment;
type KzgPoint<E> = <UnivariateKzgPCS<E> as PolynomialCommitmentScheme>::Point;
type KzgEval<E> = <UnivariateKzgPCS<E> as PolynomialCommitmentScheme>::Evaluation;
type KzgProof<E> = <UnivariateKzgPCS<E> as PolynomialCommitmentScheme>::Proof;
type KzgSrs<E> = <UnivariateKzgPCS<E> as PolynomialCommitmentScheme>::SRS;
type KzgProverParam<E> = <<UnivariateKzgPCS<E> as PolynomialCommitmentScheme>::SRS as StructuredReferenceString>::ProverParam;
type KzgVerifierParam<E> = <<UnivariateKzgPCS<E> as PolynomialCommitmentScheme>::SRS as StructuredReferenceString>::VerifierParam;

type KzgEvalsMerkleTree<E, H> = HasherMerkleTree<H, Vec<KzgEval<E>>>;
type KzgEvalsMerkleTreeNode<E, H> = <KzgEvalsMerkleTree<E, H> as MerkleTreeScheme>::NodeValue;
type KzgEvalsMerkleTreeIndex<E, H> = <KzgEvalsMerkleTree<E, H> as MerkleTreeScheme>::Index;
type KzgEvalsMerkleTreeProof<E, H> =
    <KzgEvalsMerkleTree<E, H> as MerkleTreeScheme>::MembershipProof;

impl<E, H, T> AdvzInternal<E, H, T>
where
    E: Pairing,
{
    pub(crate) fn new_internal(
        num_storage_nodes: u32,  // n (code rate: r = k/n)
        recovery_threshold: u32, // k
        srs: impl Borrow<KzgSrs<E>>,
    ) -> VidResult<Self> {
        // TODO intelligent choice of multiplicity
        // https://github.com/EspressoSystems/jellyfish/issues/534
        let multiplicity = 1;

        Self::with_multiplicity_internal(num_storage_nodes, recovery_threshold, multiplicity, srs)
    }

    pub(crate) fn with_multiplicity_internal(
        num_storage_nodes: u32,  // n (code rate: r = k/n)
        recovery_threshold: u32, // k
        multiplicity: u32,       // batch m chunks, keep the rate r = (m*k)/(m*n)
        srs: impl Borrow<KzgSrs<E>>,
    ) -> VidResult<Self> {
        // TODO support any degree, give multiple shares to nodes if needed
        // https://github.com/EspressoSystems/jellyfish/issues/393
        if num_storage_nodes < recovery_threshold {
            return Err(VidError::Argument(format!(
                "recovery_threshold {} exceeds num_storage_nodes {}",
                recovery_threshold, num_storage_nodes
            )));
        }

        if !multiplicity.is_power_of_two() {
            return Err(VidError::Argument(format!(
                "multiplicity {multiplicity} should be a power of two"
            )));
        }

        // erasure code params
        let chunk_size = multiplicity * recovery_threshold; // message length m
        let code_word_size = multiplicity * num_storage_nodes; // code word length n
        let poly_degree = chunk_size - 1;

        let (ck, vk) = UnivariateKzgPCS::trim_fft_size(srs, poly_degree as usize).map_err(vid)?;
        let multi_open_domain = UnivariateKzgPCS::<E>::multi_open_rou_eval_domain(
            poly_degree as usize,
            code_word_size as usize,
        )
        .map_err(vid)?;
        let eval_domain = Radix2EvaluationDomain::new(chunk_size as usize).ok_or_else(|| {
            VidError::Internal(anyhow::anyhow!(
                "fail to construct domain of size {}",
                chunk_size
            ))
        })?;

        // TODO TEMPORARY: enforce power-of-2 chunk size
        // Remove this restriction after we get KZG in eval form
        // https://github.com/EspressoSystems/jellyfish/issues/339
        if chunk_size as usize != eval_domain.size() {
            return Err(VidError::Argument(format!(
                "recovery_threshold {} currently unsupported, round to {} instead",
                chunk_size,
                eval_domain.size()
            )));
        }

        Ok(Self {
            recovery_threshold,
            num_storage_nodes,
            multiplicity,
            ck,
            vk,
            multi_open_domain,
            eval_domain,
            srs_on_gpu_and_cuda_stream: None,
            _pd: Default::default(),
        })
    }
}

impl<E, H> Advz<E, H>
where
    E: Pairing,
{
    /// Return a new instance of `Self` from (mostly)
    /// implementation-independent arguments.
    ///
    /// # Implementation-independent arguments
    /// - `num_storage_nodes`
    /// - `recovery_threshold`
    ///
    /// # Implementation-specific arguments
    /// - `srs`
    ///
    /// # Errors
    /// Return [`VidError::Argument`] if
    /// - `num_storage_nodes < recovery_threshold`
    /// - TEMPORARY `recovery_threshold` is not a power of two [github issue](https://github.com/EspressoSystems/jellyfish/issues/339)
    pub fn new(
        num_storage_nodes: u32,
        recovery_threshold: u32,
        srs: impl Borrow<KzgSrs<E>>,
    ) -> VidResult<Self> {
        Self::new_internal(num_storage_nodes, recovery_threshold, srs)
    }

    /// Like [`Advz::new`] except with a `multiplicity` arg.
    ///
    /// `multiplicity` is an implementation-specific optimization arg.
    /// Each storage node gets `multiplicity` evaluations per polynomial.
    ///
    /// # Errors
    /// In addition to [`Advz::new`], return [`VidError::Argument`] if
    /// - TEMPORARY `multiplicity` is not a power of two [github issue](https://github.com/EspressoSystems/jellyfish/issues/339)
    pub fn with_multiplicity(
        num_storage_nodes: u32,
        recovery_threshold: u32,
        multiplicity: u32,
        srs: impl Borrow<KzgSrs<E>>,
    ) -> VidResult<Self> {
        Self::with_multiplicity_internal(num_storage_nodes, recovery_threshold, multiplicity, srs)
    }
}

#[cfg(feature = "gpu-vid")]
impl<'srs, E, H> AdvzGPU<'srs, E, H>
where
    E: Pairing,
    UnivariateKzgPCS<E>: GPUCommittable<E>,
{
    /// Like [`Advz::new`] except with SRS loaded to GPU
    pub fn new(
        num_storage_nodes: u32,
        recovery_threshold: u32,
        srs: impl Borrow<KzgSrs<E>>,
    ) -> VidResult<Self> {
        let mut advz = Self::new_internal(num_storage_nodes, recovery_threshold, srs)?;
        advz.init_gpu_srs()?;
        Ok(advz)
    }
    /// Like [`Advz::with_multiplicity`] except with SRS loaded to GPU
    pub fn with_multiplicity(
        num_storage_nodes: u32,
        recovery_threshold: u32,
        multiplicity: u32,
        srs: impl Borrow<KzgSrs<E>>,
    ) -> VidResult<Self> {
        let mut advz = Self::with_multiplicity_internal(
            num_storage_nodes,
            recovery_threshold,
            multiplicity,
            srs,
        )?;
        advz.init_gpu_srs()?;
        Ok(advz)
    }

    fn init_gpu_srs(&mut self) -> VidResult<()> {
        let srs_on_gpu = <UnivariateKzgPCS<E> as GPUCommittable<E>>::load_prover_param_to_gpu(
            &self.ck,
            self.ck.powers_of_g.len() - 1,
        )
        .map_err(vid)?;
        self.srs_on_gpu_and_cuda_stream = Some((srs_on_gpu, warmup_new_stream().unwrap()));
        Ok(())
    }
}

/// The [`VidScheme::Share`] type for [`Advz`].
#[derive(Derivative, Deserialize, Serialize)]
#[serde(bound = "Output<H>: Serialize + for<'a> Deserialize<'a>")]
#[derivative(
    Clone(bound = ""),
    Debug(bound = ""),
    Eq(bound = ""),
    Hash(bound = ""),
    PartialEq(bound = "")
)]
pub struct Share<E, H>
where
    E: Pairing,
    H: HasherDigest,
{
    index: u32,

    #[serde(with = "canonical")]
    evals: Vec<KzgEval<E>>,

    #[serde(with = "canonical")]
    // aggretate_proofs.len() equals self.multiplicity
    // TODO further aggregate into a single KZG proof.
    aggregate_proofs: Vec<KzgProof<E>>,

    evals_proof: KzgEvalsMerkleTreeProof<E, H>,
}

/// The [`VidScheme::Common`] type for [`Advz`].
#[derive(CanonicalSerialize, CanonicalDeserialize, Derivative, Deserialize, Serialize)]
#[derivative(
    Clone(bound = ""),
    Debug(bound = ""),
    Eq(bound = ""),
    Hash(bound = ""),
    PartialEq(bound = "")
)]
pub struct Common<E, H>
where
    E: Pairing,
    H: HasherDigest,
{
    #[serde(with = "canonical")]
    poly_commits: Vec<KzgCommit<E>>,

    #[serde(with = "canonical")]
    all_evals_digest: KzgEvalsMerkleTreeNode<E, H>,

    payload_byte_len: u32,
    num_storage_nodes: u32,
    multiplicity: u32,
}

/// A helper trait that cover API that maybe instantiated using GPU code
/// in specialized implementation for concrete types
pub trait MaybeGPU<E: Pairing> {
    /// kzg batch commit
    /// TODO: (alex) it's unfortnate that we are forced to use &mut self which
    /// propagate out to `VidScheme::commit_only/disperse(&mut self)`
    /// This should be fixed once ICICLE improve their `HostOrDeviceSlice`, and
    /// we can update our `GPUCommittable::commit_on_gpu()` input type.
    /// depends on <https://github.com/ingonyama-zk/icicle/pull/412>
    fn kzg_batch_commit(
        &mut self,
        polys: &[DensePolynomial<E::ScalarField>],
    ) -> VidResult<Vec<KzgCommit<E>>>;
}

impl<E, H> MaybeGPU<E> for Advz<E, H>
where
    E: Pairing,
{
    fn kzg_batch_commit(
        &mut self,
        polys: &[DensePolynomial<E::ScalarField>],
    ) -> VidResult<Vec<KzgCommit<E>>> {
        UnivariateKzgPCS::batch_commit(&self.ck, polys).map_err(vid)
    }
}

#[cfg(feature = "gpu-vid")]
impl<'srs, E, H> MaybeGPU<E> for AdvzGPU<'srs, E, H>
where
    E: Pairing,
    UnivariateKzgPCS<E>: GPUCommittable<E>,
{
    fn kzg_batch_commit(
        &mut self,
        polys: &[DensePolynomial<E::ScalarField>],
    ) -> VidResult<Vec<KzgCommit<E>>> {
        // let mut srs_on_gpu = self.srs_on_gpu_and_cuda_stream.as_mut().unwrap().0;
        // let stream = &self.srs_on_gpu_and_cuda_stream.as_ref().unwrap().1;
        let (srs_on_gpu, stream) = self.srs_on_gpu_and_cuda_stream.as_mut().unwrap(); // safe by construction
        <UnivariateKzgPCS<E> as GPUCommittable<E>>::gpu_batch_commit_with_loaded_prover_param(
            srs_on_gpu, polys, stream,
        )
        .map_err(vid)
    }
}

impl<E, H, T> VidScheme for AdvzInternal<E, H, T>
where
    E: Pairing,
    H: HasherDigest,
    AdvzInternal<E, H, T>: MaybeGPU<E>,
{
    // use HasherNode<H> instead of Output<H> to easily meet trait bounds
    type Commit = HasherNode<H>;

    type Share = Share<E, H>;
    type Common = Common<E, H>;

    fn commit_only<B>(&mut self, payload: B) -> VidResult<Self::Commit>
    where
        B: AsRef<[u8]>,
    {
        let payload = payload.as_ref();
        let bytes_to_polys_time = start_timer!(|| "encode payload bytes into polynomials");
        let polys = self.bytes_to_polys(payload);
        end_timer!(bytes_to_polys_time);

        let poly_commits_time = start_timer!(|| "batch poly commit");
        let poly_commits = <Self as MaybeGPU<E>>::kzg_batch_commit(self, &polys)?;
        end_timer!(poly_commits_time);

        Self::derive_commit(&poly_commits, payload.len(), self.num_storage_nodes)
    }

    fn disperse<B>(&mut self, payload: B) -> VidResult<VidDisperse<Self>>
    where
        B: AsRef<[u8]>,
    {
        let payload = payload.as_ref();
        let payload_byte_len = payload.len().try_into().map_err(vid)?;
        let disperse_time = start_timer!(|| format!(
            "VID disperse {} payload bytes to {} nodes",
            payload_byte_len, self.num_storage_nodes
        ));
        let _chunk_size = self.multiplicity * self.recovery_threshold;
        let code_word_size = self.multiplicity * self.num_storage_nodes;

        // partition payload into polynomial coefficients
        let bytes_to_polys_time = start_timer!(|| "encode payload bytes into polynomials");
        let polys = self.bytes_to_polys(payload);
        end_timer!(bytes_to_polys_time);

        // evaluate polynomials
        let all_storage_node_evals_timer = start_timer!(|| format!(
            "compute all storage node evals for {} polynomials with {} coefficients",
            polys.len(),
            _chunk_size
        ));
        let all_storage_node_evals = self.evaluate_polys(&polys)?;
        end_timer!(all_storage_node_evals_timer);

        // vector commitment to polynomial evaluations
        let all_evals_commit_timer =
            start_timer!(|| "compute merkle root of all storage node evals");
        let all_evals_commit =
            KzgEvalsMerkleTree::<E, H>::from_elems(None, &all_storage_node_evals).map_err(vid)?;
        end_timer!(all_evals_commit_timer);

        let common_timer = start_timer!(|| format!("compute {} KZG commitments", polys.len()));
        let common = Common {
            poly_commits: <Self as MaybeGPU<E>>::kzg_batch_commit(self, &polys)?,
            all_evals_digest: all_evals_commit.commitment().digest(),
            payload_byte_len,
            num_storage_nodes: self.num_storage_nodes,
            multiplicity: self.multiplicity,
        };
        end_timer!(common_timer);

        let commit = Self::derive_commit(
            &common.poly_commits,
            payload_byte_len,
            self.num_storage_nodes,
        )?;
        let pseudorandom_scalar = Self::pseudorandom_scalar(&common, &commit)?;

        // Compute aggregate polynomial as a pseudorandom linear combo of polynomial via
        // evaluation of the polynomial whose coefficients are polynomials and whose
        // input point is the pseudorandom scalar.
        let aggregate_poly =
            polynomial_eval(polys.iter().map(PolynomialMultiplier), pseudorandom_scalar);

        let agg_proofs_timer = start_timer!(|| format!(
            "compute aggregate proofs for {} storage nodes",
            self.num_storage_nodes
        ));
        let aggregate_proofs = UnivariateKzgPCS::multi_open_rou_proofs(
            &self.ck,
            &aggregate_poly,
            code_word_size as usize,
            &self.multi_open_domain,
        )
        .map_err(vid)?;
        end_timer!(agg_proofs_timer);

        let assemblage_timer = start_timer!(|| "assemble shares for dispersal");
        let shares =
            self.assemble_shares(all_storage_node_evals, aggregate_proofs, all_evals_commit)?;
        end_timer!(assemblage_timer);
        end_timer!(disperse_time);

        Ok(VidDisperse {
            shares,
            common,
            commit,
        })
    }

    fn verify_share(
        &self,
        share: &Self::Share,
        common: &Self::Common,
        commit: &Self::Commit,
    ) -> VidResult<Result<(), ()>> {
        // check arguments
        let multiplicity: usize = common.multiplicity.try_into().map_err(vid)?;
        if share.evals.len() / multiplicity != common.poly_commits.len() {
            return Err(VidError::Argument(format!(
                "(share eval, common poly commit) lengths differ ({},{})",
                share.evals.len() / multiplicity,
                common.poly_commits.len()
            )));
        }

        if common.num_storage_nodes != self.num_storage_nodes {
            return Err(VidError::Argument(format!(
                "common num_storage_nodes differs from self ({},{})",
                common.num_storage_nodes, self.num_storage_nodes
            )));
        }

        let polys_len = common.poly_commits.len();

        if share.index >= self.num_storage_nodes {
            return Ok(Err(())); // not an arg error
        }

        Self::is_consistent(commit, common)?;

        // verify eval proof
        // TODO: check all indices that represents the shares
        if KzgEvalsMerkleTree::<E, H>::verify(
            common.all_evals_digest,
            &KzgEvalsMerkleTreeIndex::<E, H>::from(share.index as u64),
            &share.evals_proof,
        )
        .map_err(vid)?
        .is_err()
        {
            return Ok(Err(()));
        }

        let pseudorandom_scalar = Self::pseudorandom_scalar(common, commit)?;

        // Compute aggregate polynomial [commitment|evaluation]
        // as a pseudorandom linear combo of [commitments|evaluations]
        // via evaluation of the polynomial whose coefficients are
        // [commitments|evaluations] and whose input point is the pseudorandom
        // scalar.
        let aggregate_poly_commit = KzgCommit::<E>::from(
            polynomial_eval(
                common
                    .poly_commits
                    .iter()
                    .map(|x| CurveMultiplier(x.as_ref())),
                pseudorandom_scalar,
            )
            .into(),
        );

        // verify aggregate proof
        (0..self.multiplicity as usize)
            .map(|i| {
                let aggregate_eval = polynomial_eval(
                    share.evals[i * polys_len..(i + 1) * polys_len]
                        .iter()
                        .map(FieldMultiplier),
                    pseudorandom_scalar,
                );
                Ok(UnivariateKzgPCS::verify(
                    &self.vk,
                    &aggregate_poly_commit,
                    &self
                        .multi_open_domain
                        .element((share.index as usize * multiplicity) + i),
                    &aggregate_eval,
                    &share.aggregate_proofs[i],
                )
                .map_err(vid)?
                .then_some(())
                .ok_or(()))
            })
            .collect()
    }

    fn recover_payload(&self, shares: &[Self::Share], common: &Self::Common) -> VidResult<Vec<u8>> {
        if shares.len() < self.recovery_threshold as usize {
            return Err(VidError::Argument(format!(
                "not enough shares {}, expected at least {}",
                shares.len(),
                self.recovery_threshold
            )));
        }
        if common.num_storage_nodes != self.num_storage_nodes {
            return Err(VidError::Argument(format!(
                "common num_storage_nodes differs from self ({},{})",
                common.num_storage_nodes, self.num_storage_nodes
            )));
        }

        // all shares must have equal evals len
        let num_evals = shares
            .first()
            .ok_or_else(|| VidError::Argument("shares is empty".into()))?
            .evals
            .len();
        if let Some((index, share)) = shares
            .iter()
            .enumerate()
            .find(|(_, s)| s.evals.len() != num_evals)
        {
            return Err(VidError::Argument(format!(
                "shares do not have equal evals lengths: share {} len {}, share {} len {}",
                0,
                num_evals,
                index,
                share.evals.len()
            )));
        }
        if num_evals != self.multiplicity as usize * common.poly_commits.len() {
            return Err(VidError::Argument(format!(
                "num_evals should be (multiplicity * poly_commits): {} but is instead: {}",
                self.multiplicity as usize * common.poly_commits.len(),
                num_evals,
            )));
        }
        let chunk_size = self.multiplicity * self.recovery_threshold;
        let num_polys = num_evals / self.multiplicity as usize;

        let elems_capacity = num_polys * chunk_size as usize;
        let mut elems = Vec::with_capacity(elems_capacity);

        let mut evals = Vec::with_capacity(num_evals);
        for p in 0..num_polys {
            for share in shares {
                // extract all evaluations for polynomial p from the share
                for m in 0..self.multiplicity as usize {
                    evals.push((
                        (share.index * self.multiplicity) as usize + m,
                        share.evals[(m * num_polys) + p],
                    ))
                }
            }
            let mut coeffs = reed_solomon_erasure_decode_rou(
                mem::take(&mut evals),
                chunk_size as usize,
                &self.multi_open_domain,
            )
            .map_err(vid)?;

            // TODO TEMPORARY: use FFT to encode polynomials in eval form
            // Remove these FFTs after we get KZG in eval form
            // https://github.com/EspressoSystems/jellyfish/issues/339
            self.eval_domain.fft_in_place(&mut coeffs);

            elems.append(&mut coeffs);
        }
        assert_eq!(elems.len(), elems_capacity);

        let mut payload: Vec<_> = field_to_bytes(elems).collect();
        payload.truncate(common.payload_byte_len.try_into().map_err(vid)?);
        Ok(payload)
    }

    fn is_consistent(commit: &Self::Commit, common: &Self::Common) -> VidResult<()> {
        if *commit
            != Advz::<E, H>::derive_commit(
                &common.poly_commits,
                common.payload_byte_len,
                common.num_storage_nodes,
            )?
        {
            return Err(VidError::Argument(
                "common inconsistent with commit".to_string(),
            ));
        }
        Ok(())
    }

    fn get_payload_byte_len(common: &Self::Common) -> u32 {
        common.payload_byte_len
    }

    fn get_num_storage_nodes(common: &Self::Common) -> u32 {
        common.num_storage_nodes
    }

    fn get_multiplicity(common: &Self::Common) -> u32 {
        common.multiplicity
    }
}

impl<E, H, SrsRef> AdvzInternal<E, H, SrsRef>
where
    E: Pairing,
    H: HasherDigest,
    AdvzInternal<E, H, SrsRef>: MaybeGPU<E>,
{
    fn evaluate_polys(
        &self,
        polys: &[DensePolynomial<<E as Pairing>::ScalarField>],
    ) -> Result<Vec<Vec<<E as Pairing>::ScalarField>>, VidError>
    where
        E: Pairing,
        H: HasherDigest,
    {
        let code_word_size = (self.num_storage_nodes * self.multiplicity) as usize;
        let mut all_storage_node_evals = vec![Vec::with_capacity(polys.len()); code_word_size];
        // this is to avoid `SrsRef` not implementing `Sync` problem,
        // instead of sending entire `self` cross thread, we only send a ref which is
        // Sync
        let multi_open_domain_ref = &self.multi_open_domain;

        let all_poly_evals = parallelizable_slice_iter(polys)
            .map(|poly| {
                UnivariateKzgPCS::<E>::multi_open_rou_evals(
                    poly,
                    code_word_size,
                    multi_open_domain_ref,
                )
                .map_err(vid)
            })
            .collect::<Result<Vec<_>, VidError>>()?;

        for poly_evals in all_poly_evals {
            for (storage_node_evals, poly_eval) in all_storage_node_evals
                .iter_mut()
                .zip(poly_evals.into_iter())
            {
                storage_node_evals.push(poly_eval);
            }
        }

        // sanity checks
        assert_eq!(all_storage_node_evals.len(), code_word_size);
        for storage_node_evals in all_storage_node_evals.iter() {
            assert_eq!(storage_node_evals.len(), polys.len());
        }

        Ok(all_storage_node_evals)
    }

    fn pseudorandom_scalar(
        common: &<Self as VidScheme>::Common,
        commit: &<Self as VidScheme>::Commit,
    ) -> VidResult<KzgEval<E>> {
        let mut hasher = H::new();
        commit.serialize_uncompressed(&mut hasher).map_err(vid)?;
        common
            .all_evals_digest
            .serialize_uncompressed(&mut hasher)
            .map_err(vid)?;

        // Notes on hash-to-field:
        // - Can't use `Field::from_random_bytes` because it's fallible. (In what sense
        //   is it from "random" bytes?!). This despite the docs explicitly say: "This
        //   function is primarily intended for sampling random field elements from a
        //   hash-function or RNG output."
        // - We could use `ark_ff::fields::field_hashers::HashToField` but that forces
        //   us to add additional trait bounds `Clone + Default + DynDigest` everywhere.
        //   Also, `HashToField` does not expose an incremental API (ie. `update`) so we
        //   would need to use an ordinary hasher and pipe `hasher.finalize()` through
        //   `hash_to_field`. (Ugh!)
        // - We don't need the resulting field element to be cryptographically
        //   indistinguishable from uniformly random. We only need it to be
        //   unpredictable. So it suffices to use
        Ok(PrimeField::from_le_bytes_mod_order(&hasher.finalize()))
    }

    fn bytes_to_polys(&self, payload: &[u8]) -> Vec<DensePolynomial<<E as Pairing>::ScalarField>>
    where
        E: Pairing,
    {
        let chunk_size = (self.recovery_threshold * self.multiplicity) as usize;
        let elem_bytes_len = bytes_to_field::elem_byte_capacity::<<E as Pairing>::ScalarField>();
        let eval_domain_ref = &self.eval_domain;

        parallelizable_chunks(payload, chunk_size * elem_bytes_len)
            .map(|chunk| {
                Self::polynomial_internal(
                    eval_domain_ref,
                    chunk_size,
                    bytes_to_field::<_, KzgEval<E>>(chunk),
                )
            })
            .collect()
    }

    // This is an associated function, not a method, doesn't take in `self`, thus
    // more friendly to cross-thread `Sync`, especially when on of the generic
    // param of `Self` didn't implement `Sync`
    fn polynomial_internal<I>(
        domain_ref: &Radix2EvaluationDomain<KzgPoint<E>>,
        chunk_size: usize,
        coeffs: I,
    ) -> KzgPolynomial<E>
    where
        I: Iterator,
        I::Item: Borrow<KzgEval<E>>,
    {
        // TODO TEMPORARY: use FFT to encode polynomials in eval form
        // Remove these FFTs after we get KZG in eval form
        // https://github.com/EspressoSystems/jellyfish/issues/339
        let mut coeffs_vec: Vec<_> = coeffs.map(|c| *c.borrow()).collect();
        let pre_fft_len = coeffs_vec.len();
        EvaluationDomain::ifft_in_place(domain_ref, &mut coeffs_vec);

        // sanity check: the fft did not resize coeffs.
        // If pre_fft_len != self.recovery_threshold * self.multiplicity
        // then we were not given the correct number of coeffs. In that case
        // coeffs.len() could be anything, so there's nothing to sanity check.
        if pre_fft_len == chunk_size {
            assert_eq!(coeffs_vec.len(), pre_fft_len);
        }

        DenseUVPolynomial::from_coefficients_vec(coeffs_vec)
    }

    fn polynomial<I>(&self, coeffs: I) -> KzgPolynomial<E>
    where
        I: Iterator,
        I::Item: Borrow<KzgEval<E>>,
    {
        Self::polynomial_internal(
            &self.eval_domain,
            (self.recovery_threshold * self.multiplicity) as usize,
            coeffs,
        )
    }

    /// Derive a commitment from whatever data is needed.
    ///
    /// Generic types `T`, `U` allow caller to pass `usize` or anything else.
    /// Yes, Rust really wants these horrible trait bounds on
    /// `<T as TryInto<u32>>::Error`.
    fn derive_commit<T, U>(
        poly_commits: &[KzgCommit<E>],
        payload_byte_len: T,
        num_storage_nodes: U,
    ) -> VidResult<<Self as VidScheme>::Commit>
    where
        T: TryInto<u32>,
        <T as TryInto<u32>>::Error: ark_std::fmt::Display + Debug + Send + Sync + 'static,
        U: TryInto<u32>,
        <U as TryInto<u32>>::Error: ark_std::fmt::Display + Debug + Send + Sync + 'static,
    {
        let payload_byte_len: u32 = payload_byte_len.try_into().map_err(vid)?;
        let num_storage_nodes: u32 = num_storage_nodes.try_into().map_err(vid)?;
        let mut hasher = H::new();
        payload_byte_len
            .serialize_uncompressed(&mut hasher)
            .map_err(vid)?;
        num_storage_nodes
            .serialize_uncompressed(&mut hasher)
            .map_err(vid)?;
        for poly_commit in poly_commits {
            poly_commit
                .serialize_uncompressed(&mut hasher)
                .map_err(vid)?;
        }
        Ok(hasher.finalize().into())
    }

    /// Assemble shares from evaluations and proofs.
    ///
    /// Each share contains (for multiplicity m):
    /// 1. (m * num_poly) evaluations.
    /// 2. a collection of m KZG proofs. TODO KZG aggregation https://github.com/EspressoSystems/jellyfish/issues/356
    /// 3. a merkle tree membership proof.
    fn assemble_shares(
        &self,
        all_storage_node_evals: Vec<Vec<<E as Pairing>::ScalarField>>,
        aggregate_proofs: Vec<UnivariateKzgProof<E>>,
        all_evals_commit: KzgEvalsMerkleTree<E, H>,
    ) -> Result<Vec<Share<E, H>>, VidError>
    where
        E: Pairing,
        H: HasherDigest,
    {
        let code_word_size = (self.num_storage_nodes * self.multiplicity) as usize;
        let num_of_polys = all_storage_node_evals[0].len();
        let mut shares = Vec::with_capacity(self.num_storage_nodes as usize);
        let mut evals = Vec::with_capacity(num_of_polys * self.multiplicity as usize);
        let mut proofs = Vec::with_capacity(self.multiplicity as usize);
        let mut index = 0;
        for i in 0..code_word_size {
            evals.extend(all_storage_node_evals[i].iter());
            proofs.push(aggregate_proofs[i].clone());
            if (i + 1) % self.multiplicity as usize == 0 {
                shares.push(Share {
                    index,
                    evals: mem::take(&mut evals),
                    aggregate_proofs: mem::take(&mut proofs),
                    evals_proof: all_evals_commit // TODO: check MT lookup for each index
                        .lookup(KzgEvalsMerkleTreeIndex::<E, H>::from(index as u64))
                        .expect_ok()
                        .map_err(vid)?
                        .1,
                });
                index += 1;
            }
        }
        Ok(shares)
    }
}

/// Evaluate a generalized polynomial at a given point using Horner's method.
///
/// Coefficients can be anything that can be multiplied by a point
/// and such that the result of such multiplications can be added.
fn polynomial_eval<U, F, I>(coeffs: I, point: impl Borrow<F>) -> U
where
    I: IntoIterator,
    I::Item: for<'a> Mul<&'a F, Output = U>,
    U: Add<Output = U> + Zero,
{
    coeffs
        .into_iter()
        .fold(U::zero(), |res, coeff| coeff * point.borrow() + res)
}

struct FieldMultiplier<'a, F>(&'a F);

/// Arkworks does not provide (&F,&F) multiplication
impl<F> Mul<&F> for FieldMultiplier<'_, F>
where
    F: Field,
{
    type Output = F;

    fn mul(self, rhs: &F) -> Self::Output {
        *self.0 * rhs
    }
}

/// Arkworks does not provide (&C,&F) multiplication
struct CurveMultiplier<'a, C>(&'a C);

impl<C, F> Mul<&F> for CurveMultiplier<'_, C>
where
    C: AffineRepr<ScalarField = F>,
{
    type Output = C::Group;

    fn mul(self, rhs: &F) -> Self::Output {
        *self.0 * rhs
    }
}

/// Arkworks does not provide (&P,&F) multiplication
struct PolynomialMultiplier<'a, P>(&'a P);

impl<P, F> Mul<&F> for PolynomialMultiplier<'_, P>
where
    P: DenseUVPolynomial<F>,
    F: Field,
{
    type Output = P;

    fn mul(self, rhs: &F) -> Self::Output {
        // `Polynomial` does not impl `Mul` by scalar
        // so we need to multiply each coeff by `rhs`
        P::from_coefficients_vec(self.0.coeffs().iter().map(|coeff| *coeff * rhs).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{VidError::Argument, *};

    use crate::pcs::{checked_fft_size, prelude::UnivariateUniversalParams};
    use ark_bls12_381::Bls12_381;
    use ark_bn254::Bn254;
    use ark_std::{
        rand::{CryptoRng, RngCore},
        vec,
    };
    use sha2::Sha256;

    #[ignore]
    #[test]
    fn disperse_timer() {
        // run with 'print-trace' feature to see timer output
        let (recovery_threshold, num_storage_nodes) = (256, 512);
        let mut rng = jf_utils::test_rng();
        let srs = init_srs(recovery_threshold as usize, &mut rng);
        #[cfg(feature = "gpu-vid")]
        let mut advz_gpu =
            AdvzGPU::<'_, Bn254, Sha256>::new(num_storage_nodes, recovery_threshold, &srs).unwrap();
        let mut advz =
            Advz::<Bn254, Sha256>::new(num_storage_nodes, recovery_threshold, srs).unwrap();

        let payload_random = init_random_payload(1 << 25, &mut rng);

        #[cfg(feature = "gpu-vid")]
        let _ = advz_gpu.disperse(payload_random.clone());
        let _ = advz.disperse(payload_random);
    }

    #[ignore]
    #[test]
    fn commit_only_timer() {
        // run with 'print-trace' feature to see timer output
        let (recovery_threshold, num_storage_nodes) = (256, 512);
        let mut rng = jf_utils::test_rng();
        let srs = init_srs(recovery_threshold as usize, &mut rng);
        #[cfg(feature = "gpu-vid")]
        let mut advz_gpu =
            AdvzGPU::<'_, Bn254, Sha256>::new(num_storage_nodes, recovery_threshold, &srs).unwrap();
        let mut advz =
            Advz::<Bn254, Sha256>::new(num_storage_nodes, recovery_threshold, srs).unwrap();

        let payload_random = init_random_payload(1 << 25, &mut rng);

        #[cfg(feature = "gpu-vid")]
        let _ = advz_gpu.commit_only(payload_random.clone());

        let _ = advz.commit_only(payload_random);
    }

    #[test]
    fn sad_path_verify_share_corrupt_share() {
        let (mut advz, bytes_random) = advz_init();
        let disperse = advz.disperse(bytes_random).unwrap();
        let (shares, common, commit) = (disperse.shares, disperse.common, disperse.commit);

        for (i, share) in shares.iter().enumerate() {
            // missing share eval
            {
                let share_missing_eval = Share {
                    evals: share.evals[1..].to_vec(),
                    ..share.clone()
                };
                assert_arg_err(
                    advz.verify_share(&share_missing_eval, &common, &commit),
                    "1 missing share should be arg error",
                );
            }

            // corrupted share eval
            {
                let mut share_bad_eval = share.clone();
                share_bad_eval.evals[0].double_in_place();
                advz.verify_share(&share_bad_eval, &common, &commit)
                    .unwrap()
                    .expect_err("bad share value should fail verification");
            }

            // corrupted index, in bounds
            {
                let share_bad_index = Share {
                    index: (share.index + 1) % advz.num_storage_nodes,
                    ..share.clone()
                };
                advz.verify_share(&share_bad_index, &common, &commit)
                    .unwrap()
                    .expect_err("bad share index should fail verification");
            }

            // corrupted index, out of bounds
            {
                let share_bad_index = Share {
                    index: share.index + advz.num_storage_nodes,
                    ..share.clone()
                };
                advz.verify_share(&share_bad_index, &common, &commit)
                    .unwrap()
                    .expect_err("bad share index should fail verification");
            }

            // corrupt eval proof
            {
                // We have no way to corrupt a proof
                // (without also causing a deserialization failure).
                // So we use another share's proof instead.
                let share_bad_evals_proof = Share {
                    evals_proof: shares[(i + 1) % shares.len()].evals_proof.clone(),
                    ..share.clone()
                };
                advz.verify_share(&share_bad_evals_proof, &common, &commit)
                    .unwrap()
                    .expect_err("bad share evals proof should fail verification");
            }
        }
    }

    #[test]
    fn sad_path_verify_share_corrupt_commit() {
        let (mut advz, bytes_random) = advz_init();
        let disperse = advz.disperse(bytes_random).unwrap();
        let (shares, common, commit) = (disperse.shares, disperse.common, disperse.commit);

        // missing commit
        let common_missing_item = Common {
            poly_commits: common.poly_commits[1..].to_vec(),
            ..common.clone()
        };
        assert_arg_err(
            advz.verify_share(&shares[0], &common_missing_item, &commit),
            "1 missing commit should be arg error",
        );

        // 1 corrupt commit, poly_commit
        let common_1_poly_corruption = {
            let mut corrupted = common.clone();
            corrupted.poly_commits[0] = <Bls12_381 as Pairing>::G1Affine::zero().into();
            corrupted
        };
        assert_arg_err(
            advz.verify_share(&shares[0], &common_1_poly_corruption, &commit),
            "corrupted commit should be arg error",
        );

        // 1 corrupt commit, all_evals_digest
        let common_1_digest_corruption = {
            let mut corrupted = common;
            let mut digest_bytes = vec![0u8; corrupted.all_evals_digest.uncompressed_size()];
            corrupted
                .all_evals_digest
                .serialize_uncompressed(&mut digest_bytes)
                .expect("digest serialization should succeed");
            digest_bytes[0] += 1;
            corrupted.all_evals_digest =
                HasherNode::deserialize_uncompressed(digest_bytes.as_slice())
                    .expect("digest deserialization should succeed");
            corrupted
        };
        advz.verify_share(&shares[0], &common_1_digest_corruption, &commit)
            .unwrap()
            .expect_err("1 corrupt all_evals_digest should fail verification");
    }

    #[test]
    fn sad_path_verify_share_corrupt_share_and_commit() {
        let (mut advz, bytes_random) = advz_init();
        let disperse = advz.disperse(bytes_random).unwrap();
        let (mut shares, mut common, commit) = (disperse.shares, disperse.common, disperse.commit);

        common.poly_commits.pop();
        shares[0].evals.pop();

        // equal nonzero lengths for common, share
        assert_arg_err(
            advz.verify_share(&shares[0], &common, &commit),
            "common inconsistent with commit should be arg error",
        );

        common.poly_commits.clear();
        shares[0].evals.clear();

        // zero length for common, share
        assert_arg_err(
            advz.verify_share(&shares[0], &common, &commit),
            "expect arg error for common inconsistent with commit",
        );
    }

    #[test]
    fn sad_path_recover_payload_corrupt_shares() {
        let (mut advz, bytes_random) = advz_init();
        let disperse = advz.disperse(&bytes_random).unwrap();
        let (shares, common) = (disperse.shares, disperse.common);

        {
            // unequal share eval lengths
            let mut shares_missing_evals = shares.clone();
            for i in 0..shares_missing_evals.len() - 1 {
                shares_missing_evals[i].evals.pop();
                assert_arg_err(
                    advz.recover_payload(&shares_missing_evals, &common),
                    format!("{} shares missing 1 eval should be arg error", i + 1).as_str(),
                );
            }

            // 1 eval missing from all shares
            shares_missing_evals.last_mut().unwrap().evals.pop();
            assert_arg_err(
                advz.recover_payload(&shares_missing_evals, &common),
                format!(
                    "shares contain {} but expected {}",
                    shares_missing_evals[0].evals.len(),
                    &common.poly_commits.len()
                )
                .as_str(),
            );
        }

        // corrupted index, in bounds
        {
            let mut shares_bad_indices = shares.clone();

            // permute indices to avoid duplicates and keep them in bounds
            for share in &mut shares_bad_indices {
                share.index = (share.index + 1) % advz.num_storage_nodes;
            }

            let bytes_recovered = advz
                .recover_payload(&shares_bad_indices, &common)
                .expect("recover_payload should succeed when indices are in bounds");
            assert_ne!(bytes_recovered, bytes_random);
        }

        // corrupted index, out of bounds
        {
            let mut shares_bad_indices = shares.clone();
            for i in 0..shares_bad_indices.len() {
                shares_bad_indices[i].index +=
                    u32::try_from(advz.multi_open_domain.size()).unwrap();
                advz.recover_payload(&shares_bad_indices, &common)
                    .expect_err("recover_payload should fail when indices are out of bounds");
            }
        }
    }

    /// Routine initialization tasks.
    ///
    /// Returns the following tuple:
    /// 1. An initialized [`Advz`] instance.
    /// 2. A `Vec<u8>` filled with random bytes.
    pub(super) fn advz_init() -> (Advz<Bls12_381, Sha256>, Vec<u8>) {
        let (recovery_threshold, num_storage_nodes) = (4, 6);
        let mut rng = jf_utils::test_rng();
        let srs = init_srs(recovery_threshold as usize, &mut rng);
        let advz = Advz::new(num_storage_nodes, recovery_threshold, srs).unwrap();
        let bytes_random = init_random_payload(4000, &mut rng);
        (advz, bytes_random)
    }

    /// Convenience wrapper to assert [`VidError::Argument`] return value.
    pub(super) fn assert_arg_err<T>(res: VidResult<T>, msg: &str) {
        assert!(matches!(res, Err(Argument(_))), "{}", msg);
    }

    pub(super) fn init_random_payload<R>(len: usize, rng: &mut R) -> Vec<u8>
    where
        R: RngCore + CryptoRng,
    {
        let mut bytes_random = vec![0u8; len];
        rng.fill_bytes(&mut bytes_random);
        bytes_random
    }

    pub(super) fn init_srs<E, R>(num_coeffs: usize, rng: &mut R) -> UnivariateUniversalParams<E>
    where
        E: Pairing,
        R: RngCore + CryptoRng,
    {
        UnivariateKzgPCS::gen_srs_for_testing(rng, checked_fft_size(num_coeffs - 1).unwrap())
            .unwrap()
    }
}
