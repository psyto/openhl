//! openhl — Hyperliquid-shape L1 reference implementation.
//!
//! Three subcommands:
//!
//!   - `info` (default) — print the node's static config + initial state.
//!   - `devnet [N]` — `N` single-validator consensus rounds through an
//!     in-memory EVM bridge, calling `OpenHlNode::tick` between blocks.
//!     Stage 13b. The smallest runnable demo of the full per-block flow
//!     at the binary level.
//!   - `reth-devnet [N]` — Boots the production-shape stack: Reth via
//!     `NodeBuilder` + `OpenHlExecutorBuilder`, then `LiveRethEvmBridge`
//!     against its provider, then the Malachite actor engine via
//!     consensus `OpenHlNode::start`, then `run_engine_app` to drive
//!     consensus decisions. Stage 13c.
//!
//!     Stage 13d + 8e make `reth-devnet N` produce N real blocks
//!     end-to-end. 13d plumbed Reth's `ChainSpec::genesis_hash()` as
//!     the consensus engine's initial parent. 8e made the bridge's
//!     `build_payload` consult its own internal `chain` map for parent
//!     lookup before falling back to Reth's provider — the bridge's
//!     `commit` doesn't upload an `ExecutionPayload` to Reth (the
//!     synthetic headers have placeholder `state_root`s that Reth would
//!     reject), but consensus only needs the bridge to be
//!     self-consistent, which it now is.
//!
//! Examples:
//!   $ openhl                                      # equivalent to `openhl info`
//!   $ openhl info
//!   $ openhl devnet                               # one in-memory round
//!   $ openhl devnet --rounds 5                    # five in-memory rounds
//!   $ openhl reth-devnet                          # one Reth-backed decision
//!   $ openhl reth-devnet --rounds 3
//!   $ openhl reth-devnet --moniker alice --data-dir ~/.openhl/data
//!
//! Stage 13e (this commit) introduces clap-based subcommands and the
//! `--moniker` / `--data-dir` flags. Full production `NodeBuilder` path
//! (persistent across restarts, real network config, multi-validator)
//! lands in Stage 13f.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_genesis::Genesis;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use informalsystems_malachitebft_app::node::{Node, NodeHandle};
use informalsystems_malachitebft_signing_ed25519::PrivateKey;
use openhl_consensus::run_engine_app;
use openhl_consensus::run_single_validator;
use openhl_consensus::OpenHlPrivateKeyFile;
use openhl_evm::{BridgeSnapshot, InMemoryEvmBridge, LiveRethEvmBridge, OpenHlExecutorBuilder};
use openhl_funding::MarkPrice;
use openhl_node::{OpenHlNode, OpenHlNodeConfig, TickInput, TickReport};
use openhl_types::BlockHash;
use rand::rngs::OsRng;
use reth_chainspec::ChainSpec;
use reth_db::{init_db, mdbx::DatabaseArguments};
use reth_node_builder::{NodeBuilder, NodeHandle as RethNodeHandle};
use reth_node_core::{
    args::DatadirArgs,
    dirs::{DataDirPath, MaybePlatformPath},
    node_config::NodeConfig,
};
use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
use reth_tasks::Runtime;
use sha2::{Digest, Sha256};

