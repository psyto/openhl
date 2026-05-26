//! Synchronous runners that drive Malachite's `Driver` to a decision.
//!
//! [`run_single_validator`] is the simplest possible "consensus closes a loop"
//! demo — a single node, no peers, placeholder signatures.
//!
//! [`run_multi_validator`] simulates a full N-validator round: we drive our
//! own Driver and synthesize honest votes from N-1 stub validators (all keys
//! known to us). Real Ed25519 signatures via [`crate::signing`], so the
//! Driver could verify any of them.
//!
//! Both are pedagogical scaffolds for Module 1 L5+L11; the actor-based
//! [`malachitebft-engine`] integration replaces them in the next stage.

use std::collections::HashMap;

use informalsystems_malachitebft_core_driver::{Driver, Input, Output, ThresholdParams};
use informalsystems_malachitebft_core_types::{
    Context as _, Height as _, NilOrVal, Proposal as _, Round, SignedMessage, SigningProvider as _,
    Validity, Value as _, VotingPower,
};
use informalsystems_malachitebft_signing_ed25519::{PrivateKey, Signature};
use openhl_types::{BlockHash, PayloadAttrs};
use rand::rngs::OsRng;
use thiserror::Error;

use crate::bridge::{BridgeError, ConsensusBridge};
use crate::context::OpenHlContext;
use crate::signing::sign_vote;
use crate::signing_provider::OpenHlSigningProvider;
use crate::types::{
    OpenHlAddress, OpenHlHeight, OpenHlValidator, OpenHlValidatorSet, OpenHlValue, OpenHlVote,
};

#[derive(Debug, Error)]
pub enum RunError {
    #[error("driver: {0}")]
    Driver(String),

