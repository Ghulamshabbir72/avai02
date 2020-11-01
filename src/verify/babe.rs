// Substrate-lite
// Copyright (C) 2019-2020  Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! BABE consensus.
//!
//! BABE, or Blind Assignment for Blockchain Extension, is the consensus algorithm used by
//! Polkadot in order to determine who is authorized to generate a block.
//!
//! Every block (with the exception of the genesis block) must contain, in its header, some data
//! that makes it possible to verify that it has been generated by a legitimate author.
//!
//! References:
//!
//! - https://research.web3.foundation/en/latest/polkadot/BABE/Babe.html
//!
//! # Overview of BABE
//!
//! In the BABE algorithm, time is divided into non-overlapping **epochs**, themselves divided
//! into **slots**. How long an epoch and a slot are is determined by calling the
//! `BabeApi_configuration` runtime entry point.
//!
//! > **Note**: As example values, in the Polkadot genesis, a slot lasts for 6 seconds and an
//! >           epoch consists of 2400 slots (in other words, four hours).
//!
//! Every block that is produced must belong to a specific slot. This slot number can be found in
//! the block header, with the exception of the genesis block which is considered timeless and
//! doesn't have any slot number.
//!
//! At the moment, the current slot number is determined purely based on the slot duration (e.g.
//! 6 seconds for Polkadot) and the local clock based on the UNIX EPOCH. The current slot
//! number is `unix_timestamp / duration_per_slot`. This might change in the future.
//!
//! The first epoch (epoch number 0) starts at `slot_number(block #1)` and ends at
//! `slot_number(block #1) + slots_per_epoch`. The second epoch (epoch #1) starts at slot
//! `end_of_epoch_1 + 1`. All epochs end at `start_of_new_epoch + slots_per_epoch`. Block #0
//! doesn't belong to any epoch.
//!
//! The header of first block produced after a transition to a new epoch (including block #1) must
//! contain a log entry indicating the public keys that are allowed to sign blocks, alongside with
//! a weight for each of them, and a "randomness value". This information does not concern the
//! newly-started epoch, but the one immediately after. In other words, the first block of epoch
//! `N` contains the information about epoch `N+1`.
//!
//! > **Note**: The way the list of authorities and their weights is determined is at the
//! >           discretion of the runtime code and is out of scope of this module, but it normally
//! >           corresponds to the list of validators and how much stake is available to them.
//!
//! In order to produce a block, one must generate, using a
//! [VRF (Verifiable Random Function)](https://en.wikipedia.org/wiki/Verifiable_random_function),
//! and based on the slot number, genesis hash, and aformentioned "randomness value",
//! a number whose value is lower than a certain threshold.
//!
//! The number that has been generated must be included in the header of the authored block,
//! alongside with the proof of the correct generation that can be verified using one of the
//! public keys allowed to generate blocks in that epoch. The weight associated to that public key
//! determines the allowed threshold.
//!
//! The "randomess value" of an epoch `N` is calculated by combining the generated numbers of all
//! the blocks of the epoch `N - 2`.
//!
//! ## Secondary slots
//!
//! While all slots can be claimed by generating a number below a certain threshold, each slot is
//! additionally assigned to a specific public key amongst the ones allowed. The owner of a
//! public key is always allowed to generate a block during the slot assigned to it.
//!
//! The mechanism of attributing each slot to a public key is called "secondary slot claims",
//! while the mechanism of generating a number below a certain threshold is called a "primary
//! slot claim". As their name indicates, primary slot claims have a higher priority over
//! secondary slot claims.
//!
//! Secondary slot claims are a way to guarantee that all slots can potentially lead to a block
//! being produced.
//!
//! ## Chain selection
//!
//! The "best" block of a chain in the BABE algorithm is the one with the highest slot number.
//! If there exists multiple blocks on the same slot, the best block is one with the highest number
//! of primary slot claims. In other words, if two blocks have the same parent, but one is a
//! primary slot claim and the other is a secondary slot claim, we prefer the one with the primary
//! slot claim.
//!
//! Keep in mind that there can still be draws in terms of primary slot claims count, in which
//! case the winning block is the one upon which the next block author builds upon.
//!
//! ## Epochs 0 and 1
//!
//! The information about an epoch `N` is provided by the first block of the epoch `N-1`.
//!
//! Because of this, we need to special-case epoch 0. The information about epoch 0 is contained
//! in the chain-wide BABE configuration found in the runtime. The first block of epoch 0 is the
//! block number #1. The information about epoch 1 is therefore contained in block #1.
//!
//! # Usage
//!
//! Before any block can be verified, to need to create a [`BabeGenesisConfiguration`] using the
//! genesis block. See the documentation of [`BabeGenesisConfiguration`] for more information.
//!
//! Verifying a BABE block is done in two phases:
//!
//! - First, call [`start_verify_header`] to start the verification process. This returns a
//! [`SuccessOrPending`] enum.
//! - If [`SuccessOrPending::Pending`] has been returned, you need to provide a specific
//! [`header::BabeNextEpoch`] struct.
//!
//! The information to keep in memory is:
//!
//! - The slot number of block 1. When block number 1 is verified, one needs to keep in memory
//! slot number that is returned with [`VerifySuccess::slot_number`]. This value must later be
//! provided as part of the [`VerifyConfig`].
//! - The [`header::BabeNextEpoch`] structs corresponding to each epoch number. An [`header::BabeNextEpoch`]
//! can be extracted from a block's header, therefore for long-term storage you only need to store
//! which block contains the information about each epoch number, and that block's header.
//!
//! In both situations, you need to be aware of forks. There can be multiple block 1s, and
//! multiple blocks which contain an [`header::BabeNextEpoch`] for a given epoch number. Only the
//! information contained in an ancestor of the block being verified must be provided.
//!