#[derive(Debug, Parser)]
#[command(
    name = "openhl",
    version,
    about = "Hyperliquid-shape L1 reference implementation",
    long_about = None
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the node's static config and initial state (default).
    Info,

    /// Drive single-validator consensus rounds through an in-memory bridge,
    /// calling `OpenHlNode::tick` between blocks. Stage 13b demo path.
    Devnet {
        /// Number of consensus rounds to drive.
        #[arg(long, default_value_t = 1)]
        rounds: u64,
    },

    /// Drive consensus decisions through Reth-backed `LiveRethEvmBridge` +
    /// the Malachite actor engine. Stage 13c-e production-shape boot.
    RethDevnet {
        /// Number of consensus decisions to drive.
        #[arg(long, default_value_t = 1)]
        rounds: u64,

        /// Moniker for the consensus node identity (used in logs / network
        /// p2p discovery when wired). Default: openhl-reth-devnet.
        #[arg(long, default_value = "openhl-reth-devnet")]
        moniker: String,

        /// Data directory for Reth's MDBX database and the consensus
        /// home dir. Defaults to `$HOME/.openhl/data`.
        ///
        /// Stage 13f swapped this to the production `NodeBuilder` path
        /// (`reth_db::init_db` + `with_database` + `with_launch_context`),
        /// so the directory is now a real persistent MDBX database — it
        /// is **not** deleted at process exit. Re-running with the same
        /// `--data-dir` opens the existing database. Stages 13g–13i
        /// added bridge snapshot, validator-key, and consensus-height
        /// resume on top.
        #[arg(long)]
        data_dir: Option<PathBuf>,

        /// Path to a JSON chain spec file (Stage 13j). If omitted, the
        /// embedded dev chain spec (chain id 2600) is used — the same
        /// one Reth uses in its `examples/custom-dev-node`. Real
        /// deployments load a per-network spec.
        ///
        /// Format: the standard `alloy_genesis::Genesis` JSON
        /// (`nonce` / `timestamp` / `extraData` / `gasLimit` /
        /// `difficulty` / `mixHash` / `coinbase` / `alloc` / `number` /
        /// `gasUsed` / `parentHash` / `config`).
        #[arg(long)]
        chain_spec: Option<PathBuf>,

        /// Path to a JSON validator-set file (Stage 13j). If omitted,
        /// a single-validator set is constructed from the loaded
        /// (or freshly generated) validator key — the existing
        /// behavior through Stage 13h.
        ///
        /// Format:
        /// ```json
        /// {
        ///   "validators": [
        ///     {
        ///       "pubkey_hex": "<64 hex chars>",
        ///       "voting_power": 1,
        ///       "peer_multiaddr": "/ip4/10.0.0.5/tcp/9000"
        ///     }
        ///   ]
        /// }
        /// ```
        ///
        /// `peer_multiaddr` is optional (Stage 13k) and currently
        /// only logged — full vote relay wiring is a follow-up.
        ///
        /// When supplied, the locally-loaded validator key's public
        /// key must appear in the set, otherwise the node refuses to
        /// start — refusing to sign on behalf of an identity the
        /// network doesn't recognize.
        #[arg(long)]
        validators: Option<PathBuf>,

        /// libp2p listen multiaddr for this node's consensus engine
        /// (Stage 13k). Default: `/ip4/127.0.0.1/tcp/0` (ephemeral
        /// local port — single-validator devnet default). For
        /// multi-validator deployments use `/ip4/0.0.0.0/tcp/<port>`
        /// so peers can dial in.
        #[arg(long)]
        listen_addr: Option<String>,

        /// Reth HTTP RPC server bind in `<addr>:<port>` form (Stage
        /// 13k). Default: Reth's `RpcServerArgs::default()` which is
        /// `127.0.0.1:8545`. Examples:
        ///   `0.0.0.0:8545` (listen on all interfaces)
        ///   `127.0.0.1:0` (let the OS pick a free port)
        ///
        /// Use IPv6 by wrapping the address in brackets:
        /// `[::1]:8545`.
        #[arg(long)]
        rpc_bind: Option<String>,
    },
}

/// On-disk shape of `--validators <path>`. Stage 13j.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValidatorSetFile {
    validators: Vec<ValidatorEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ValidatorEntry {
    /// Hex-encoded 32-byte Ed25519 public key (no `0x` prefix, no
    /// length-byte; just 64 hex chars).
    pubkey_hex: String,
    /// Voting power for this validator. Must be > 0.
    voting_power: u64,
    /// libp2p multiaddr where peers can reach this validator
    /// (Stage 13k). Optional; when present it's logged so operators
    /// can sanity-check the network layout. Full vote relay wiring
    /// remains a follow-up — the consensus engine doesn't yet
    /// consume per-peer multiaddrs from the validator set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    peer_multiaddr: Option<String>,
}

fn main() -> eyre::Result<()> {
    // Initialise tracing so Malachite/libp2p events surface. Default
    // filter `info,libp2p=warn` keeps multi-validator bring-up logs
    // readable; override via `RUST_LOG` for deeper investigation.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,libp2p=warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init()
        .ok();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Info) {
        Command::Info => {
            print_info();
            Ok(())
        }
        Command::Devnet { rounds } => tokio_rt()?.block_on(run_devnet(rounds)),
        Command::RethDevnet {
            rounds,
            moniker,
            data_dir,
            chain_spec,
            validators,
            listen_addr,
            rpc_bind,
        } => tokio_rt()?.block_on(run_reth_devnet(
            rounds,
            moniker,
            data_dir,
            chain_spec,
            validators,
            listen_addr,
            rpc_bind,
        )),
    }
}

fn tokio_rt() -> eyre::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Into::into)
}