    #[error("bridge: {0}")]
    Bridge(#[from] BridgeError),

    #[error("driver halted without producing a decision")]
    Stuck,

    #[error("invalid validator count: {0}")]
    InvalidValidatorCount(usize),
}

/// Drive one consensus round to a decision with a single validator (ourselves).
///
/// Returns the decided `BlockHash` after committing via the bridge.
pub async fn run_single_validator<B>(
    bridge: &B,
    parent: BlockHash,
) -> Result<BlockHash, RunError>
where
    B: ConsensusBridge,
{
    let private = PrivateKey::generate(OsRng);
    let public = private.public_key();
    let address = OpenHlAddress(address_from_public_key(&public));

    let validator_set = OpenHlValidatorSet::new(vec![OpenHlValidator::new(
        address, public, 1 as VotingPower,
    )]);

    let height = OpenHlHeight::INITIAL;
    let mut driver = Driver::new(
        OpenHlContext,
        height,
        validator_set,
        address,
        ThresholdParams::default(),
    );

    let mut outputs = driver
        .process(Input::NewRound(height, Round::new(0), address))
        .map_err(|e| RunError::Driver(format!("{e:?}")))?;

    loop {
        let mut next: Vec<Input<OpenHlContext>> = Vec::new();

        for output in outputs.drain(..) {
            match output {
                Output::GetValue(_h, r, _timeout) => {
                    let id = bridge
                        .build_payload(parent, default_attrs())
                        .await?;
                    let block = bridge.payload_ready(id).await?;
                    next.push(Input::ProposeValue(r, OpenHlValue(block.hash)));
                }
                Output::Propose(proposal) => {
                    next.push(Input::Proposal(
                        SignedMessage::new(proposal, Signature::test()),
                        Validity::Valid,
                    ));
                }
                Output::Vote(vote) => {
                    next.push(Input::Vote(SignedMessage::new(vote, Signature::test())));
                }
                Output::Decide(_round, proposal) => {
                    let hash = proposal.value().id();
                    bridge.commit(hash).await?;
                    return Ok(hash);
                }
                Output::NewRound(h, r) => {
                    next.push(Input::NewRound(h, r, address));
                }
                Output::ScheduleTimeout(_) => {}
            }
        }

        if next.is_empty() {
            return Err(RunError::Stuck);
        }

        outputs.clear();
        for input in next {
            let batch = driver
                .process(input)
                .map_err(|e| RunError::Driver(format!("{e:?}")))?;
            outputs.extend(batch);
        }
    }
}

/// Drive one consensus round with N validators (us + N-1 honest stubs).
///
/// All N keypairs are generated locally so we can sign votes/proposals as
/// any of them — this is the cheapest way to exercise the 3f+1 quorum math
/// without spinning up real peers. Signatures are real Ed25519 over a
/// canonical encoding, so the Driver could verify any of them.
///
/// For `n = 1` this degenerates to the single-validator case.
pub async fn run_multi_validator<B>(
    bridge: &B,
    parent: BlockHash,
    n: usize,
) -> Result<BlockHash, RunError>
where
    B: ConsensusBridge,
{
    if n == 0 {
        return Err(RunError::InvalidValidatorCount(n));
    }

    // Generate N validators; give index 0 (us) higher voting power so we land
    // first in the canonical sort and don't have to map back to a randomised
    // position.
    let keys: Vec<PrivateKey> = (0..n).map(|_| PrivateKey::generate(OsRng)).collect();
    let mut entries: Vec<(OpenHlAddress, PrivateKey, OpenHlValidator)> = keys
        .into_iter()
        .enumerate()
        .map(|(i, sk)| {
            let pk = sk.public_key();
            let addr = OpenHlAddress(address_from_public_key(&pk));
            let power: VotingPower = if i == 0 { 2 } else { 1 };
            let validator = OpenHlValidator::new(addr, pk, power);
            (addr, sk, validator)
        })
        .collect();

    let our_address = entries[0].0;
    let our_provider = OpenHlSigningProvider::new(entries[0].1.clone());
    let validator_set = OpenHlValidatorSet::new(entries.iter().map(|(_, _, v)| v.clone()).collect());

    let signers: HashMap<OpenHlAddress, PrivateKey> = entries
        .drain(..)
        .map(|(addr, sk, _)| (addr, sk))
        .collect();

    // Pre-build the value once — every validator agrees on the same one.
    let height = OpenHlHeight::INITIAL;
    let id = bridge.build_payload(parent, default_attrs()).await?;
    let block = bridge.payload_ready(id).await?;
    let value = OpenHlValue(block.hash);
    let value_id_for_votes = NilOrVal::Val(block.hash);

    // Determine proposer for round 0.
    let proposer_address = OpenHlContext
        .select_proposer(&validator_set, height, Round::new(0))
        .address;
    let we_are_proposer = proposer_address == our_address;

    let mut driver = Driver::new(
        OpenHlContext,
        height,
        validator_set.clone(),
        our_address,
        ThresholdParams::default(),
    );

    let mut outputs = driver
        .process(Input::NewRound(height, Round::new(0), proposer_address))
        .map_err(|e| RunError::Driver(format!("{e:?}")))?;

    if !we_are_proposer {
        let proposal = OpenHlContext.new_proposal(
            height,
            Round::new(0),
            value,
            Round::Nil,
            proposer_address,
        );
        let proposer_provider = OpenHlSigningProvider::new(
            signers
                .get(&proposer_address)
                .expect("proposer in signers")
                .clone(),
        );
        let signed = proposer_provider.sign_proposal(proposal);
        let batch = driver
            .process(Input::Proposal(signed, Validity::Valid))
            .map_err(|e| RunError::Driver(format!("{e:?}")))?;
        outputs.extend(batch);
    }

    drive_loop_multi(
        &mut driver,
        outputs,
        bridge,
        value,
        value_id_for_votes,
        our_address,
        &our_provider,
        &signers,
        &validator_set,
    )
    .await
}

#[allow(clippy::too_many_arguments)] // each argument is irreducible state for the loop
async fn drive_loop_multi<B>(
    driver: &mut Driver<OpenHlContext>,
    mut outputs: Vec<Output<OpenHlContext>>,
    bridge: &B,
    value: OpenHlValue,
    value_id_for_votes: NilOrVal<BlockHash>,
    our_address: OpenHlAddress,
    our_provider: &OpenHlSigningProvider,
    signers: &HashMap<OpenHlAddress, PrivateKey>,
    validator_set: &OpenHlValidatorSet,
) -> Result<BlockHash, RunError>
where
    B: ConsensusBridge,
{
    loop {
        let mut next: Vec<Input<OpenHlContext>> = Vec::new();

        for output in outputs.drain(..) {
            match output {
                Output::GetValue(_h, r, _timeout) => {
                    next.push(Input::ProposeValue(r, value));
                }
                Output::Propose(proposal) => {
                    let signed = our_provider.sign_proposal(proposal);
                    next.push(Input::Proposal(signed, Validity::Valid));
                }
                Output::Vote(our_vote) => {
                    let signed_us = our_provider.sign_vote(our_vote.clone());
                    next.push(Input::Vote(signed_us));

                    for (addr, sk) in signers {
                        if *addr == our_address {
                            continue;
                        }
                        let peer_vote = matching_vote_from(&our_vote, *addr, value_id_for_votes);
                        let signed_peer = sign_vote(peer_vote, sk);
                        next.push(Input::Vote(signed_peer));
                    }
                }
                Output::Decide(_round, proposal) => {
                    let hash = proposal.value().id();
                    bridge.commit(hash).await?;
                    return Ok(hash);
                }
                Output::NewRound(h, r) => {
                    let next_proposer = OpenHlContext
                        .select_proposer(validator_set, h, r)
                        .address;
                    next.push(Input::NewRound(h, r, next_proposer));
                }
                Output::ScheduleTimeout(_) => {}
            }
        }

        if next.is_empty() {
            return Err(RunError::Stuck);
        }

        outputs.clear();
        for input in next {
            let batch = driver
                .process(input)
                .map_err(|e| RunError::Driver(format!("{e:?}")))?;
            outputs.extend(batch);
        }
    }
}

/// Build a vote with the same height/round/value/type as `template`, but
/// attributed to `address`. Used to synthesize honest peer votes.
fn matching_vote_from(
    template: &OpenHlVote,
    address: OpenHlAddress,
    value_id: NilOrVal<BlockHash>,
) -> OpenHlVote {
    OpenHlVote {
        height: template.height,
        round: template.round,
        value_id: match template.value_id {
            NilOrVal::Nil => NilOrVal::Nil,
            NilOrVal::Val(_) => value_id,
        },
        vote_type: template.vote_type,
        address,
    }
}

fn default_attrs() -> PayloadAttrs {
    PayloadAttrs {
        timestamp: 0,
        fee_recipient: [0u8; 20],
        prev_randao: [0u8; 32],
    }
}

/// Derive an Ethereum-style 20-byte address from an Ed25519 public key.
/// Last 20 bytes of SHA-256(public_key). Deterministic, version-stable;
/// not EIP-55. Real chains will want something stronger.
fn address_from_public_key(
    pk: &informalsystems_malachitebft_signing_ed25519::PublicKey,
) -> [u8; 20] {
    use sha2::{Digest, Sha256};
    let bytes = pk.as_bytes();
    let digest = Sha256::digest(bytes);
    let mut addr = [0u8; 20];
    addr.copy_from_slice(&digest[12..32]);
    addr
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use openhl_types::{ExecutedBlock, PayloadId, PayloadStatus};
    use std::sync::Mutex;

    #[derive(Debug, Default)]
    struct StubBridge {
        committed: Mutex<Option<BlockHash>>,
    }

    #[async_trait]
    impl ConsensusBridge for StubBridge {
        async fn build_payload(
            &self,
            _parent: BlockHash,
            _attrs: PayloadAttrs,
        ) -> Result<PayloadId, BridgeError> {
            Ok(PayloadId(1))
        }

        async fn payload_ready(
            &self,
            _id: PayloadId,
        ) -> Result<ExecutedBlock, BridgeError> {
            Ok(ExecutedBlock {
                hash: BlockHash([0x42u8; 32]),
                parent_hash: BlockHash([0u8; 32]),
                number: 1,
                state_root: [0u8; 32],
                timestamp: 1,
            })
        }

        async fn validate_payload(
            &self,
            _block: &ExecutedBlock,
        ) -> Result<PayloadStatus, BridgeError> {
            Ok(PayloadStatus::Valid)
        }

        async fn commit(&self, block_hash: BlockHash) -> Result<(), BridgeError> {
            *self.committed.lock().expect("poisoned") = Some(block_hash);
            Ok(())
        }
    }

    #[tokio::test]
    async fn single_validator_decides_and_commits() {
        let bridge = StubBridge::default();
        let decided = run_single_validator(&bridge, BlockHash([0u8; 32]))
            .await
            .unwrap();
        assert_eq!(decided, BlockHash([0x42u8; 32]));
        assert_eq!(*bridge.committed.lock().unwrap(), Some(decided));
    }

    #[tokio::test]
    async fn four_validators_reach_quorum() {
        let bridge = StubBridge::default();
        let decided = run_multi_validator(&bridge, BlockHash([0u8; 32]), 4)
            .await
            .unwrap();
        assert_eq!(decided, BlockHash([0x42u8; 32]));
        assert_eq!(*bridge.committed.lock().unwrap(), Some(decided));
    }

    #[tokio::test]
    async fn seven_validators_reach_quorum() {
        // 7 validators tolerate 2 byzantine — exercises a non-trivial set size.
        let bridge = StubBridge::default();
        let decided = run_multi_validator(&bridge, BlockHash([0u8; 32]), 7)
            .await
            .unwrap();
        assert_eq!(decided, BlockHash([0x42u8; 32]));
    }

    #[tokio::test]
    async fn multi_validator_with_n_one_works() {
        let bridge = StubBridge::default();
        let decided = run_multi_validator(&bridge, BlockHash([0u8; 32]), 1)
            .await
            .unwrap();
        assert_eq!(decided, BlockHash([0x42u8; 32]));
    }

    #[tokio::test]
    async fn multi_validator_rejects_zero() {
        let bridge = StubBridge::default();
        let err = run_multi_validator(&bridge, BlockHash([0u8; 32]), 0)
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::InvalidValidatorCount(0)));
    }
}