use crate::{chain::chain_information::babe::BabeGenesisConfiguration, header};

use core::{convert::TryFrom as _, num::NonZeroU64, time::Duration};
use num_traits::{cast::ToPrimitive as _, identities::One as _};

/// Configuration for [`start_verify_header`].
pub struct VerifyConfig<'a> {
    /// Header of the block to verify.
    pub header: header::HeaderRef<'a>,

    /// Time elapsed since [the Unix Epoch](https://en.wikipedia.org/wiki/Unix_time) (i.e.
    /// 00:00:00 UTC on 1 January 1970), ignoring leap seconds.
    // TODO: unused, should check against a block's slot
    pub now_from_unix_epoch: Duration,

    /// Header of the parent of the block to verify.
    ///
    /// [`start_verify_header`] assumes that this block has been successfully verified before.
    ///
    /// The hash of this header must be the one referenced in [`VerifyConfig::header`].
    pub parent_block_header: header::HeaderRef<'a>,

    /// BABE configuration retrieved from the genesis block.
    ///
    /// Can be obtained by calling [`BabeGenesisConfiguration::from_virtual_machine_prototype`]
    /// with the runtime of the genesis block.
    pub genesis_configuration: &'a BabeGenesisConfiguration,

    /// Slot number of block #1. **Must** be provided, unless the block being verified is block
    /// #1 itself.
    pub block1_slot_number: Option<u64>,
}

/// Information yielded back after successfully verifying a block.
#[derive(Debug)]
pub struct VerifySuccess {
    /// Slot number the block belongs to.
    // TODO: the info is already in the header, maybe replace this field with a `header::BabePreDigestRef`?
    pub slot_number: u64,

    /// Epoch number the block belongs to.
    pub epoch_number: u64,

    /// If `Some`, the verified block contains an epoch transition describing the given epoch.
    /// This epoch transition must later be provided back when calling [`PendingVerify::finish`]
    /// when verifying blocks that are part of that epoch.
    ///
    /// > **Note**: If `Some`, the value is always equal to [`VerifySuccess::epoch_number`] + 1.
    pub epoch_transition_target: Option<NonZeroU64>,
}

/// Failure to verify a block.
#[derive(Debug, derive_more::Display)]
pub enum VerifyError {
    /// The seal (containing the signature of the authority) is missing from the header.
    MissingSeal,
    /// No pre-runtime digest in the block header.
    MissingPreRuntimeDigest,
    /// Parent block doesn't contain any Babe information.
    ParentIsntBabeConsensus,
    /// Slot number must be strictly increasing between a parent and its child.
    SlotNumberNotIncreasing,
    /// Block contains an epoch change digest log, but no epoch change is to be performed.
    UnexpectedEpochChangeLog,
    /// Block is the first block after a new epoch, but it is missing an epoch change digest log.
    MissingEpochChangeLog,
    /// Authority index stored within block is out of range.
    InvalidAuthorityIndex,
    /// Block header signature is invalid.
    BadSignature,
    /// VRF proof in the block header is invalid.
    BadVrfProof,
    /// Block is a secondary slot claim and its author is not the expected author.
    BadSecondarySlotAuthor,
    /// VRF output is over threshold required to claim the primary slot.
    OverPrimaryClaimThreshold,
    /// Type of slot claim forbidden by current configuration.
    ForbiddenSlotType,
}