/// Resolve the effective `--data-dir` path. If the user passed one
/// explicitly we use it as-is; otherwise we default to
/// `$HOME/.openhl/data`. Errors if neither is available (no HOME).
fn resolve_data_dir(user_supplied: Option<&PathBuf>) -> eyre::Result<PathBuf> {
    if let Some(p) = user_supplied {
        return Ok(p.clone());
    }
    let home = std::env::var("HOME")
        .map_err(|_| eyre::eyre!("--data-dir not supplied and $HOME is not set"))?;
    Ok(PathBuf::from(home).join(".openhl").join("data"))
}

fn print_info() {
    let config = OpenHlNodeConfig::hyperliquid_default();
    let node = OpenHlNode::new(config);

    println!(
        "openhl v{} (Hyperliquid-shape L1 reference)",
        env!("CARGO_PKG_VERSION")
    );
    println!("config:");
    println!(
        "  oracle refresh interval : {}s",
        config.oracle_refresh_interval_secs
    );
    println!(
        "  oracle staleness window : {}s",
        config.oracle_params.staleness_window_secs
    );
    println!(
        "  oracle min feeds        : {}",
        config.oracle_params.min_feeds_required
    );
    println!(
        "  initial margin          : {} bps",
        config.liquidation_params.initial_margin_bps
    );
    println!(
        "  maintenance margin      : {} bps",
        config.liquidation_params.maintenance_margin_bps
    );
    println!(
        "  liquidation fee         : {} bps",
        config.liquidation_params.liquidation_fee_bps
    );
    println!(
        "  vault min deposit       : {}",
        config.vault_params.min_deposit
    );
    println!(
        "  auto-ADL on deficit     : {}",
        config.run_adl_on_unfilled_deficit
    );
    println!("state:");
    println!("  oracle feeds            : {}", node.oracle().feed_count());
    println!(
        "  insurance fund balance  : {}",
        node.scanner().fund_balance()
    );
    println!(
        "  vault shares            : {}",
        node.vault().total_shares().0
    );
    println!(
        "  vault assets            : {}",
        node.vault().total_assets().0
    );
}

/// Drive `rounds` single-validator consensus rounds through an
/// **in-memory** EVM bridge, calling `OpenHlNode::tick` between each.
/// Stage 13b path — no Reth boot.
async fn run_devnet(rounds: u64) -> eyre::Result<()> {
    let mut coordinator = OpenHlNode::new(OpenHlNodeConfig::hyperliquid_default());
    let bridge = Arc::new(InMemoryEvmBridge::new());

    let mut parent = BlockHash([0u8; 32]);

    println!(
        "openhl v{} — driving {} single-validator devnet round{}",
        env!("CARGO_PKG_VERSION"),
        rounds,
        if rounds == 1 { "" } else { "s" }
    );

    for round in 0..rounds {
        let block_height = round + 1;
        let block_time = wallclock_secs().saturating_add(round);

        let decided = run_single_validator(bridge.as_ref(), parent).await?;
        println!(
            "round {}: decided {} via in-memory bridge",
            block_height,
            short_hash(&decided)
        );

        let report = coordinator.tick(TickInput {
            block_height,
            block_time,
            mark: MarkPrice(100),
            account_snapshots: &[],
            vault_total_assets: coordinator.vault().total_assets().0,
        });
        print_tick_report(&report);

        parent = decided;
    }

    Ok(())
}

