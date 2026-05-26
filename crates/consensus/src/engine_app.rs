//! Engine app loop — consumes `AppMsg` from the Malachite engine and routes
//! every consensus-relevant event through a [`ConsensusBridge`].
//!
//! This is the missing half of Stage 6c: with `OpenHlNode::start()` spinning
//! up the actor system, this loop is what makes those actors do useful work.
//! Once a `Decided` arrives we commit through the bridge, increment height,
//! and (optionally) stop after N decisions for tests.

use std::sync::Arc;
use std::collections::HashSet;

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
///
/// `initial_height` is the consensus height for the **first** decision
/// this engine produces. Fresh chains start at `OpenHlHeight::INITIAL`
/// (height 1). For a restart resuming from a prior committed chain
/// (Stage 13i), callers pass `OpenHlHeight(prior_decisions + 1)` so
/// consensus log lines and any future multi-validator peers see a
/// height that continues the prior chain instead of restarting at 1.
/// Generic per-commit hook fired by [`run_engine_app`] after each
/// successful `bridge.commit(hash)`. Stage 14a uses this to drive
/// [`openhl_node::OpenHlNode::tick`] without leaking integration-layer
/// types into the consensus crate — engine_app stays consensus-only
/// and the binary plugs in the coordinator-tick closure.
///
/// The hook receives the committed block hash and its consensus height.
/// If it returns `Err`, `run_engine_app` propagates the error.
#[allow(clippy::too_many_lines)] // 12 AppMsg arms — laid out flat for lesson L11's match-by-match walk
#[allow(clippy::too_many_arguments)] // 7 args, all load-bearing — see doc comments
pub async fn run_engine_app<B, F>(
    bridge: Arc<B>,
    mut channels: Channels<OpenHlContext>,
    validator_set: OpenHlValidatorSet,
    initial_parent: BlockHash,
    initial_height: OpenHlHeight,
    stop_after_decisions: usize,
    mut on_committed: F,
) -> eyre::Result<Vec<BlockHash>>
where
    B: ConsensusBridge + 'static,
    F: FnMut(BlockHash, OpenHlHeight) -> eyre::Result<()> + Send,
{
    let mut decided: Vec<BlockHash> = Vec::new();
    let mut current_parent = initial_parent;
    let mut current_height = initial_height;
    let history_min_height = initial_height;
    let mut locally_built_heights: HashSet<OpenHlHeight> = HashSet::new();

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
                locally_built_heights.insert(height);
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
                if reply.send(history_min_height).is_err() {
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

                // Stage 13n: follower-side bridge replication via deterministic
                // recompute.
                //
                // Important: only nodes that did NOT run `GetValue` at this
                // height should re-build. If proposer-side code re-builds here,
                // bridge side effects can run twice (for example draining
                // pending fills in LiveRethEvmBridge).
                if !locally_built_heights.remove(&certificate.height) {
                    let id = bridge.build_payload(current_parent, default_attrs()).await?;
                    let block = bridge.payload_ready(id).await?;
                    if block.hash != hash {
                        return Err(eyre!(
                            "Stage 13n: deterministic build_payload mismatch — \
                             consensus decided {hash:?} but our recompute yielded {recomputed:?}; \
                             the proposer's attrs or parent state diverged from ours",
                            hash = hash,
                            recomputed = block.hash,
                        ));
                    }
                }

                bridge.commit(hash).await?;
                on_committed(hash, certificate.height)?;
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
    use informalsystems_malachitebft_app::events::TxEvent;
    use informalsystems_malachitebft_app_channel::{AppMsg, Channels};
    use informalsystems_malachitebft_core_types::{CommitCertificate, Round, VoteExtensions};
    use informalsystems_malachitebft_signing_ed25519::PrivateKey;
    use openhl_types::{ExecutedBlock, PayloadId, PayloadStatus};
    use rand::rngs::OsRng;
    use sha2::{Digest, Sha256};
    use std::sync::{Arc as StdArc, Mutex};
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[derive(Debug, Default)]
    struct StubBridge {
        last_built: Mutex<Option<BlockHash>>,
        build_calls: Mutex<usize>,
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
            *self.build_calls.lock().expect("poisoned") += 1;
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
        // ProposalAndParts is currently the most stable mode for actor-based
        // engine integration tests in upstream Malachite.
        OpenHlNode::new(sk, validator_set, home_dir, "openhl-engine-test")
            .with_value_payload(informalsystems_malachitebft_config::ValuePayload::ProposalAndParts)
    }

    /// End-to-end: spawn the engine actor system, drive one block through the
    /// `AppMsg` loop, assert the bridge built+committed exactly the hash the
    /// engine decided on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "Diagnostic: passes outside sandbox but can timeout under restricted socket environments"]
    async fn first_block_via_engine_actors() {
        let tmp = tempfile::tempdir().unwrap();
        let node = make_test_node(tmp.path().to_path_buf());
        let validator_set = node.validator_set.clone();

        let handle = node.start().await.expect("start_engine failed");
        let channels = handle
            .take_channels()
            .await
            .expect("channels available exactly once");
        let mut event_rx = handle.subscribe();

        let observed_app_msgs: StdArc<Mutex<Vec<&'static str>>> =
            StdArc::new(Mutex::new(Vec::new()));
        let observed_events: StdArc<Mutex<Vec<String>>> = StdArc::new(Mutex::new(Vec::new()));

        let app_msgs_for_task = observed_app_msgs.clone();
        let (proxy_tx, proxy_rx) = mpsc::channel(128);
        let mut raw_consensus_rx = channels.consensus;
        tokio::spawn(async move {
            while let Some(msg) = raw_consensus_rx.recv().await {
                app_msgs_for_task
                    .lock()
                    .expect("poisoned")
                    .push(app_msg_name(&msg));
                if proxy_tx.send(msg).await.is_err() {
                    break;
                }
            }
        });

        let events_for_task = observed_events.clone();
        tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(ev) => events_for_task
                        .lock()
                        .expect("poisoned")
                        .push(ev.to_string()),
                    Err(_) => break,
                }
            }
        });

        let channels = Channels {
            consensus: proxy_rx,
            network: channels.network,
            events: channels.events,
        };

        let bridge = Arc::new(StubBridge::default());
        let bridge_for_check = bridge.clone();

        let app_task = tokio::spawn(run_engine_app(
            bridge,
            channels,
            validator_set,
            BlockHash([0u8; 32]),
            OpenHlHeight::INITIAL,
            1,
            |_hash, _height| Ok(()),
        ));

        let decisions = tokio::time::timeout(Duration::from_secs(15), app_task)
            .await
            .unwrap_or_else(|_| {
                let app = observed_app_msgs.lock().expect("poisoned").clone();
                let evs = observed_events.lock().expect("poisoned").clone();
                panic!("app loop timed out; observed AppMsgs={app:?}; observed events={evs:?}");
            })
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
        assert_eq!(
            *bridge_for_check.build_calls.lock().unwrap(),
            1,
            "GetValue path should avoid duplicate build_payload on Decided for proposer heights",
        );

        handle.kill(None).await.unwrap();
    }

    fn app_msg_name(msg: &AppMsg<OpenHlContext>) -> &'static str {
        match msg {
            AppMsg::ConsensusReady { .. } => "ConsensusReady",
            AppMsg::StartedRound { .. } => "StartedRound",
            AppMsg::GetValue { .. } => "GetValue",
            AppMsg::ExtendVote { .. } => "ExtendVote",
            AppMsg::VerifyVoteExtension { .. } => "VerifyVoteExtension",
            AppMsg::RestreamProposal { .. } => "RestreamProposal",
            AppMsg::GetHistoryMinHeight { .. } => "GetHistoryMinHeight",
            AppMsg::ReceivedProposalPart { .. } => "ReceivedProposalPart",
            AppMsg::GetValidatorSet { .. } => "GetValidatorSet",
            AppMsg::Decided { .. } => "Decided",
            AppMsg::GetDecidedValue { .. } => "GetDecidedValue",
            AppMsg::ProcessSyncedValue { .. } => "ProcessSyncedValue",
        }
    }

    #[tokio::test]
    async fn get_history_min_height_matches_initial_height() {
        let bridge = Arc::new(StubBridge::default());
        let (tx_consensus, rx_consensus) = mpsc::channel(4);
        let (tx_network, _rx_network) = mpsc::channel(4);
        let channels = Channels {
            consensus: rx_consensus,
            network: tx_network,
            events: TxEvent::new(),
        };

        let sk = PrivateKey::generate(OsRng);
        let pk = sk.public_key();
        let digest = Sha256::digest(pk.as_bytes());
        let mut addr_bytes = [0u8; 20];
        addr_bytes.copy_from_slice(&digest[12..32]);
        let address = OpenHlAddress(addr_bytes);
        let validator_set = OpenHlValidatorSet::new(vec![OpenHlValidator::new(address, pk, 1)]);

        let app_task = tokio::spawn(run_engine_app(
            bridge,
            channels,
            validator_set,
            BlockHash([0u8; 32]),
            OpenHlHeight(7),
            1,
            |_hash, _height| Ok(()),
        ));

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        tx_consensus
            .send(AppMsg::GetHistoryMinHeight { reply: reply_tx })
            .await
            .expect("send history request");
        drop(tx_consensus);

        let min_height = reply_rx.await.expect("history min reply");
        assert_eq!(min_height, OpenHlHeight(7));

        let err = app_task
            .await
            .expect("app task join")
            .expect_err("channel close should return error");
        assert!(
            err.to_string().contains("consensus channel closed after 0 decisions"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn decided_after_get_value_does_not_rebuild_payload() {
        let bridge = Arc::new(StubBridge::default());
        let (tx_consensus, rx_consensus) = mpsc::channel(8);
        let (tx_network, _rx_network) = mpsc::channel(4);
        let channels = Channels {
            consensus: rx_consensus,
            network: tx_network,
            events: TxEvent::new(),
        };

        let sk = PrivateKey::generate(OsRng);
        let pk = sk.public_key();
        let digest = Sha256::digest(pk.as_bytes());
        let mut addr_bytes = [0u8; 20];
        addr_bytes.copy_from_slice(&digest[12..32]);
        let address = OpenHlAddress(addr_bytes);
        let validator_set = OpenHlValidatorSet::new(vec![OpenHlValidator::new(address, pk, 1)]);

        let app_task = tokio::spawn(run_engine_app(
            bridge.clone(),
            channels,
            validator_set.clone(),
            BlockHash([0u8; 32]),
            OpenHlHeight::INITIAL,
            1,
            |_hash, _height| Ok(()),
        ));

        let (gv_tx, gv_rx) = tokio::sync::oneshot::channel();
        tx_consensus
            .send(AppMsg::GetValue {
                height: OpenHlHeight::INITIAL,
                round: Round::new(0),
                timeout: Duration::from_secs(1),
                reply: gv_tx,
            })
            .await
            .expect("send get value");
        let proposed = gv_rx.await.expect("get value reply");
        assert_eq!(proposed.value.0, BlockHash([0x42u8; 32]));

        let (decided_tx, decided_rx) = tokio::sync::oneshot::channel();
        tx_consensus
            .send(AppMsg::Decided {
                certificate: CommitCertificate {
                    height: OpenHlHeight::INITIAL,
                    round: Round::new(0),
                    value_id: BlockHash([0x42u8; 32]),
                    commit_signatures: Vec::new(),
                },
                extensions: VoteExtensions::default(),
                reply: decided_tx,
            })
            .await
            .expect("send decided");
        drop(tx_consensus);

        let _ = decided_rx.await.expect("decided reply");
        let decisions = app_task
            .await
            .expect("app task join")
            .expect("app task success");
        assert_eq!(decisions, vec![BlockHash([0x42u8; 32])]);
        assert_eq!(
            *bridge.build_calls.lock().expect("poisoned"),
            1,
            "GetValue at this height must prevent duplicate build_payload in Decided",
        );
    }

    #[tokio::test]
    async fn decided_without_get_value_rebuilds_once_for_follower_path() {
        let bridge = Arc::new(StubBridge::default());
        let (tx_consensus, rx_consensus) = mpsc::channel(8);
        let (tx_network, _rx_network) = mpsc::channel(4);
        let channels = Channels {
            consensus: rx_consensus,
            network: tx_network,
            events: TxEvent::new(),
        };

        let sk = PrivateKey::generate(OsRng);
        let pk = sk.public_key();
        let digest = Sha256::digest(pk.as_bytes());
        let mut addr_bytes = [0u8; 20];
        addr_bytes.copy_from_slice(&digest[12..32]);
        let address = OpenHlAddress(addr_bytes);
        let validator_set = OpenHlValidatorSet::new(vec![OpenHlValidator::new(address, pk, 1)]);

        let app_task = tokio::spawn(run_engine_app(
            bridge.clone(),
            channels,
            validator_set,
            BlockHash([0u8; 32]),
            OpenHlHeight::INITIAL,
            1,
            |_hash, _height| Ok(()),
        ));

        let (decided_tx, decided_rx) = tokio::sync::oneshot::channel();
        tx_consensus
            .send(AppMsg::Decided {
                certificate: CommitCertificate {
                    height: OpenHlHeight::INITIAL,
                    round: Round::new(0),
                    value_id: BlockHash([0x42u8; 32]),
                    commit_signatures: Vec::new(),
                },
                extensions: VoteExtensions::default(),
                reply: decided_tx,
            })
            .await
            .expect("send decided");
        drop(tx_consensus);

        let _ = decided_rx.await.expect("decided reply");
        let decisions = app_task
            .await
            .expect("app task join")
            .expect("app task success");
        assert_eq!(decisions, vec![BlockHash([0x42u8; 32])]);
        assert_eq!(
            *bridge.build_calls.lock().expect("poisoned"),
            1,
            "Follower path should rebuild once in Decided when GetValue was not called",
        );
    }
}
