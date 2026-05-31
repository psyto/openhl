//! Engine app loop — consumes `AppMsg` from the Malachite engine and routes
//! every consensus-relevant event through a [`ConsensusBridge`].
//!
//! ### Stage 18a: real follower-side replication via ProposalAndParts
//!
//! Through Stage 13n the follower learned a proposer's block by calling
//! `bridge.build_payload` itself on `Decided` and checking the hash
//! matched what consensus decided. That worked only because v0's
//! `build_payload` was a pure function of `(parent, attrs)`; the
//! moment payload-building stops being deterministic (real EVM tx
//! execution, mempool ordering, wallclock-dependent fields) the
//! follower's recompute would diverge from the proposer's.
//!
//! 18a replaces that with the production-shape pattern:
//!   * Proposer's `GetValue`: build the payload, ask the bridge to
//!     `encode_proposed_block` into wire bytes, ship them as a
//!     `StreamMessage::Data` followed by a `StreamMessage::Fin`
//!     through `NetworkMsg::PublishProposalPart`.
//!   * Follower's `ReceivedProposalPart`: buffer the two messages
//!     per `(peer, stream_id)`, then call `register_proposed_block`
//!     once the stream completes. The bridge stores the block in its
//!     pending map, so the follower's subsequent `commit(hash)` finds
//!     it without recomputing anything.
//!   * `Decided` no longer rebuilds — it just commits the hash, with
//!     the block's timestamp looked up in the engine_app's local
//!     `block_times` map that both paths populate.

use std::sync::Arc;
use std::collections::{BTreeMap, HashMap};

use eyre::eyre;
use informalsystems_malachitebft_app::engine::host::Next;
use informalsystems_malachitebft_app::types::streaming::{StreamId, StreamMessage};
use informalsystems_malachitebft_app::types::{PeerId, ProposedValue};
use informalsystems_malachitebft_app_channel::{AppMsg, Channels, NetworkMsg};
use informalsystems_malachitebft_core_types::{Height as _, Round, Validity};
use informalsystems_malachitebft_engine::util::streaming::StreamContent;
use openhl_types::{BlockHash, PayloadAttrs};

use crate::bridge::ConsensusBridge;
use crate::context::OpenHlContext;
use crate::types::{OpenHlHeight, OpenHlProposalPart, OpenHlValidatorSet, OpenHlValue};

const APP_REPLY_WAIT_LOG: &str = "engine_app: peer replied unsuccessfully (channel closed)";