/// Drive `rounds` consensus decisions through the **production-shape**
/// actor-engine loop with a Reth-backed [`LiveRethEvmBridge`].
/// Stage 13c path — the real boot ceremony.
///
/// Flow:
///   1. Spin up a Reth `EthereumNode` with `OpenHlExecutorBuilder`
///      (so the EVM has our custom CLOB precompiles registered).
///   2. Construct a [`LiveRethEvmBridge`] against the node's provider.
///   3. Bootstrap a consensus [`openhl_consensus::OpenHlNode`] with a
///      fresh Ed25519 keypair and a single-validator set.
///   4. `node.start().await` — spawns the Malachite actor system.
///   5. `take_channels().await` — get the engine's `AppMsg` channels.
///   6. Spawn `run_engine_app(bridge, channels, validator_set, rounds)`
///      to drive `rounds` decisions then exit.
///   7. Clean shutdown of the consensus node.
#[allow(clippy::too_many_lines)] // 6-step boot ceremony — flat for readability
#[allow(clippy::too_many_arguments)] // CLI surface — clap collects + forwards
async fn run_reth_devnet(
    rounds: u64,
    moniker: String,
    data_dir: Option<PathBuf>,
    chain_spec_path: Option<PathBuf>,
    validators_path: Option<PathBuf>,
    listen_addr: Option<String>,
    rpc_bind: Option<String>,
) -> eyre::Result<()> {
    println!(
        "openhl v{} — driving {} reth-backed decision{}",
        env!("CARGO_PKG_VERSION"),
        rounds,
        if rounds == 1 { "" } else { "s" }
    );

    // 1. Reth boot — production path (`init_db` + `with_database` +
    //    `with_launch_context`, no `test-utils` feature).
    let data_dir_path = resolve_data_dir(data_dir.as_ref())?;
    std::fs::create_dir_all(&data_dir_path)?;
    let reth_db_path = data_dir_path.join("reth");
    std::fs::create_dir_all(&reth_db_path)?;

    println!("[1/6] booting Reth EthereumNode with OpenHlExecutorBuilder…");
    println!("      data dir         = {}", data_dir_path.display());
    println!("      Reth MDBX dir    = {}", reth_db_path.display());

    let chain_spec = if let Some(path) = chain_spec_path.as_deref() {
        println!("      chain spec       = {} (loaded)", path.display());
        load_chain_spec(path)?
    } else {
        println!("      chain spec       = (embedded dev chain id 2600)");
        dev_chain_spec()
    };

    // Stage 13k: optional `--rpc-bind <addr:port>` overrides Reth's
    // default RPC bind (127.0.0.1:8545). Parse <ip>:<port>; supports
    // IPv4 (e.g. `0.0.0.0:8545`) and bracketed IPv6 (e.g. `[::1]:8545`).
    // `RpcServerArgs` exposes `http_addr`/`http_port` as public fields,
    // so we mutate the default rather than using a builder method.
    let mut rpc_args = reth_node_core::args::RpcServerArgs::default();
    if let Some(spec) = rpc_bind.as_deref() {
        let (ip, port) = parse_socket_spec(spec)?;
        println!("      rpc bind         = {ip}:{port}");
        rpc_args.http_addr = ip;
        rpc_args.http_port = port;
        // Stage 13l: overriding `--rpc-bind` is the signal that this
        // process shares a host with other openhl nodes. Reth's WS
        // (8546) and auth-RPC (8551) defaults would collide between
        // peers, so bind both to ephemeral ports (port 0 — OS picks).
        // Operators who need stable WS/auth ports can switch to
        // explicit flags later.
        rpc_args.ws_addr = ip;
        rpc_args.ws_port = 0;
        rpc_args.auth_addr = ip;
        rpc_args.auth_port = 0;
        println!("      ws / auth bind   = {ip}:ephemeral (multi-node-safe)");
    } else {
        println!("      rpc bind         = (Reth default 127.0.0.1:8545)");
    }
    let node_config = NodeConfig::test()
        .dev()
        .with_chain(chain_spec.clone())
        .with_datadir_args(DatadirArgs {
            datadir: MaybePlatformPath::<DataDirPath>::from(reth_db_path.clone()),
            ..Default::default()
        })
        .with_rpc(rpc_args);
    let runtime = Runtime::test();

    // `init_db` opens an existing MDBX database at the path or creates
    // a fresh one if none exists — idempotent across restarts.
    let db = Arc::new(init_db(&reth_db_path, DatabaseArguments::default())?);

    let RethNodeHandle {
        node,
        node_exit_future: _,
    } = NodeBuilder::new(node_config)
        .with_database(db)
        .with_launch_context(runtime)
        .with_types::<EthereumNode>()
        .with_components(EthereumNode::components().executor(OpenHlExecutorBuilder::default()))
        .with_add_ons(EthereumAddOns::default())
        .launch()
        .await?;
    println!(
        "      Reth up; chain id = {}",
        node.chain_spec().chain.id()
    );

    // 2. LiveRethEvmBridge against the live node's provider.
    println!("[2/6] constructing LiveRethEvmBridge against node provider…");
    // Capture the genesis hash *before* moving chain_spec into the bridge —
    // run_engine_app needs it as the initial parent of its first decision
    // (Stage 13d gap closure).
    let genesis_hash_bytes: [u8; 32] = chain_spec.genesis_hash().into();
    let genesis_parent = BlockHash(genesis_hash_bytes);
    let bridge = Arc::new(LiveRethEvmBridge::new(node.provider.clone(), chain_spec));
    println!(
        "      genesis hash     = 0x{}…{}",
        hex_prefix(&genesis_hash_bytes, 4),
        hex_suffix(&genesis_hash_bytes, 4),
    );

    // Stage 13g+13i: load any prior bridge state and derive both the
    // initial parent hash AND the initial consensus height.
    let bridge_state_path = data_dir_path.join("bridge").join("state.json");
    let (resume_parent, prior_decisions) = if bridge_state_path.exists() {
        let bytes = std::fs::read(&bridge_state_path)?;
        let snapshot: BridgeSnapshot = serde_json::from_slice(&bytes)
            .map_err(|e| eyre::eyre!("malformed bridge snapshot at {bridge_state_path:?}: {e}"))?;
        let head_for_print = snapshot.head;
        let chain_len = snapshot.chain.len();
        bridge.load_snapshot(snapshot);
        println!(
            "      loaded snapshot  = {} block(s); head = {}",
            chain_len,
            head_for_print.map_or_else(|| "(none)".to_string(), |h| short_b256(&h)),
        );
        (
            head_for_print.map(|b| BlockHash(b.into())),
            u64::try_from(chain_len).unwrap_or(u64::MAX),
        )
    } else {
        println!("      no prior snapshot (fresh chain)");
        (None, 0)
    };
    let initial_parent_for_consensus = resume_parent.unwrap_or(genesis_parent);
    // Stage 13i: consensus height = prior decisions + 1, so log lines
    // and (future) multi-validator peers see a continuous height
    // sequence rather than restarting at 1 every run.
    let initial_height_for_consensus =
        openhl_consensus::types::OpenHlHeight(prior_decisions.saturating_add(1));

    // 3. Consensus node with single-validator set.
    //    Stage 13h: load the validator key from disk if present,
    //    otherwise generate fresh and write it. With this in place
    //    consecutive runs use the same validator identity, which is
    //    a prerequisite for Malachite WAL reuse (Stage 13h+).
    let key_path = data_dir_path.join("validator-key.json");
    let (private, key_status) = if key_path.exists() {
        let bytes = std::fs::read(&key_path)?;
        let file: OpenHlPrivateKeyFile = serde_json::from_slice(&bytes)
            .map_err(|e| eyre::eyre!("malformed validator key at {key_path:?}: {e}"))?;
        (file.into_private_key(), "loaded")
    } else {
        let fresh = PrivateKey::generate(OsRng);
        let file = OpenHlPrivateKeyFile::from_private_key(&fresh);
        if let Some(parent) = key_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&key_path, serde_json::to_vec_pretty(&file)?)?;
        // Make the key file owner-readable only — minor hardening so a
        // shared-filesystem mishap doesn't surface the validator's
        // secret to other users on the host.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
        }
        (fresh, "generated")
    };
    let public = private.public_key();
    println!("[3/6] {key_status} validator key from {}", key_path.display());

    // Stage 13j: validator set — load from file if given, else
    // construct single-validator set from the loaded key (preserves
    // pre-13j behavior).
    // Stage 13l: also derive the libp2p dial list — every peer entry
    // with a `peer_multiaddr` that isn't *us*.
    let (validator_set, persistent_peers) = if let Some(path) = validators_path.as_deref() {
        let LoadedValidatorSet {
            set,
            peer_multiaddrs,
            self_index,
        } = load_validator_set(path, &public)?;
        println!(
            "      validator set    = {} ({} validator{})",
            path.display(),
            set.validators().len(),
            if set.validators().len() == 1 { "" } else { "s" }
        );
        // Log advertised peer multiaddrs (Stage 13k) so operators can
        // sanity-check the network layout.
        for (idx, addr) in peer_multiaddrs.iter().enumerate() {
            let marker = if idx == self_index { " (self)" } else { "" };
            match addr {
                Some(a) => println!("        peer[{idx}].multiaddr = {a}{marker}"),
                None => println!("        peer[{idx}].multiaddr = (unset){marker}"),
            }
        }
        // Stage 13l: build the dial list — every non-self entry that
        // has a multiaddr set. Self is excluded to avoid a libp2p
        // self-dial; entries without a multiaddr are skipped (they're
        // valid validators we just can't reach until they advertise).
        let dial_list: Vec<String> = peer_multiaddrs
            .iter()
            .enumerate()
            .filter_map(|(idx, addr)| {
                if idx == self_index {
                    None
                } else {
                    addr.clone()
                }
            })
            .collect();
        (set, dial_list)
    } else {
        let digest = Sha256::digest(public.as_bytes());
        let mut addr_bytes = [0u8; 20];
        addr_bytes.copy_from_slice(&digest[12..32]);
        let address = openhl_consensus::types::OpenHlAddress(addr_bytes);
        println!("      validator set    = (single-validator default)");
        let set = openhl_consensus::types::OpenHlValidatorSet::new(vec![
            openhl_consensus::types::OpenHlValidator::new(address, public, 1),
        ]);
        (set, Vec::new())
    };
    // Consensus home dir: a subdir of the resolved data dir. Persists
    // across restarts so the Malachite WAL has a stable location (real
    // WAL load/save remains Stage 13g work).
    let consensus_home = data_dir_path.join("consensus");
    std::fs::create_dir_all(&consensus_home)?;
    println!("      consensus home   = {}", consensus_home.display());
    let mut consensus_node = openhl_consensus::OpenHlNode::new(
        private,
        validator_set.clone(),
        consensus_home,
        moniker.clone(),
    );
    if let Some(ref multiaddr) = listen_addr {
        consensus_node = consensus_node.with_listen_addr(multiaddr.clone());
        println!("      listen addr      = {multiaddr}");
    } else {
        println!("      listen addr      = (ephemeral /ip4/127.0.0.1/tcp/0)");
    }
    // Stage 13l: forward the derived dial list. Empty (single-validator
    // path, or no peer_multiaddrs in the validator file) preserves
    // pre-13l behavior.
    if persistent_peers.is_empty() {
        println!("      persistent peers = (none)");
    } else {
        println!("      persistent peers = {} peer(s)", persistent_peers.len());
        for (idx, peer) in persistent_peers.iter().enumerate() {
            println!("        dial[{idx}]            = {peer}");
        }
        consensus_node = consensus_node.with_persistent_peers(persistent_peers.clone());
    }
    println!("      moniker          = {moniker}");

    // 4. Start the Malachite actor system.
    println!("[4/6] starting Malachite actor system…");
    let handle = consensus_node.start().await?;

    // 5. Take the engine's AppMsg channels.
    println!("[5/6] taking engine AppMsg channels…");
    let channels = handle
        .take_channels()
        .await
        .ok_or_else(|| eyre::eyre!("channels already taken"))?;

    // 6. Drive run_engine_app for N decisions, seeded with Reth's
    //    actual genesis hash so the first `build_payload` finds its
    //    parent block in the database.
    println!(
        "[6/6] driving run_engine_app for {rounds} decision(s) starting at height {}…",
        initial_height_for_consensus.0
    );
    let bridge_for_engine = bridge.clone();
    let validator_set_for_engine = validator_set.clone();
    let rounds_usize = usize::try_from(rounds)
        .map_err(|_| eyre::eyre!("rounds value too large for usize on this target"))?;
    let app_task = tokio::spawn(async move {
        run_engine_app(
            bridge_for_engine,
            channels,
            validator_set_for_engine,
            initial_parent_for_consensus,
            initial_height_for_consensus,
            rounds_usize,
        )
        .await
    });

    #[allow(clippy::duration_suboptimal_units)]
    let timeout = std::time::Duration::from_secs(60);
    let app_result = tokio::time::timeout(timeout, app_task)
        .await
        .map_err(|_| eyre::eyre!("run_engine_app timed out after 60s"))?
        .map_err(|e| eyre::eyre!("run_engine_app task panicked: {e}"))?;

    match app_result {
        Ok(decisions) => {
            for (idx, hash) in decisions.iter().enumerate() {
                println!(
                    "decision {}: {} via reth-backed bridge",
                    idx + 1,
                    short_hash(hash)
                );
            }
            println!(
                "reth-devnet complete: {} decision(s) committed",
                decisions.len()
            );
        }
        Err(e) => {
            println!("run_engine_app halted with error: {e}");
        }
    }

    // Stage 13g: persist the bridge's final committed-chain state so
    // the next run can resume from it. Saved as JSON for easy
    // inspection (e.g., `jq < state.json '.head'`).
    let final_snapshot = bridge.snapshot();
    let chain_len = final_snapshot.chain.len();
    if let Some(parent) = bridge_state_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &bridge_state_path,
        serde_json::to_vec_pretty(&final_snapshot)?,
    )?;
    println!(
        "persisted bridge snapshot ({} block(s)) → {}",
        chain_len,
        bridge_state_path.display()
    );

    // Clean shutdown regardless of the run_engine_app result above —
    // proves the teardown path works even when block production stops
    // short.
    println!("shutting down consensus actor system…");
    handle.kill(None).await?;
    println!("reth-devnet teardown complete");

    Ok(())
}

