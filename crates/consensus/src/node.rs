//! `Node` trait implementation — describes our chain to Malachite's engine
//! and provides the [`OpenHlNode::start`] entry point that calls
//! `malachitebft_app_channel::start_engine` to spawn the actor system.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::eyre;
use informalsystems_malachitebft_app::node::{EngineHandle, Node, NodeConfig, NodeHandle};
use informalsystems_malachitebft_app::types::Keypair;
use informalsystems_malachitebft_app::events::TxEvent;
use informalsystems_malachitebft_app_channel::Channels;
use informalsystems_malachitebft_config::{ConsensusConfig, ValueSyncConfig, ValuePayload};
use informalsystems_malachitebft_core_types::Height as _;
use informalsystems_malachitebft_signing_ed25519::{PrivateKey, PublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::timeout;
use std::time::Duration;

use crate::codec::OpenHlCodec;
use crate::context::OpenHlContext;
use crate::signing_provider::OpenHlSigningProvider;
use crate::types::{OpenHlAddress, OpenHlHeight, OpenHlValidatorSet};

const DEFAULT_STARTUP_READY_TIMEOUT: Duration = Duration::from_secs(5);

fn spawn_consensus_forwarder(
    mut raw_consensus: mpsc::Receiver<informalsystems_malachitebft_app_channel::AppMsg<OpenHlContext>>,
    consensus_tx: mpsc::Sender<informalsystems_malachitebft_app_channel::AppMsg<OpenHlContext>>,
) -> oneshot::Receiver<()> {
    let (ready_tx, ready_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut ready_tx = Some(ready_tx);
        while let Some(msg) = raw_consensus.recv().await {
            if let Some(tx) = ready_tx.take() {
                let _ = tx.send(());
            }
            if consensus_tx.send(msg).await.is_err() {
                break;
            }
        }
    });
    ready_rx
}

async fn await_startup_ready(
    ready_rx: oneshot::Receiver<()>,
    wait_for: Duration,
) -> eyre::Result<()> {
    match timeout(wait_for, ready_rx).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(eyre!(
            "consensus startup failed: host/app channel closed before first message"
        )),
        Err(_) => Err(eyre!(
            "consensus startup timed out after {}s waiting for first app message; check listen_addr/permissions",
            wait_for.as_secs()
        )),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenHlConfig {
    pub moniker: String,
    #[serde(flatten)]
    pub consensus: ConsensusConfig,
    pub value_sync: ValueSyncConfig,
}

impl OpenHlConfig {
    #[must_use]
    pub fn new(moniker: impl Into<String>) -> Self {
        // OpenHL runs ProposalOnly (no streaming proposal parts) — must match
        // our `Context::ProposalPart` shape.
        let consensus = ConsensusConfig {
            value_payload: ValuePayload::ProposalOnly,
            ..ConsensusConfig::default()
        };
        Self {
            moniker: moniker.into(),
            consensus,
            value_sync: ValueSyncConfig::default(),
        }
    }
}

impl NodeConfig for OpenHlConfig {
    fn moniker(&self) -> &str {
        &self.moniker
    }
    fn consensus(&self) -> &ConsensusConfig {
        &self.consensus
    }
    fn value_sync(&self) -> &ValueSyncConfig {
        &self.value_sync
    }
}

/// Genesis is a unit struct at v0 — the validator set is passed directly to
/// `start_engine` rather than read from disk. When `OpenHL` grows a real
/// on-disk genesis format this becomes the `load_genesis()` return.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpenHlGenesis;

/// Wire-friendly wrapper around the raw 32-byte Ed25519 private key.
#[derive(Clone, Serialize, Deserialize)]
pub struct OpenHlPrivateKeyFile {
    pub bytes: [u8; 32],
}

impl OpenHlPrivateKeyFile {
    #[must_use]
    pub fn from_private_key(sk: &PrivateKey) -> Self {
        Self {
            bytes: sk.inner().to_bytes(),
        }
    }

    #[must_use]
    pub fn into_private_key(self) -> PrivateKey {
        PrivateKey::from(self.bytes)
    }
}

impl std::fmt::Debug for OpenHlPrivateKeyFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenHlPrivateKeyFile")
            .field("bytes", &"[redacted]")
            .finish()
    }
}