/// Verifies whether a block header provides a correct proof of the legitimacy of the authorship.
///
/// Returns either a [`PendingVerify`] if more information is needed, or a [`VerifySuccess`] if
/// the verification could be successfully performed.
///
/// # Panic
///
/// Panics if `config.parent_block_header` is invalid.
/// Panics if `config.block1_slot_number` is `None` and `config.header.number` is not 1.
///
pub fn start_verify_header<'a>(config: VerifyConfig<'a>) -> Result<SuccessOrPending, VerifyError> {
    // TODO: handle OnDisabled

    // Gather the BABE-related information from the header.
    let (authority_index, slot_number, primary, vrf) = match config.header.digest.babe_pre_runtime()
    {
        Some(header::BabePreDigestRef::Primary(digest)) => (
            digest.authority_index,
            digest.slot_number,
            true,
            Some((*digest.vrf_output, *digest.vrf_proof)),
        ),
        Some(header::BabePreDigestRef::SecondaryPlain(digest)) => {
            (digest.authority_index, digest.slot_number, false, None)
        }
        Some(header::BabePreDigestRef::SecondaryVRF(digest)) => (
            digest.authority_index,
            digest.slot_number,
            false,
            Some((*digest.vrf_output, *digest.vrf_proof)),
        ),
        None => return Err(VerifyError::MissingPreRuntimeDigest),
    };

    // Determine the epoch number the block we verify belongs to.
    let epoch_number = match (slot_number, config.block1_slot_number) {
        (curr, Some(block1)) => {
            slot_number_to_epoch(curr, config.genesis_configuration, block1).unwrap()
        } // TODO: don't unwrap
        (_, None) if config.header.number == 1 => 0,
        (_, None) => panic!(), // Bad `VerifyConfig`.
    };

    // Determine the epoch number of the parent block. `None` if the parent is the genesis block.
    let parent_epoch_number = if config.parent_block_header.number != 0 {
        let parent_slot_number = match config.parent_block_header.digest.babe_pre_runtime() {
            Some(pr) => pr.slot_number(),
            None => return Err(VerifyError::ParentIsntBabeConsensus),
        };

        if slot_number <= parent_slot_number {
            return Err(VerifyError::SlotNumberNotIncreasing);
        }

        Some(
            slot_number_to_epoch(
                parent_slot_number,
                config.genesis_configuration,
                config.block1_slot_number.unwrap(),
            )
            .unwrap(),
        )
    } else {
        None
    };

    // Extract the epoch change information stored in the header, if any.
    let epoch_transition_target = config
        .header
        .digest
        .babe_epoch_information()
        .map(|_| NonZeroU64::new(epoch_number + 1).unwrap());

    // TODO: in case of epoch change, should also check the randomness value; while the runtime
    //       checks that the randomness value is correct, light clients in particular do not
    //       execute the runtime

    // Make sure that the expected epoch transitions correspond to what the blocks report.
    match (
        &epoch_transition_target,
        Some(epoch_number) != parent_epoch_number,
    ) {
        (Some(_), true) => {}
        (None, false) => {}
        (Some(_), false) => return Err(VerifyError::UnexpectedEpochChangeLog),
        (None, true) => return Err(VerifyError::MissingEpochChangeLog),
    };

    let seal_signature = match config.header.digest.babe_seal() {
        Some(seal) => {
            schnorrkel::Signature::from_bytes(seal).map_err(|_| VerifyError::BadSignature)?
        }
        None => return Err(VerifyError::MissingSeal),
    };

    // The signature in the seal applies to the header from where the signature isn't present.
    // Build the hash that is expected to be signed.
    let pre_seal_hash = {
        let mut unsealed_header = config.header;
        let _popped = unsealed_header.digest.pop_seal();
        debug_assert!(matches!(_popped, Some(header::Seal::Babe(_))));
        unsealed_header.hash()
    };

    // Intermediary object representing the state of the verification at this point.
    let pending = PendingVerify {
        pre_seal_hash,
        seal_signature,
        epoch_transition_target,
        epoch_number,
        slot_number,
        authority_index,
        primary_slot_claim: primary,
        vrf_output_and_proof: vrf,
    };

    // The information about epoch number 0 is never given by any block and is instead found in
    // the BABE genesis configuration.
    Ok(if epoch_number == 0 {
        SuccessOrPending::Success(pending.finish((
            From::from(&config.genesis_configuration.epoch0_information),
            config.genesis_configuration.epoch0_configuration,
        ))?)
    } else {
        SuccessOrPending::Pending(pending)
    })
}