/// Load a `ChainSpec` from a JSON file containing an
/// `alloy_genesis::Genesis`. The file format is the same one the
/// embedded `dev_chain_spec` uses inline.
fn load_chain_spec(path: &Path) -> eyre::Result<Arc<ChainSpec>> {
    let bytes = std::fs::read(path)
        .map_err(|e| eyre::eyre!("failed to read chain spec at {}: {e}", path.display()))?;
    let genesis: Genesis = serde_json::from_slice(&bytes)
        .map_err(|e| eyre::eyre!("malformed chain spec at {}: {e}", path.display()))?;
    Ok(Arc::new(genesis.into()))
}

/// Load a validator set from a JSON file. The locally-loaded validator
/// key's public key MUST appear in the set — otherwise the node
/// refuses to sign on behalf of an identity the network doesn't
/// recognize.
/// Loaded validator-set result. `peer_multiaddrs[i]` is the
/// `peer_multiaddr` entry for validator `i` (parallel to
/// `set.validators()`); `self_index` is the position of *our*
/// validator in the set — used to filter our own entry out of the
/// libp2p dial list (Stage 13l).
struct LoadedValidatorSet {
    set: openhl_consensus::types::OpenHlValidatorSet,
    peer_multiaddrs: Vec<Option<String>>,
    self_index: usize,
}