/// Handle returned by [`OpenHlNode::start`]. Owns the engine actor system
/// and the channel handles for the (yet-to-be-implemented) app loop.
pub struct OpenHlNodeHandle {
    engine: EngineHandle,
    channels: Mutex<Option<Channels<OpenHlContext>>>,
    events: TxEvent<OpenHlContext>,
}

impl std::fmt::Debug for OpenHlNodeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenHlNodeHandle")
            .field("engine", &"<EngineHandle>")
            .field("channels", &"<Channels>")
            .finish()
    }
}

impl OpenHlNodeHandle {
    /// Take ownership of the engine→app message channels. Returns None on
    /// the second call. Stage 6d will consume from this to drive the bridge.
    pub async fn take_channels(&self) -> Option<Channels<OpenHlContext>> {
        self.channels.lock().await.take()
    }
}

#[async_trait]
impl NodeHandle<OpenHlContext> for OpenHlNodeHandle {
    fn subscribe(&self) -> informalsystems_malachitebft_app::events::RxEvent<OpenHlContext> {
        self.events.subscribe()
    }

    async fn kill(&self, _reason: Option<String>) -> eyre::Result<()> {
        self.engine.actor.kill_and_wait(None).await?;
        self.engine.handle.abort();
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct OpenHlNode {
    pub private_key: PrivateKey,
    pub validator_set: OpenHlValidatorSet,
    pub home_dir: PathBuf,
    pub moniker: String,
    /// Optional libp2p listen multiaddr override (Stage 13k). When
    /// `None`, defaults to `/ip4/127.0.0.1/tcp/0` (ephemeral local
    /// port — the prior behavior, fine for single-validator devnets
    /// and tests). When `Some`, must be a valid libp2p multiaddr such
    /// as `/ip4/0.0.0.0/tcp/9000`.
    pub listen_addr: Option<String>,
    /// libp2p multiaddrs of other validators we should maintain
    /// persistent connections to (Stage 13l). Empty for
    /// single-validator devnets. For multi-validator deployments,
    /// callers populate this from the validator-set JSON's
    /// `peer_multiaddr` entries, filtered to exclude self. Parsed
    /// during `load_config()` and forwarded into
    /// `cfg.consensus.p2p.persistent_peers`.
    pub persistent_peers: Vec<String>,
    /// Optional override for Malachite value payload mode.
    ///
    /// Production defaults to `ProposalOnly` (the OpenHL target shape), but
    /// tests can force `ProposalAndParts` for better compatibility with the
    /// current upstream app-channel behaviors.
    pub value_payload: Option<ValuePayload>,
    /// If set, `start()` waits for the first consensus app message to prove
    /// the host/network path is alive; on timeout it tears down actors and
    /// returns an error instead of handing back a silently stalled handle.
    pub startup_ready_timeout: Option<Duration>,
}

impl OpenHlNode {
    #[must_use]
    pub fn new(
        private_key: PrivateKey,
        validator_set: OpenHlValidatorSet,
        home_dir: PathBuf,
        moniker: impl Into<String>,
    ) -> Self {
        Self {
            private_key,
            validator_set,
            home_dir,
            moniker: moniker.into(),
            listen_addr: None,
            persistent_peers: Vec::new(),
            value_payload: None,
            startup_ready_timeout: Some(DEFAULT_STARTUP_READY_TIMEOUT),
        }
    }

    /// Override the libp2p listen multiaddr. See
    /// [`OpenHlNode::listen_addr`]; typical production deployments
    /// pass `/ip4/0.0.0.0/tcp/<port>` so peers can dial in.
    #[must_use]
    pub fn with_listen_addr(mut self, multiaddr: impl Into<String>) -> Self {
        self.listen_addr = Some(multiaddr.into());
        self
    }

    /// Set the libp2p persistent-peer multiaddrs. See
    /// [`OpenHlNode::persistent_peers`]; callers should filter out
    /// their own peer entry to avoid self-dial.
    #[must_use]
    pub fn with_persistent_peers<I, S>(mut self, peers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.persistent_peers = peers.into_iter().map(Into::into).collect();
        self
    }

    /// Override the consensus value payload mode.
    #[must_use]
    pub fn with_value_payload(mut self, value_payload: ValuePayload) -> Self {
        self.value_payload = Some(value_payload);
        self
    }

    /// Override startup readiness timeout. `None` disables the check.
    #[must_use]
    pub fn with_startup_ready_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.startup_ready_timeout = timeout;
        self
    }

    /// Disable startup readiness checks. Useful for deterministic unit tests
    /// in constrained environments.
    #[must_use]
    pub fn without_startup_ready_check(mut self) -> Self {
        self.startup_ready_timeout = None;
        self
    }
}

#[async_trait]
impl Node for OpenHlNode {
    type Context = OpenHlContext;
    type Config = OpenHlConfig;
    type Genesis = OpenHlGenesis;
    type PrivateKeyFile = OpenHlPrivateKeyFile;
    type SigningProvider = OpenHlSigningProvider;
    type NodeHandle = OpenHlNodeHandle;