/// Verification in progress. The block is **not** fully verified yet. You must call
/// [`PendingVerify::finish`] in order to finish the verification.
#[must_use]
pub enum SuccessOrPending {
    /// Need information about an epoch in order to finish verifying the block.
    Pending(PendingVerify),
    /// Block has been successfully verified.
    Success(VerifySuccess),
}

/// Verification in progress. The block is **not** fully verified yet. You must call
/// [`PendingVerify::finish`] in order to finish the verification.
#[must_use]
pub struct PendingVerify {
    /// Hash of the block header without its seal.
    pre_seal_hash: [u8; 32],
    /// Block signature contained in the header that we verify.
    seal_signature: schnorrkel::Signature,
    /// If `Some`, block is at an epoch transition.
    /// This can only happen for blocks that are the first block of an epoch.
    epoch_transition_target: Option<NonZeroU64>,
    /// Epoch number the block belongs to.
    epoch_number: u64,
    /// Slot number the block belongs to.
    slot_number: u64,
    /// Index of the authority that has signed the block, according to the block header.
    authority_index: u32,
    /// If true, the slot claim is a primary claim. If false, a secondary claim.
    primary_slot_claim: bool,
    /// VRF output and proof contained in the block header. Cannot be `None` if
    /// `primary_slot_claim` is true.
    vrf_output_and_proof: Option<([u8; 32], [u8; 64])>,
}

impl PendingVerify {
    /// Returns the epoch number whose information must be passed to [`PendingVerify::finish`].
    pub fn epoch_number(&self) -> u64 {
        self.epoch_number
    }

    /// Returns true if the epoch of the verified block is the same as its parent's.
    pub fn same_epoch_as_parent(&self) -> bool {
        self.epoch_transition_target.is_none()
    }

