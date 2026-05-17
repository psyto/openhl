//! Single-validator runner — drives one Malachite consensus round end-to-end.
//!
//! Pedagogical entry point for Module 1 L5 + L11: shows the propose →
//! prevote → precommit → decide loop without an actor framework, by feeding
//! `Driver` outputs back as inputs and bridging async `ConsensusBridge`
//! calls into the otherwise-sync state machine.
//!
//! For multi-validator runs use `malachitebft-engine` (next stage).

use informalsystems_malachitebft_core_driver::{Driver, Input, Output, ThresholdParams};
use informalsystems_malachitebft_core_types::{
    Height as _, Proposal as _, Round, SignedMessage, Validity, Value as _, VotingPower,
};
use informalsystems_malachitebft_signing_ed25519::{PrivateKey, Signature};
use openhl_types::{BlockHash, PayloadAttrs};
use rand::rngs::OsRng;
use thiserror::Error;

use crate::bridge::{BridgeError, ConsensusBridge};
use crate::context::OpenHlContext;
use crate::types::{OpenHlAddress, OpenHlHeight, OpenHlValidator, OpenHlValidatorSet, OpenHlValue};

#[derive(Debug, Error)]
pub enum RunError {
    #[error("driver: {0}")]
    Driver(String),

    #[error("bridge: {0}")]
    Bridge(#[from] BridgeError),

    #[error("driver halted without producing a decision")]
    Stuck,
}

/// Drive one consensus round to a decision using a single validator (ourselves).
///
/// Returns the `BlockHash` the round decided on, after committing via the bridge.
/// Useful for devnet bootstrap, integration tests, and as the simplest possible
/// "consensus actually closes a loop" demonstration.
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

    // Bootstrap: enter round 0 with ourselves as proposer.
    let mut outputs = driver
        .process(Input::NewRound(height, Round::new(0), address))
        .map_err(|e| RunError::Driver(format!("{e:?}")))?;

    // Drive the round to completion. Each loop iteration converts the current
    // batch of outputs into the next batch of inputs and feeds them back.
    loop {
        let mut next: Vec<Input<OpenHlContext>> = Vec::new();

        for output in outputs.drain(..) {
            match output {
                Output::GetValue(_h, r, _timeout) => {
                    let id = bridge
                        .build_payload(parent, PayloadAttrs {
                            timestamp: 0,
                            fee_recipient: [0u8; 20],
                            prev_randao: [0u8; 32],
                        })
                        .await?;
                    let block = bridge.payload_ready(id).await?;
                    next.push(Input::ProposeValue(r, OpenHlValue(block.hash)));
                }
                Output::Propose(proposal) => {
                    // Self-sign with a placeholder signature — single-validator
                    // means no other node verifies it. Real signing arrives
                    // when the engine takes over (next stage).
                    let signed = SignedMessage::new(proposal, Signature::test());
                    next.push(Input::Proposal(signed, Validity::Valid));
                }
                Output::Vote(vote) => {
                    let signed = SignedMessage::new(vote, Signature::test());
                    next.push(Input::Vote(signed));
                }
                Output::Decide(_round, proposal) => {
                    let hash = proposal.value().id();
                    bridge.commit(hash).await?;
                    return Ok(hash);
                }
                Output::NewRound(h, r) => {
                    // Driver advanced to a new round (shouldn't happen on the
                    // happy path with a single validator, but handle it).
                    next.push(Input::NewRound(h, r, address));
                }
                Output::ScheduleTimeout(_) => {
                    // Single validator: timeouts never fire in the happy path.
                }
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

/// Derive an Ethereum-style 20-byte address from an Ed25519 public key.
/// Last 20 bytes of the SHA-256(public-key) — a deterministic, version-stable
/// derivation that doesn't claim to be EIP-55 compliant.
fn address_from_public_key(pk: &informalsystems_malachitebft_signing_ed25519::PublicKey) -> [u8; 20] {
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
            parent: BlockHash,
            _attrs: PayloadAttrs,
        ) -> Result<PayloadId, BridgeError> {
            let mut h = [0u8; 32];
            h[..20].copy_from_slice(&parent.0[..20]);
            h[31] = 0x42;
            // Store the synthesized hash in the id so payload_ready returns it.
            // Use a stable u64 by hashing first 8 bytes of h.
            Ok(PayloadId(u64::from_le_bytes([
                h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7],
            ])))
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
        let parent = BlockHash([0u8; 32]);
        let decided = run_single_validator(&bridge, parent).await.unwrap();
        assert_eq!(decided, BlockHash([0x42u8; 32]));
        let committed = bridge.committed.lock().unwrap();
        assert_eq!(*committed, Some(decided));
    }
}