    fn get_home_dir(&self) -> PathBuf {
        self.home_dir.clone()
    }

    fn load_config(&self) -> eyre::Result<Self::Config> {
        let mut cfg = OpenHlConfig::new(&self.moniker);
        if let Some(payload) = self.value_payload {
            cfg.consensus.value_payload = payload;
        }
        // listen_addr: ephemeral local port by default (fine for tests
        // and single-validator devnets), explicit override via
        // `OpenHlNode::with_listen_addr` for multi-validator deployments.
        let raw = self
            .listen_addr
            .as_deref()
            .unwrap_or("/ip4/127.0.0.1/tcp/0");
        cfg.consensus.p2p.listen_addr = raw
            .parse()
            .map_err(|e| eyre!("invalid listen_addr `{raw}`: {e}"))?;
        // Stage 13l: parse persistent peer multiaddrs and forward
        // into Malachite's p2p config. Empty list (default) preserves
        // the single-validator path.
        let mut parsed_peers = Vec::with_capacity(self.persistent_peers.len());
        for peer in &self.persistent_peers {
            let parsed = peer
                .parse()
                .map_err(|e| eyre!("invalid persistent peer multiaddr `{peer}`: {e}"))?;
            parsed_peers.push(parsed);
        }
        cfg.consensus.p2p.persistent_peers = parsed_peers;
        Ok(cfg)
    }

    fn get_address(&self, pk: &PublicKey) -> OpenHlAddress {
        let digest = Sha256::digest(pk.as_bytes());
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&digest[12..32]);
        OpenHlAddress(addr)
    }

    fn get_public_key(&self, pk: &PrivateKey) -> PublicKey {
        pk.public_key()
    }

    fn get_keypair(&self, pk: PrivateKey) -> Keypair {
        Keypair::ed25519_from_bytes(pk.inner().to_bytes())
            .expect("ed25519 private key is always 32 bytes")
    }

    fn load_private_key(&self, file: Self::PrivateKeyFile) -> PrivateKey {
        file.into_private_key()
    }

    fn load_private_key_file(&self) -> eyre::Result<Self::PrivateKeyFile> {
        Ok(OpenHlPrivateKeyFile::from_private_key(&self.private_key))
    }

    fn load_genesis(&self) -> eyre::Result<Self::Genesis> {
        // Validator set is passed directly to start_engine; genesis carries
        // nothing else at v0.
        Ok(OpenHlGenesis)
    }

    fn get_signing_provider(&self, private_key: PrivateKey) -> Self::SigningProvider {
        OpenHlSigningProvider::new(private_key)
    }