    /// Finishes the verification. Must provide the information about the epoch whose number is
    /// obtained with [`PendingVerify::epoch_number`].
    pub fn finish(
        self,
        epoch_info: (header::BabeNextEpochRef, header::BabeNextConfig),
    ) -> Result<VerifySuccess, VerifyError> {
        // Check that the claim is one of the allowed slot types.
        match (
            epoch_info.1.allowed_slots,
            self.primary_slot_claim,
            self.vrf_output_and_proof,
        ) {
            (_, true, None) => unreachable!(),
            (_, true, Some(_)) => {}
            (header::BabeAllowedSlots::PrimaryAndSecondaryPlainSlots, false, None) => {}
            (header::BabeAllowedSlots::PrimaryAndSecondaryVRFSlots, false, Some(_)) => {}
            _ => return Err(VerifyError::ForbiddenSlotType),
        }

        // Fetch the authority that has supposedly signed the block.
        let signing_authority = epoch_info
            .0
            .authorities
            .clone()
            .nth(
                usize::try_from(self.authority_index)
                    .map_err(|_| VerifyError::InvalidAuthorityIndex)?,
            )
            .ok_or(VerifyError::InvalidAuthorityIndex)?;

        // This `unwrap()` can only panic if `public_key` is the wrong length, which we know can't
        // happen as it's of type `[u8; 32]`.
        let signing_public_key =
            schnorrkel::PublicKey::from_bytes(signing_authority.public_key).unwrap();

        // Now verifying the signature in the seal.
        signing_public_key
            .verify_simple(b"substrate", &self.pre_seal_hash, &self.seal_signature)
            .map_err(|_| VerifyError::BadSignature)?;

        // Now verify the VRF output and proof, if any.
        // The lack of VRF output/proof in the header is checked when we check whether the slot
        // type is allowed by the current configuration.
        if let Some((vrf_output, vrf_proof)) = self.vrf_output_and_proof {
            // In order to verify the VRF output, we first need to create a transcript containing all
            // the data to verify the VRF against.
            let transcript = {
                let mut transcript = merlin::Transcript::new(&b"BABE"[..]);
                transcript.append_u64(b"slot number", self.slot_number);
                transcript.append_u64(b"current epoch", self.epoch_number);
                transcript.append_message(b"chain randomness", &epoch_info.0.randomness[..]);
                transcript
            };

            // These `unwrap()`s can only panic if `vrf_output` or `vrf_proof` are of the wrong
            // length, which we know can't happen as they're of types `[u8; 32]` and `[u8; 64]`.
            let vrf_output = schnorrkel::vrf::VRFPreOut::from_bytes(&vrf_output[..]).unwrap();
            let vrf_proof = schnorrkel::vrf::VRFProof::from_bytes(&vrf_proof[..]).unwrap();

            let (vrf_in_out, _) = signing_public_key
                .vrf_verify(transcript, &vrf_output, &vrf_proof)
                .map_err(|_| VerifyError::BadVrfProof)?;

            // If this is a primary slot claim, we need to make sure that the VRF output is below
            // a certain threshold, otherwise all the authorities could claim all the slots.
            if self.primary_slot_claim {
                let threshold = calculate_primary_threshold(
                    epoch_info.1.c,
                    epoch_info.0.authorities.clone().map(|a| a.weight),
                    signing_authority.weight,
                );
                if u128::from_le_bytes(vrf_in_out.make_bytes::<[u8; 16]>(b"substrate-babe-vrf"))
                    >= threshold
                {
                    return Err(VerifyError::OverPrimaryClaimThreshold);
                }
            }
        } else {
            debug_assert!(!self.primary_slot_claim);
        }

        // Each slot can be claimed by one specific authority in what is called a secondary slot
        // claim. If the block is a secondary slot claim, we need to make sure that the author
        // is indeed the one that is expected.
        if !self.primary_slot_claim {
            // Expected author is determined based on `blake2(randomness | slot_number)`.
            let hash = {
                let mut hash = blake2_rfc::blake2b::Blake2b::new(32);
                hash.update(epoch_info.0.randomness);
                hash.update(&self.slot_number.to_le_bytes());
                hash.finalize()
            };

            // The expected authority index is `hash % num_authorities`.
            let expected_authority_index = {
                let hash = num_bigint::BigUint::from_bytes_be(hash.as_bytes());
                let authorities_len = num_bigint::BigUint::from(epoch_info.0.authorities.len());
                debug_assert_ne!(epoch_info.0.authorities.len(), 0);
                hash % authorities_len
            };

            if u32::try_from(expected_authority_index).map_or(true, |v| v != self.authority_index) {
                return Err(VerifyError::BadSecondarySlotAuthor);
            }
        }

        // Success! 🚀
        Ok(VerifySuccess {
            epoch_transition_target: self.epoch_transition_target,
            slot_number: self.slot_number,
            epoch_number: self.epoch_number,
        })
    }
}

/// Turns a slot number into an epoch number.
///
/// Returns an error if `slot_number` is inferior to `block1_slot_number`.
fn slot_number_to_epoch(
    slot_number: u64,
    genesis_config: &BabeGenesisConfiguration,
    block1_slot_number: u64,
) -> Result<u64, ()> {
    let slots_diff = slot_number.checked_sub(block1_slot_number).ok_or(())?;
    Ok(slots_diff / genesis_config.slots_per_epoch.get())
}

/// Calculates the primary selection threshold for a given authority, taking
/// into account `c` (`1 - c` represents the probability of a slot being empty).
///
/// The value of `c` can be found in the current Babe configuration.
///
/// `authorities_weights` must be the list of all weights of all autorities.
/// `authority_weight` must be the weight of the authority whose threshold to calculate.
///
/// # Panic
///
/// Panics if `authorities_weights` is empty.
/// Panics if `authority_weight` is 0.
///
fn calculate_primary_threshold(
    c: (u64, u64),
    authorities_weights: impl ExactSizeIterator<Item = u64>,
    authority_weight: u64, // TODO: use a NonZeroU64 once crate::header also has weights that use NonZeroU64
) -> u128 {
    assert!(authorities_weights.len() != 0);

    let c = c.0 as f64 / c.1 as f64;
    assert!(c.is_finite());

    let theta = authority_weight as f64 / authorities_weights.sum::<u64>() as f64;
    assert!(theta > 0.0);

    // The calculations below has been copy-pasted from Substrate and is guaranteed to not panic.
    let p = num_rational::BigRational::from_float(1f64 - (1f64 - c).powf(theta)).unwrap();
    let numer = p.numer().to_biguint().unwrap();
    let denom = p.denom().to_biguint().unwrap();
    ((num_bigint::BigUint::one() << 128u32) * numer / denom)
        .to_u128()
        .unwrap()
}