fn load_validator_set(
    path: &Path,
    our_pubkey: &informalsystems_malachitebft_signing_ed25519::PublicKey,
) -> eyre::Result<LoadedValidatorSet> {
    let bytes = std::fs::read(path)
        .map_err(|e| eyre::eyre!("failed to read validator set at {}: {e}", path.display()))?;
    let file: ValidatorSetFile = serde_json::from_slice(&bytes)
        .map_err(|e| eyre::eyre!("malformed validator set at {}: {e}", path.display()))?;
    if file.validators.is_empty() {
        return Err(eyre::eyre!("validator set at {} is empty", path.display()));
    }
    let our_pubkey_bytes = our_pubkey.as_bytes();

    let mut self_index: Option<usize> = None;
    let mut built = Vec::with_capacity(file.validators.len());
    let mut peer_multiaddrs = Vec::with_capacity(file.validators.len());
    for (idx, entry) in file.validators.iter().enumerate() {
        if entry.voting_power == 0 {
            return Err(eyre::eyre!(
                "validator with pubkey_hex={} has voting_power=0; must be > 0",
                entry.pubkey_hex
            ));
        }
        let raw = hex::decode(&entry.pubkey_hex)
            .map_err(|e| eyre::eyre!("invalid hex in pubkey_hex={}: {e}", entry.pubkey_hex))?;
        let bytes: [u8; 32] = raw
            .try_into()
            .map_err(|v: Vec<u8>| eyre::eyre!("pubkey_hex must decode to 32 bytes, got {}", v.len()))?;
        // PublicKey::from_bytes panics on invalid Ed25519 points; go
        // through `VerificationKey::try_from` so malformed entries
        // surface as a graceful eyre error instead.
        let vk = ed25519_consensus::VerificationKey::try_from(bytes).map_err(|e| {
            eyre::eyre!(
                "pubkey_hex={} is not a valid Ed25519 public key: {e}",
                entry.pubkey_hex
            )
        })?;
        let pubkey = informalsystems_malachitebft_signing_ed25519::PublicKey::new(vk);
        if pubkey.as_bytes() == our_pubkey_bytes {
            if let Some(prior) = self_index {
                return Err(eyre::eyre!(
                    "validator set at {} lists our public key twice (positions {prior} and {idx})",
                    path.display()
                ));
            }
            self_index = Some(idx);
        }
        let digest = Sha256::digest(pubkey.as_bytes());
        let mut addr = [0u8; 20];
        addr.copy_from_slice(&digest[12..32]);
        built.push(openhl_consensus::types::OpenHlValidator::new(
            openhl_consensus::types::OpenHlAddress(addr),
            pubkey,
            entry.voting_power,
        ));
        peer_multiaddrs.push(entry.peer_multiaddr.clone());
    }
    let self_index = self_index.ok_or_else(|| {
        eyre::eyre!(
            "loaded validator key's public key does not appear in {}; \
             refusing to start (won't sign as an unrecognized identity)",
            path.display()
        )
    })?;
    Ok(LoadedValidatorSet {
        set: openhl_consensus::types::OpenHlValidatorSet::new(built),
        peer_multiaddrs,
        self_index,
    })
}

