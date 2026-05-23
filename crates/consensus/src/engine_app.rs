//! Engine app loop — consumes `AppMsg` from the Malachite engine and routes
//! every consensus-relevant event through a [`ConsensusBridge`].
//!
//! This is the missing half of Stage 6c: with `OpenHlNode::start()` spinning
//! up the actor system, this loop is what makes those actors do useful work.
//! Once a `Decided` arrives we commit through the bridge, increment height,
//! and (optionally) stop after N decisions for tests.

use std::sync::Arc;

use eyre::eyre;
use informalsystems_malachitebft_app::engine::host::Next;
use informalsystems_malachitebft_app_channel::{AppMsg, Channels};
use informalsystems_malachitebft_core_types::Height as _;
use openhl_types::{BlockHash, PayloadAttrs};

use crate::bridge::ConsensusBridge;
use crate::context::OpenHlContext;
use crate::types::{OpenHlHeight, OpenHlValidatorSet, OpenHlValue};

const APP_REPLY_WAIT_LOG: &str = "engine_app: peer replied unsuccessfully (channel closed)";

/// Drive the engine app loop until `stop_after_decisions` decisions have been
/// committed through the bridge, or the consensus channel closes.
///
/// Returns the `BlockHash`es that were decided, in order. Single-validator mode
/// uses this with `stop_after_decisions = 1` to exit after the first block.
///
/// `initial_parent` is the `BlockHash` of the block this engine should
/// build on top of for its first decision. For a fresh chain, this is
/// the execution-layer's genesis hash — `bin/openhl reth-devnet` queries
/// it from `ChainSpec::genesis_hash()` (Stage 13d). For a chain restart,
/// callers pass the last decided hash from prior consensus state. Stub
/// bridges that don't validate parent hashes (e.g., in unit tests) can
/// pass `BlockHash([0u8; 32])` and the engine will happily build on the
/// zero hash.
#[allow(clippy::too_many_lines)] // 12 AppMsg arms — laid out flat for lesson L11's match-by-match walk
pub async fn run_engine_app<B>(
    bridge: Arc<B>,
    mut channels: Channels<OpenHlContext>,
    validator_set: OpenHlValidatorSet,
    initial_parent: BlockHash,
    stop_after_decisions: usize,
) -> eyre::Result<Vec<BlockHash>>
where
    B: ConsensusBridge + 'static,
{
    let mut decided: Vec<BlockHash> = Vec::new();
    let mut current_parent = initial_parent;
    let mut current_height = OpenHlHeight::INITIAL;

    while let Some(msg) = channels.consensus.recv().await {
        match msg {
            AppMsg::ConsensusReady { reply, .. } => {
                if reply
                    .send((current_height, validator_set.clone()))
                    .is_err()
                {
                    tracing::warn!("{APP_REPLY_WAIT_LOG} (ConsensusReady)");
                }
            }

            AppMsg::StartedRound {
                height,
                round: _,
                reply_value,
                ..
            } => {
                current_height = height;
                if reply_value.send(Vec::new()).is_err() {
                    tracing::warn!("{APP_REPLY_WAIT_LOG} (StartedRound)");
                }
            }

            AppMsg::GetValue {
                height,
                round,
                timeout: _,
                reply,
            } => {
                let attrs = default_attrs();
                let id = bridge.build_payload(current_parent, attrs).await?;
                let block = bridge.payload_ready(id).await?;
                let value = OpenHlValue(block.hash);
                let lpv =
                    informalsystems_malachitebft_app_channel::app::types::LocallyProposedValue::new(
                        height, round, value,
                    );
                if reply.send(lpv).is_err() {
                    tracing::warn!("{APP_REPLY_WAIT_LOG} (GetValue)");
                }
            }

            AppMsg::ExtendVote { reply, .. } => {
                if reply.send(None).is_err() {
                    tracing::warn!("{APP_REPLY_WAIT_LOG} (ExtendVote)");
                }
            }

            AppMsg::VerifyVoteExtension { reply, .. } => {
                if reply.send(Ok(())).is_err() {
                    tracing::warn!("{APP_REPLY_WAIT_LOG} (VerifyVoteExtension)");
                }
            }

            AppMsg::RestreamProposal { .. } => {
                // Single-validator mode never re-streams.
            }

            AppMsg::GetHistoryMinHeight { reply } => {
                if reply.send(OpenHlHeight::INITIAL).is_err() {
                    tracing::warn!("{APP_REPLY_WAIT_LOG} (GetHistoryMinHeight)");
                }
            }

            AppMsg::ReceivedProposalPart { reply, .. } => {
                // ProposalOnly value-payload mode — proposal parts never arrive.
                if reply.send(None).is_err() {
                    tracing::warn!("{APP_REPLY_WAIT_LOG} (ReceivedProposalPart)");
                }
            }

            AppMsg::GetValidatorSet { reply, .. } => {
                if reply.send(Some(validator_set.clone())).is_err() {
                    tracing::warn!("{APP_REPLY_WAIT_LOG} (GetValidatorSet)");
                }
            }

            AppMsg::Decided {
                certificate, reply, ..
            } => {
                let hash = certificate.value_id;
                bridge.commit(hash).await?;
                decided.push(hash);
                current_parent = hash;

                if decided.len() >= stop_after_decisions {
                    // Send a reply so consensus doesn't hang waiting on us before
                    // we drop the channel.
                    let next_height = certificate.height.increment();
                    let _ = reply.send(Next::Start(next_height, validator_set.clone()));
                    return Ok(decided);
                }

                let next_height = certificate.height.increment();
                current_height = next_height;
                if reply
                    .send(Next::Start(next_height, validator_set.clone()))
                    .is_err()
                {
                    tracing::warn!("{APP_REPLY_WAIT_LOG} (Decided)");
                }
            }

            AppMsg::GetDecidedValue { reply, .. } => {
                if reply.send(None).is_err() {
                    tracing::warn!("{APP_REPLY_WAIT_LOG} (GetDecidedValue)");
                }
            }

            AppMsg::ProcessSyncedValue { reply, .. } => {
                if reply.send(None).is_err() {
                    tracing::warn!("{APP_REPLY_WAIT_LOG} (ProcessSyncedValue)");
                }
            }
        }
    }

    Err(eyre!(
        "consensus channel closed after {n} decisions (wanted {stop_after_decisions})",
        n = decided.len()
    ))
}