/// Per-stream assembly state. v0 streams are exactly two messages
/// (one `Data`, one `Fin`), so we don't need the heap-ordered
/// reassembler the Malachite example uses for multi-part streams —
/// just hold onto the data until Fin arrives.
#[derive(Default)]
struct InProgressStream {
    data: Option<OpenHlProposalPart>,
    fin_seen: bool,
}

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
///
/// `on_committed` receives the committed block hash, its consensus
/// height, and the **block-header timestamp** (Stage 15e). Using the
/// header timestamp instead of host wallclock keeps the coordinator
/// tick (oracle refresh interval, funding clock, etc.) deterministic
/// across validators — host clocks could drift millisecond-to-second
/// in a real distributed setting and silently break consensus
/// downstream.
///
/// If the hook returns `Err`, `run_engine_app` propagates the error.
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
    F: FnMut(BlockHash, OpenHlHeight, u64) -> eyre::Result<()> + Send,
{
    let mut decided: Vec<BlockHash> = Vec::new();
    let mut current_parent = initial_parent;
    let mut current_height = initial_height;
    let history_min_height = initial_height;

    // Stage 18a: timestamps keyed by decided block hash. Both proposer
    // (in `GetValue`) and follower (in `ReceivedProposalPart`) populate
    // this so `Decided` can hand the integration coordinator the
    // chain-derived block_time without recomputing the header.
    let mut block_times: HashMap<BlockHash, u64> = HashMap::new();

    // Per-(peer, stream_id) part assembly state. Streams complete in
    // O(1) for v0 since each carries exactly two messages. `BTreeMap`
    // rather than `HashMap` because Malachite's `StreamId` derives
    // `Ord` but not `Hash`.
    let mut streams: BTreeMap<(PeerId, StreamId), InProgressStream> = BTreeMap::new();

    // Per-height monotonic counter for our own outbound stream ids
    // (the engine wants distinct ones per height/round).
    let mut next_stream_id: u64 = 0;

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
                block_times.insert(block.hash, block.timestamp);

                let value = OpenHlValue(block.hash);
                let lpv =
                    informalsystems_malachitebft_app_channel::app::types::LocallyProposedValue::new(
                        height, round, value,
                    );
                if reply.send(lpv).is_err() {
                    tracing::warn!("{APP_REPLY_WAIT_LOG} (GetValue)");
                }

                // Stage 18a: stream the block to followers. Encode via
                // the bridge (opaque wire format), wrap in an
                // `OpenHlProposalPart`, send a Data + Fin pair under a
                // fresh stream id.
                let block_bytes = bridge.encode_proposed_block(id).await?;
                let part = OpenHlProposalPart {
                    height,
                    round,
                    pol_round: Round::Nil,
                    proposer: select_proposer_address(&validator_set, height, round),
                    block_bytes,
                };
                let stream_id = make_stream_id(height, round, &mut next_stream_id);
                let data_msg = StreamMessage::new(
                    stream_id.clone(),
                    0,
                    StreamContent::Data(part),
                );
                let fin_msg = StreamMessage::new(stream_id, 1, StreamContent::Fin);
                channels
                    .network
                    .send(NetworkMsg::PublishProposalPart(data_msg))
                    .await
                    .map_err(|e| eyre!("publish proposal Data part: {e}"))?;
                channels
                    .network
                    .send(NetworkMsg::PublishProposalPart(fin_msg))
                    .await
                    .map_err(|e| eyre!("publish proposal Fin part: {e}"))?;
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
                // v0 doesn't restream — a peer that missed the original
                // stream waits for the next round's broadcast or syncs
                // via `GetDecidedValue`. Production hardening would
                // re-stream from a cache of recent payloads.
            }

            AppMsg::GetHistoryMinHeight { reply } => {
                if reply.send(history_min_height).is_err() {
                    tracing::warn!("{APP_REPLY_WAIT_LOG} (GetHistoryMinHeight)");
                }
            }

            AppMsg::ReceivedProposalPart { from, part, reply } => {
                // Buffer the incoming message and, once both Data and
                // Fin have arrived for this stream, register the block
                // with the bridge and reply with a ProposedValue.
                let key = (from, part.stream_id.clone());
                let entry = streams.entry(key.clone()).or_default();
                match part.content {
                    StreamContent::Data(p) => {
                        entry.data = Some(p);
                    }
                    StreamContent::Fin => {
                        entry.fin_seen = true;
                    }
                }
                let proposed = if entry.data.is_some() && entry.fin_seen {
                    let entry = streams.remove(&key).expect("entry exists");
                    let part = entry.data.expect("data set per branch above");
                    let block = bridge
                        .register_proposed_block(&part.block_bytes)
                        .await?;
                    block_times.insert(block.hash, block.timestamp);
                    Some(ProposedValue {
                        height: part.height,
                        round: part.round,
                        valid_round: part.pol_round,
                        proposer: part.proposer,
                        value: OpenHlValue(block.hash),
                        validity: Validity::Valid,
                    })
                } else {
                    None
                };
                if reply.send(proposed).is_err() {
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

                // Stage 18a: no more recompute. The block is in the
                // bridge's pending map — either because this validator
                // built it (`GetValue` path) or because it received and
                // registered the proposer's stream
                // (`ReceivedProposalPart` path). `commit(hash)` looks
                // it up; the timestamp comes from our local map.
                let block_time = block_times.remove(&hash).ok_or_else(|| {
                    eyre!(
                        "Stage 18a: Decided for {hash:?} but no timestamp recorded — \
                         neither GetValue nor ReceivedProposalPart ran for this hash"
                    )
                })?;

                bridge.commit(hash).await?;
                on_committed(hash, certificate.height, block_time)?;
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

/// Build a fresh stream id for a proposer's outgoing parts. The byte
/// layout (height || round || counter) is opaque to the engine; what
/// matters is uniqueness across this validator's outbound streams so
/// follower-side reassembly can key on it cleanly.
fn make_stream_id(
    height: OpenHlHeight,
    round: Round,
    counter: &mut u64,
) -> StreamId {
    use bytes::BytesMut;
    let mut bytes = BytesMut::with_capacity(8 + 4 + 8);
    bytes.extend_from_slice(&height.0.to_be_bytes());
    bytes.extend_from_slice(&round.as_i64().to_be_bytes());
    bytes.extend_from_slice(&counter.to_be_bytes());
    *counter += 1;
    StreamId::new(bytes.freeze())
}

/// Pull the proposer address for `(height, round)` straight out of the
/// validator set using the same selection rule
/// [`OpenHlContext::select_proposer`] uses. Each engine_app instance
/// only ever calls this for heights where IT is the proposer, so the
/// result matches its own validator address — Malachite uses the
/// embedded `proposer` for attribution when peers receive the part.
fn select_proposer_address(
    validator_set: &OpenHlValidatorSet,
    height: OpenHlHeight,
    round: Round,
) -> crate::types::OpenHlAddress {
    use informalsystems_malachitebft_core_types::ValidatorSet as _;
    let count = validator_set.count();
    assert!(count > 0, "validator set is empty");
    let round_u64 = u64::try_from(round.as_i64().max(0)).unwrap_or(0);
    let index = usize::try_from(height.0.wrapping_add(round_u64))
        .unwrap_or(usize::MAX) % count;
    let validator = validator_set
        .get_by_index(index)
        .expect("index < count by construction");
    use informalsystems_malachitebft_core_types::Validator as _;
    *validator.address()
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
    use openhl_types::{ExecutedBlock, PayloadAttrs, PayloadId, PayloadStatus};
    use rand::rngs::OsRng;
    use sha2::{Digest, Sha256};
    use std::sync::{Arc as StdArc, Mutex};
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[derive(Debug, Default)]
    struct StubBridge {
        last_built: Mutex<Option<BlockHash>>,
        build_calls: Mutex<usize>,
        register_calls: Mutex<usize>,
        committed: Mutex<Vec<BlockHash>>,
    }

    /// Test wire format for StubBridge — just the executed block. Symmetric
    /// with `ProposedBlockWire` in `openhl-evm` (which also includes a
    /// Header), but the stub doesn't need a Header to satisfy its own
    /// commit/build_payload contract.
    #[derive(serde::Serialize, serde::Deserialize)]
    struct StubProposedBlock {
        block: ExecutedBlock,
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
            self.committed.lock().expect("poisoned").push(block_hash);
            Ok(())
        }

        async fn encode_proposed_block(
            &self,
            _id: PayloadId,
        ) -> Result<Vec<u8>, BridgeError> {
            let wire = StubProposedBlock {
                block: ExecutedBlock {
                    hash: BlockHash([0x42u8; 32]),
                    parent_hash: BlockHash([0u8; 32]),
                    number: 1,
                    state_root: [0u8; 32],
                    timestamp: 1,
                },
            };
            serde_json::to_vec(&wire).map_err(|e| BridgeError::Internal(eyre!(e)))
        }

        async fn register_proposed_block(
            &self,
            bytes: &[u8],
        ) -> Result<ExecutedBlock, BridgeError> {
            *self.register_calls.lock().expect("poisoned") += 1;
            let wire: StubProposedBlock = serde_json::from_slice(bytes)
                .map_err(|e| BridgeError::Rejected(e.to_string()))?;
            Ok(wire.block)
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
            |_hash, _height, _block_time| Ok(()),
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
            "Stage 18a: single-validator proposer-only path → exactly one build_payload",
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
            |_hash, _height, _block_time| Ok(()),
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

    /// Stage 18a — proposer path: a GetValue followed by Decided does
    /// NOT call register_proposed_block (the block already lives in
    /// pending from the build_payload call) and does NOT re-call
    /// build_payload (no more recompute trick). Exactly one
    /// build_payload total.
    #[tokio::test]
    async fn decided_after_get_value_does_not_rebuild_or_register() {
        let bridge = Arc::new(StubBridge::default());
        let (tx_consensus, rx_consensus) = mpsc::channel(8);
        let (tx_network, mut rx_network) = mpsc::channel(16);
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
            |_hash, _height, _block_time| Ok(()),
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

        // The engine_app should also have pushed Data + Fin to the
        // network — drain them so the channel doesn't fill up.
        for _ in 0..2 {
            let _ = tokio::time::timeout(Duration::from_secs(1), rx_network.recv()).await;
        }

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
            "GetValue triggers exactly one build_payload; Decided does NOT recompute",
        );
        assert_eq!(
            *bridge.register_calls.lock().expect("poisoned"),
            0,
            "Proposer path never goes through register_proposed_block",
        );
    }

    /// Stage 18a — follower path: a Decided without prior GetValue or
    /// ReceivedProposalPart should ERROR (no recompute fallback). This
    /// pins the "we no longer silently rebuild" property.
    #[tokio::test]
    async fn decided_without_register_errors_on_unknown_hash() {
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
            |_hash, _height, _block_time| Ok(()),
        ));

        let (decided_tx, _decided_rx) = tokio::sync::oneshot::channel();
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

        let err = app_task
            .await
            .expect("app task join")
            .expect_err("Decided without prior register/build must error");
        let msg = err.to_string();
        assert!(
            msg.contains("no timestamp recorded"),
            "expected timestamp-missing error, got: {msg}",
        );
        assert_eq!(
            *bridge.build_calls.lock().expect("poisoned"),
            0,
            "no implicit rebuild — that's the whole point of Stage 18a",
        );
    }

    /// Stage 18a — follower path: ReceivedProposalPart (Data + Fin)
    /// then Decided commits without ever calling build_payload.
    #[tokio::test]
    async fn follower_register_then_decided_commits_without_rebuild() {
        use informalsystems_malachitebft_app::types::PeerId;
        use informalsystems_malachitebft_app::types::streaming::{StreamId, StreamMessage};
        use informalsystems_malachitebft_engine::util::streaming::StreamContent;
        use bytes::Bytes;

        // A valid multihash with identity-hash code (0x00) and a 32-byte
        // payload. We need any concrete PeerId; the contents don't
        // matter for assembly (the engine_app keys streams by
        // (peer, stream_id), not by validating peer authenticity).
        let mut peer_bytes = vec![0x00, 0x20];
        peer_bytes.extend_from_slice(&[0x11u8; 32]);
        let from = PeerId::from_bytes(&peer_bytes).expect("valid peer id multihash");

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
            |_hash, _height, _block_time| Ok(()),
        ));

        // Simulate a proposer streaming the Data + Fin parts.
        let stream_id = StreamId::new(Bytes::from_static(b"test-stream"));
        let stub_block_bytes = serde_json::to_vec(&StubProposedBlock {
            block: ExecutedBlock {
                hash: BlockHash([0x42u8; 32]),
                parent_hash: BlockHash([0u8; 32]),
                number: 1,
                state_root: [0u8; 32],
                timestamp: 1,
            },
        })
        .unwrap();
        let part = OpenHlProposalPart {
            height: OpenHlHeight::INITIAL,
            round: Round::new(0),
            pol_round: Round::Nil,
            proposer: address,
            block_bytes: stub_block_bytes,
        };

        let (data_reply_tx, data_reply_rx) = tokio::sync::oneshot::channel();
        tx_consensus
            .send(AppMsg::ReceivedProposalPart {
                from,
                part: StreamMessage::new(stream_id.clone(), 0, StreamContent::Data(part)),
                reply: data_reply_tx,
            })
            .await
            .expect("send data part");
        let after_data = data_reply_rx.await.expect("data reply");
        assert!(after_data.is_none(), "Data alone shouldn't complete the stream");

        let (fin_reply_tx, fin_reply_rx) = tokio::sync::oneshot::channel();
        tx_consensus
            .send(AppMsg::ReceivedProposalPart {
                from,
                part: StreamMessage::new(stream_id, 1, StreamContent::Fin),
                reply: fin_reply_tx,
            })
            .await
            .expect("send fin part");
        let after_fin = fin_reply_rx.await.expect("fin reply");
        let proposed = after_fin.expect("Fin should complete the stream");
        assert_eq!(proposed.value.0, BlockHash([0x42u8; 32]));

        // Now Decided commits — and the block_time is the one
        // register_proposed_block returned.
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
            0,
            "Follower path must NEVER call build_payload — the 18a contract",
        );
        assert_eq!(
            *bridge.register_calls.lock().expect("poisoned"),
            1,
            "Exactly one register_proposed_block per follower-received block",
        );
        assert_eq!(
            bridge.committed.lock().unwrap().as_slice(),
            &[BlockHash([0x42u8; 32])],
        );
    }
}