/// Parse an `<addr>:<port>` socket spec for `--rpc-bind`. Accepts:
///   `127.0.0.1:8545` (IPv4)
///   `0.0.0.0:8545`   (IPv4 all-interfaces)
///   `[::1]:8545`     (IPv6, brackets required to disambiguate `:` in addr)
fn parse_socket_spec(spec: &str) -> eyre::Result<(IpAddr, u16)> {
    let (addr_str, port_str) = if let Some(rest) = spec.strip_prefix('[') {
        // Bracketed IPv6: `[<v6>]:<port>`
        let (v6, after) = rest
            .split_once(']')
            .ok_or_else(|| eyre::eyre!("malformed IPv6 spec `{spec}`: missing closing `]`"))?;
        let port = after
            .strip_prefix(':')
            .ok_or_else(|| eyre::eyre!("malformed IPv6 spec `{spec}`: expected `:` after `]`"))?;
        (v6, port)
    } else {
        spec.rsplit_once(':')
            .ok_or_else(|| eyre::eyre!("malformed socket spec `{spec}`: expected `<addr>:<port>`"))?
    };
    let addr: IpAddr = addr_str
        .parse()
        .map_err(|e| eyre::eyre!("invalid IP `{addr_str}` in `{spec}`: {e}"))?;
    let port: u16 = port_str
        .parse()
        .map_err(|e| eyre::eyre!("invalid port `{port_str}` in `{spec}`: {e}"))?;
    Ok((addr, port))
}