fn default_attrs() -> PayloadAttrs {
    PayloadAttrs {
        timestamp: 0,
        fee_recipient: [0u8; 20],
        prev_randao: [0u8; 32],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::BridgeError;
    use crate::node::OpenHlNode;
    use crate::types::{OpenHlAddress, OpenHlValidator};
    use async_trait::async_trait;
    use informalsystems_malachitebft_app::node::{Node as _, NodeHandle as _};
    use informalsystems_malachitebft_signing_ed25519::PrivateKey;
    use openhl_types::{ExecutedBlock, PayloadId, PayloadStatus};
    use rand::rngs::OsRng;
    use sha2::{Digest, Sha256};
    use std::sync::Mutex;
    use std::time::Duration;

    #[derive(Debug, Default)]
    struct StubBridge {
        last_built: Mutex<Option<BlockHash>>,
        committed: Mutex<Vec<BlockHash>>,
    }

    #[async_trait]
    impl ConsensusBridge for StubBridge {
        async fn build_payload(
            &self,
            _parent: BlockHash,
            _attrs: PayloadAttrs,
        ) -> Result<PayloadId, BridgeError> {
            let hash = BlockHash([0x42u8; 32]);
            *self.last_built.lock().expect("poisoned") = Some(hash);
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
            })
        }

        async fn validate_payload(
            &self,
            _block: &ExecutedBlock,
        ) -> Result<PayloadStatus, BridgeError> {
            Ok(PayloadStatus::Valid)
        }

        async fn commit(&self, block_hash: BlockHash) -> Result<(), BridgeError> {
            self.committed.lock().expect("poisoned").push(block_hash);
            Ok(())
        }
    }

    fn make_test_node(home_dir: std::path::PathBuf) -> OpenHlNode {
        let sk = PrivateKey::generate(OsRng);
        let pk = sk.public_key();
        let digest = Sha256::digest(pk.as_bytes());
        let mut addr_bytes = [0u8; 20];
        addr_bytes.copy_from_slice(&digest[12..32]);
        let address = OpenHlAddress(addr_bytes);
        let validator_set = OpenHlValidatorSet::new(vec![OpenHlValidator::new(address, pk, 1)]);
        OpenHlNode::new(sk, validator_set, home_dir, "openhl-engine-test")
    }

    /// End-to-end: spawn the engine actor system, drive one block through the
    /// `AppMsg` loop, assert the bridge built+committed exactly the hash the
    /// engine decided on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn first_block_via_engine_actors() {
        let tmp = tempfile::tempdir().unwrap();
        let node = make_test_node(tmp.path().to_path_buf());
        let validator_set = node.validator_set.clone();

        let handle = node.start().await.expect("start_engine failed");
        let channels = handle
            .take_channels()
            .await
            .expect("channels available exactly once");

        let bridge = Arc::new(StubBridge::default());
        let bridge_for_check = bridge.clone();

        let app_task = tokio::spawn(run_engine_app(
            bridge,
            channels,
            validator_set,
            BlockHash([0u8; 32]),
            1,
        ));

        let decisions = tokio::time::timeout(Duration::from_secs(15), app_task)
            .await
            .expect("app loop timed out")
            .expect("app task panicked")
            .expect("app loop returned error");

        assert_eq!(decisions.len(), 1, "expected exactly one decided block");
        let decided_hash = decisions[0];

        let committed = bridge_for_check.committed.lock().unwrap().clone();
        assert_eq!(committed, vec![decided_hash], "bridge must commit decided hash");
        assert_eq!(
            *bridge_for_check.last_built.lock().unwrap(),
            Some(decided_hash),
            "decided hash must match what we built",
        );

        handle.kill(None).await.unwrap();
    }
}