    async fn start(&self) -> eyre::Result<Self::NodeHandle> {
        let cfg = self.load_config()?;
        let validator_set = self.validator_set.clone();

        let (channels, engine) = informalsystems_malachitebft_app_channel::start_engine(
            OpenHlContext,
            self.clone(),
            cfg,
            OpenHlCodec, // WAL
            OpenHlCodec, // Network
            Some(OpenHlHeight::INITIAL),
            validator_set,
        )
        .await?;

        let raw_consensus = channels.consensus;
        let (consensus_tx, consensus_rx) = mpsc::channel(128);
        let ready_rx = spawn_consensus_forwarder(raw_consensus, consensus_tx);

        if let Some(wait_for) = self.startup_ready_timeout {
            if let Err(err) = await_startup_ready(ready_rx, wait_for).await {
                let _ = engine.actor.kill_and_wait(None).await;
                engine.handle.abort();
                return Err(err);
            }
        }

        let events = channels.events.clone();
        let channels = Channels {
            consensus: consensus_rx,
            network: channels.network,
            events: channels.events,
        };

        Ok(OpenHlNodeHandle {
            engine,
            channels: Mutex::new(Some(channels)),
            events,
        })
    }

    async fn run(self) -> eyre::Result<()> {
        // Stage 6d will consume from channels here and run the app loop.
        Err(eyre!("OpenHlNode::run is not yet implemented (Stage 6d)"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use informalsystems_malachitebft_app_channel::AppMsg;
    use crate::types::OpenHlValidator;
    use rand::rngs::OsRng;
    use std::time::Duration;

    fn single_validator_node(home_dir: PathBuf) -> OpenHlNode {
        let sk = PrivateKey::generate(OsRng);
        let pk = sk.public_key();
        let digest = Sha256::digest(pk.as_bytes());
        let mut addr_bytes = [0u8; 20];
        addr_bytes.copy_from_slice(&digest[12..32]);
        let address = OpenHlAddress(addr_bytes);
        let validator_set = OpenHlValidatorSet::new(vec![OpenHlValidator::new(address, pk, 1)]);
        OpenHlNode::new(sk, validator_set, home_dir, "openhl-test").without_startup_ready_check()
    }

    #[test]
    fn private_key_file_round_trips() {
        let sk = PrivateKey::generate(OsRng);
        let file = OpenHlPrivateKeyFile::from_private_key(&sk);
        let restored = file.into_private_key();
        assert_eq!(restored.inner().to_bytes(), sk.inner().to_bytes());
    }

    #[test]
    fn load_config_sets_proposal_only_payload_and_ephemeral_listen_addr() {
        let tmp = tempfile::tempdir().unwrap();
        let node = single_validator_node(tmp.path().to_path_buf());
        let cfg = node.load_config().unwrap();
        assert_eq!(cfg.consensus.value_payload, ValuePayload::ProposalOnly);
        // listen_addr should be /ip4/127.0.0.1/tcp/0 (ephemeral)
        let listen_str = cfg.consensus.p2p.listen_addr.to_string();
        assert!(
            listen_str.starts_with("/ip4/127.0.0.1/tcp/0"),
            "unexpected listen_addr: {listen_str}"
        );
    }

    #[test]
    fn with_listen_addr_overrides_default() {
        let tmp = tempfile::tempdir().unwrap();
        let node = single_validator_node(tmp.path().to_path_buf())
            .with_listen_addr("/ip4/0.0.0.0/tcp/26656");
        let cfg = node.load_config().unwrap();
        let listen_str = cfg.consensus.p2p.listen_addr.to_string();
        assert!(
            listen_str.starts_with("/ip4/0.0.0.0/tcp/26656"),
            "expected listen_addr override, got: {listen_str}"
        );
    }

    #[test]
    fn default_persistent_peers_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let node = single_validator_node(tmp.path().to_path_buf());
        let cfg = node.load_config().unwrap();
        assert!(cfg.consensus.p2p.persistent_peers.is_empty());
    }

    #[test]
    fn with_persistent_peers_forwards_into_consensus_config() {
        let tmp = tempfile::tempdir().unwrap();
        let node = single_validator_node(tmp.path().to_path_buf())
            .with_persistent_peers(vec![
                "/ip4/10.0.0.5/tcp/9001",
                "/ip4/10.0.0.6/tcp/9002",
            ]);
        let cfg = node.load_config().unwrap();
        let rendered: Vec<String> = cfg
            .consensus
            .p2p
            .persistent_peers
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            rendered,
            vec![
                "/ip4/10.0.0.5/tcp/9001".to_string(),
                "/ip4/10.0.0.6/tcp/9002".to_string(),
            ]
        );
    }

    #[test]
    fn with_persistent_peers_rejects_malformed_multiaddr() {
        let tmp = tempfile::tempdir().unwrap();
        let node = single_validator_node(tmp.path().to_path_buf())
            .with_persistent_peers(vec!["not-a-multiaddr"]);
        let err = node.load_config().expect_err("malformed peer should error");
        let msg = format!("{err}");
        assert!(
            msg.contains("invalid persistent peer multiaddr"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn with_listen_addr_rejects_malformed_multiaddr() {
        let tmp = tempfile::tempdir().unwrap();
        let node = single_validator_node(tmp.path().to_path_buf())
            .with_listen_addr("not-a-multiaddr");
        let err = node.load_config().expect_err("malformed multiaddr should error");
        let msg = format!("{err}");
        assert!(
            msg.contains("invalid listen_addr"),
            "unexpected error message: {msg}"
        );
    }

    #[test]
    fn get_address_matches_runner_derivation() {
        let tmp = tempfile::tempdir().unwrap();
        let node = single_validator_node(tmp.path().to_path_buf());
        let pk = node.private_key.public_key();
        let addr1 = node.get_address(&pk);
        // Same derivation as runner.rs (last 20 bytes of SHA-256(pubkey)).
        let digest = Sha256::digest(pk.as_bytes());
        let mut expected = [0u8; 20];
        expected.copy_from_slice(&digest[12..32]);
        assert_eq!(addr1, OpenHlAddress(expected));
    }

    /// Smoke test: spin up the actor system, get a handle back, kill cleanly.
    /// Does NOT drive consensus — that's Stage 6d.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_engine_smoke_spawns_and_kills() {
        let tmp = tempfile::tempdir().unwrap();
        let node = single_validator_node(tmp.path().to_path_buf());
        let handle = match node.start().await {
            Ok(h) => h,
            Err(e) => panic!("start_engine failed: {e:?}"),
        };
        // Sanity-poke the channels handle is available exactly once.
        assert!(handle.take_channels().await.is_some());
        assert!(handle.take_channels().await.is_none());
        handle.kill(None).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_forwarder_signals_and_forwards_first_message() {
        let (raw_tx, raw_rx) = mpsc::channel(8);
        let (forward_tx, mut forward_rx) = mpsc::channel(8);
        let ready_rx = spawn_consensus_forwarder(raw_rx, forward_tx);

        let (reply, _wait) = oneshot::channel();
        raw_tx
            .send(AppMsg::GetHistoryMinHeight { reply })
            .await
            .expect("send mock app message");

        await_startup_ready(ready_rx, Duration::from_secs(1))
            .await
            .expect("startup ready should be signaled");

        let forwarded = forward_rx
            .recv()
            .await
            .expect("forwarder should preserve first message");
        assert!(matches!(forwarded, AppMsg::GetHistoryMinHeight { .. }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn startup_ready_timeout_errors_without_signal() {
        let (_tx, rx) = oneshot::channel::<()>();
        let err = await_startup_ready(rx, Duration::from_millis(20))
            .await
            .expect_err("missing startup signal should timeout");
        assert!(
            err.to_string().contains("timed out"),
            "unexpected startup timeout error: {err}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "Diagnostic: event timing is environment-dependent (sandbox/actor scheduling)"]
    async fn start_engine_emits_listening_event() {
        let tmp = tempfile::tempdir().unwrap();
        let node = single_validator_node(tmp.path().to_path_buf());
        let handle = node.start().await.expect("start_engine failed");
        let mut events = handle.subscribe();

        let first_event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("timed out waiting for first engine event")
            .expect("event channel closed before first event");

        let event_text = first_event.to_string();
        assert!(
            event_text.contains("Listening(") || event_text.contains("StartedHeight("),
            "unexpected first event: {event_text}"
        );

        handle.kill(None).await.unwrap();
    }
}