/// Minimal post-merge dev genesis. Chain ID 2600 mirrors the upstream
/// reth custom-dev-node example so behaviour can be compared 1:1 if
/// needed. Same shape `crates/evm` uses in its integration tests.
fn dev_chain_spec() -> Arc<ChainSpec> {
    let genesis_json = r#"{
        "nonce": "0x42",
        "timestamp": "0x0",
        "extraData": "0x5343",
        "gasLimit": "0x5208",
        "difficulty": "0x400000000",
        "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "coinbase": "0x0000000000000000000000000000000000000000",
        "alloc": {},
        "number": "0x0",
        "gasUsed": "0x0",
        "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "config": {
            "ethash": {},
            "chainId": 2600,
            "homesteadBlock": 0,
            "eip150Block": 0,
            "eip155Block": 0,
            "eip158Block": 0,
            "byzantiumBlock": 0,
            "constantinopleBlock": 0,
            "petersburgBlock": 0,
            "istanbulBlock": 0,
            "berlinBlock": 0,
            "londonBlock": 0,
            "terminalTotalDifficulty": 0,
            "terminalTotalDifficultyPassed": true,
            "shanghaiTime": 0
        }
    }"#;
    let genesis: Genesis = serde_json::from_str(genesis_json).expect("dev genesis parses");
    Arc::new(genesis.into())
}

fn wallclock_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn short_hash(h: &BlockHash) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(10);
    for b in &h.0[..4] {
        let _ = write!(s, "{b:02x}");
    }
    s.push('…');
    s
}

fn hex_prefix(bytes: &[u8; 32], n: usize) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(n * 2);
    for b in &bytes[..n] {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn hex_suffix(bytes: &[u8; 32], n: usize) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(n * 2);
    for b in &bytes[32 - n..] {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn short_b256(h: &alloy_primitives::B256) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(10);
    for b in &h.0[..4] {
        let _ = write!(s, "{b:02x}");
    }
    s.push('…');
    s
}

fn print_tick_report(report: &TickReport) {
    print!(
        "  tick(height={}, time={}): ",
        report.block_height, report.block_time
    );
    match &report.oracle {
        Some(Ok(p)) => print!("oracle=Ok(idx={}, feeds={}) ", p.index.0, p.feeds_used),
        Some(Err(e)) => print!("oracle=Err({e:?}) "),
        None => print!("oracle=skip "),
    }
    print!(
        "scan(records={}, dep={}, wd={}, deficit={}) ",
        report.liquidation.records.len(),
        report.liquidation.fund_deposits,
        report.liquidation.fund_withdrawals,
        report.liquidation.unfilled_deficit
    );
    match &report.adl {
        Some(a) => print!(
            "adl(records={}, absorbed={}, remaining={}) ",
            a.records.len(),
            a.deficit_absorbed,
            a.deficit_remaining,
        ),
        None => print!("adl=skip "),
    }
    println!(
        "vault(shares={}, assets={}, price_bps={:?})",
        report.vault_total_shares, report.vault_total_assets, report.vault_share_price_bps
    );
}
