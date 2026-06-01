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

mod rpc;
use crate::rpc::OpenHlInfoApiServer as _;

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_genesis::Genesis;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use informalsystems_malachitebft_app::node::{Node, NodeHandle};
use informalsystems_malachitebft_signing_ed25519::PrivateKey;
use openhl_consensus::run_engine_app;
use openhl_consensus::run_single_validator;
use openhl_consensus::OpenHlPrivateKeyFile;
use openhl_clob::{AccountId as ClobAccountId, Order, OrderId, OrderType, Price, Qty, Side};
use openhl_evm::{BridgeSnapshot, InMemoryEvmBridge, LiveRethEvmBridge, OpenHlExecutorBuilder};
use k256::ecdsa::{signature::Signer, SigningKey};
use openhl_funding::{IndexPrice, MarkPrice, Notional, PositionSize};
use openhl_liquidation::{AccountSnapshot, CloseOutcomeKind};
use openhl_node::{CoordinatorSnapshot, OpenHlNode, OpenHlNodeConfig, TickInput, TickReport};
use openhl_oracle::{FeedId, PriceObservation, PublisherKey, Signature as OracleSignature};
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
    // Stage 19a: enable the HTTP JSON-RPC server. Reth's CLI flips
    // this on via `--http` (and via the `--dev` shortcut); when we
    // construct `RpcServerArgs` programmatically the field stays
    // `false` and `extend_rpc_modules` would have nowhere to plug
    // the `openhl_*` namespace into.
    rpc_args.http = true;
    if let Some(spec) = rpc_bind.as_deref() {
        let (ip, port) = parse_socket_spec(spec)?;
        println!("      rpc bind         = {ip}:{port}");
        rpc_args.http_addr = ip;
        rpc_args.http_port = port;
        // Stage 13l/13n: overriding `--rpc-bind` is the signal that this
        // process shares a host with other openhl nodes. Reth's WS
        // (8546) and auth-RPC (8551) defaults would collide between
        // peers, so bind both to ephemeral ports (port 0 — OS picks).
        // The IPC endpoint at `/tmp/reth.ipc` is a single global path
        // shared across processes — disable it entirely to avoid the
        // collision (we don't use IPC yet anyway).
        // Operators who need stable WS/auth ports or IPC can switch
        // to explicit flags later.
        rpc_args.ws_addr = ip;
        rpc_args.ws_port = 0;
        rpc_args.auth_addr = ip;
        rpc_args.auth_port = 0;
        rpc_args.ipcdisable = true;
        println!("      ws / auth bind   = {ip}:ephemeral (multi-node-safe)");
        println!("      ipc              = disabled (multi-node-safe)");
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

    // Stage 19a: register the `openhl_*` JSON-RPC namespace on
    // Reth's RPC server. The bridge is constructed AFTER Reth
    // launches (it needs `node.provider`), so we pre-create a
    // bridge cell and fill it post-launch; the RPC handler reads
    // through the cell and returns a "bridge not ready" error
    // during the narrow window before it's filled.
    let rpc_bridge_cell = rpc::new_bridge_cell();
    let rpc_bridge_cell_for_hook = rpc_bridge_cell.clone();
    let RethNodeHandle {
        node,
        node_exit_future: _,
    } = NodeBuilder::new(node_config)
        .with_database(db)
        .with_launch_context(runtime)
        .with_types::<EthereumNode>()
        .with_components(EthereumNode::components().executor(OpenHlExecutorBuilder::default()))
        .with_add_ons(EthereumAddOns::default())
        .extend_rpc_modules(move |ctx| {
            let server = rpc::OpenHlInfoServer::new(rpc_bridge_cell_for_hook);
            ctx.modules.merge_configured(server.into_rpc())?;
            println!("      openhl rpc       = openhl_* namespace registered");
            Ok(())
        })
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
    // Stage 17l → 17m: thread the full margin params from the node
    // config into the bridge. Same rate the coordinator constructed
    // below at L668 picks up from `hyperliquid_default()`; bridge
    // uses `params.initial_margin_bps` for withdraw and the full
    // params for `margin_health()`.
    let liquidation_params = OpenHlNodeConfig::hyperliquid_default().liquidation_params;
    let bridge = Arc::new(
        LiveRethEvmBridge::new(node.provider.clone(), chain_spec)
            .with_liquidation_params(liquidation_params),
    );
    // Stage 19a: now that the bridge exists, fill the RPC cell so
    // openhl_* methods start resolving against it. Any client that
    // hit an openhl_* method before this point got a "bridge not
    // ready" error rather than a stale or zero answer.
    rpc::install_bridge(&rpc_bridge_cell, bridge.clone());
    println!(
        "      margin params    = {} bps initial / {} bps maintenance / {} bps fee",
        liquidation_params.initial_margin_bps,
        liquidation_params.maintenance_margin_bps,
        liquidation_params.liquidation_fee_bps,
    );
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

    // Stage 14a: integration coordinator. One `OpenHlNode` per
    // running validator. Every committed block triggers a `tick`.
    // Stage 14e: if a prior run persisted a coordinator snapshot,
    // load it so the insurance fund balance, vault, and oracle
    // refresh marker carry across restart. Mirrors the bridge
    // snapshot pattern (Stage 13g).
    // Stage 15a: dev override — shrink the funding interval from
    // Hyperliquid's 1 hour to 1 second so the clock fires per block
    // in a 3-round test. Production deployments leave it at 3600.
    let mut node_config = OpenHlNodeConfig::hyperliquid_default();
    node_config.funding_params.interval_secs = 1;
    let mut coordinator_inner = OpenHlNode::new(node_config);
    let coordinator_state_path = data_dir_path.join("coordinator").join("state.json");
    if coordinator_state_path.exists() {
        let bytes = std::fs::read(&coordinator_state_path)?;
        let snap: CoordinatorSnapshot = serde_json::from_slice(&bytes).map_err(|e| {
            eyre::eyre!("malformed coordinator snapshot at {coordinator_state_path:?}: {e}")
        })?;
        coordinator_inner.load_snapshot(snap);
        println!(
            "      loaded coordinator snapshot: fund={}, vault_shares={}, vault_assets={}, last_oracle_refresh_at={:?}",
            snap.insurance_fund_balance,
            snap.vault_total_shares,
            snap.vault_total_assets,
            snap.last_oracle_refresh_at,
        );
    } else {
        println!("      no prior coordinator snapshot (fresh state)");
    }
    let coordinator = Arc::new(Mutex::new(coordinator_inner));

    // Stage 14b: register synthetic publishers and seed the oracle
    // so the per-block refresh has feeds to aggregate. In production
    // these come from external CEX publishers; here we generate them
    // in-process with deterministic seeds (same code on every
    // validator → identical signed bytes → matching aggregation).
    let publishers: Vec<SyntheticPublisher> = SYNTHETIC_FEEDS
        .iter()
        .map(|(feed_id, seed, _)| SyntheticPublisher::from_seed(*feed_id, *seed))
        .collect();
    {
        let mut node = coordinator.lock().expect("coordinator mutex poisoned");
        for pub_ in &publishers {
            node.register_publisher(pub_.feed, pub_.public_key);
        }
    }
    println!(
        "      oracle publishers = {} synthetic feed(s) registered",
        publishers.len()
    );
    let publishers = Arc::new(publishers);

    // Stage 17h: seed five accounts via real CLOB fills, all
    // trading at the same fair price (100). Replaces the Stage 17a
    // single-MM seed (account 999 taking absurd off-market orders).
    // The cascade-inducing PnL now comes from the mark drift in
    // `seed_mark_orders` — exactly how real markets generate
    // winners and losers.
    //
    // Cast: Alice (10), Bob (20), Carol (30) are demo traders going
    // long; Dave (40), Eve (50) are makers taking the other side.
    // After the seed sequence, the mark book opens at midpoint 96
    // (Buy@95 / Sell@97), and the deposit phase funds collateral
    // (200, 50, 100, 300, 200) such that on the first tick:
    //   - Bob is Liquidatable (scan target),
    //   - Carol is Underwater (ADL target),
    //   - Dave + Eve are ADL-eligible counterparties (Safe + positive uPnL),
    //   - Alice rides through Safe.
    //
    // Scan and ADL never share an account in a single tick — the
    // disjoint-target invariant the `apply records` logic below
    // relies on is preserved across the retire-the-MM rewrite.
    let accounts_already_loaded = !bridge.accounts_snapshot().is_empty();
    if accounts_already_loaded {
        println!(
            "      accounts             = {} loaded from bridge snapshot",
            bridge.accounts_snapshot().len(),
        );
        // The mark-providing token orders are not part of the
        // bridge's persisted state (the CLOB book itself doesn't
        // snapshot today), so re-seed them on every boot.
        seed_mark_orders(&bridge);
        println!("      mark book            = re-seeded (Buy@95 / Sell@97)");
    } else {
        let fills_count = seed_accounts_via_fills(&bridge);
        println!(
            "      seed fills           = {} (Alice/Bob/Carol take longs from Dave + Eve @ price 100)",
            fills_count,
        );
        seed_mark_orders(&bridge);
        println!("      mark book            = Buy@95 / Sell@97 (mid 96 — 4-point drift)");
        // Stage 17b: deposit collateral via the bridge's deposit
        // primitive instead of mutating the account map directly.
        // This is the bridge-layer hook an EVM-side
        // `deposit(account, amount)` instruction would call once
        // we have a real on-chain collateral flow.
        for (id, coll) in [(10, 200), (20, 50), (30, 100), (40, 300), (50, 200)] {
            let new_balance = bridge.deposit(ClobAccountId(id), coll);
            println!(
                "      deposit              = account {id} → collateral {}",
                new_balance.0,
            );
        }
        println!(
            "      accounts             = {} (3 traders + 2 makers, no MM)",
            bridge.accounts_snapshot().len(),
        );
    }

    let coordinator_for_hook = coordinator.clone();
    let publishers_for_hook = publishers.clone();
    let bridge_for_hook = bridge.clone();
    let app_task = tokio::spawn(async move {
        run_engine_app(
            bridge_for_engine,
            channels,
            validator_set_for_engine,
            initial_parent_for_consensus,
            initial_height_for_consensus,
            rounds_usize,
            move |hash, height, block_time| {
                let mut node = coordinator_for_hook
                    .lock()
                    .map_err(|_| eyre::eyre!("coordinator mutex poisoned"))?;

                // Stage 14b: ingest one fresh signed observation per
                // synthetic publisher before the tick. Prices are the
                // hardcoded per-feed values from SYNTHETIC_FEEDS; the
                // timestamp is the same `block_time` the tick will
                // see, so the staleness window is irrelevant. Errors
                // are non-fatal (we log them and let `tick` decide
                // whether the resulting feed count is enough to
                // aggregate) — this matches the production pattern
                // where a bridge would never halt the chain on a
                // single feed's ingestion failure.
                for (publisher, &(_, _, price)) in
                    publishers_for_hook.iter().zip(SYNTHETIC_FEEDS.iter())
                {
                    let obs = publisher.sign(IndexPrice(price), block_time);
                    if let Err(e) = node.ingest_signed_observation(obs, block_time) {
                        tracing::warn!(
                            "stage 14b: ingest_signed_observation failed for feed {}: {e:?}",
                            publisher.feed.0,
                        );
                    }
                }

                let vault_total_assets = node.vault().total_assets().0;
                // Stage 14c: live CLOB mark from the bridge. Falls back
                // to MarkPrice(100) only when the book is one-sided or
                // empty (e.g., if every order has been crossed out).
                // The fallback keeps the tick running with a stable
                // value rather than failing on a transient empty book.
                let (mark, mark_source) = match bridge_for_hook.current_mark() {
                    Some(m) => (m, "clob"),
                    None => (MarkPrice(100), "stub-empty-book"),
                };

                // Stage 16c: read the bridge-owned accounts into a
                // tick-input slice. `Account` and `AccountSnapshot`
                // are structurally identical (same fields, same
                // types); the conversion is a field-by-field copy.
                let snapshots: Vec<AccountSnapshot> = bridge_for_hook
                    .accounts_snapshot()
                    .into_iter()
                    .map(|a| AccountSnapshot {
                        account: a.account,
                        position_size: a.position_size,
                        avg_entry: a.avg_entry,
                        collateral: a.collateral,
                    })
                    .collect();

                let report = node.tick(TickInput {
                    block_height: height.0,
                    block_time,
                    mark,
                    account_snapshots: &snapshots,
                    vault_total_assets,
                });
                println!("  mark = {} ({mark_source})", mark.0);
                print_tick_report(&report);

                // Stage 15b → 16c: apply funding settlements back to
                // the bridge-owned account map. The bridge is the
                // sole source of truth for per-account state — every
                // delta lands there.
                if let Some(ref ft) = report.funding {
                    bridge_for_hook.with_accounts_mut(|accts| {
                        for settlement in &ft.settlements {
                            if let Some(acct) = accts.get_mut(&settlement.account) {
                                let prev = acct.collateral.0;
                                let next = prev.saturating_add(settlement.delta.0);
                                acct.collateral = Notional(next);
                                println!(
                                    "  funding apply: account {} collateral {} → {} (Δ={})",
                                    acct.account.0, prev, next, settlement.delta.0,
                                );
                            }
                        }
                    });
                }

                // Stage 15d → 16c: liquidation + ADL records also
                // go through the bridge. Same disjoint-target
                // invariant: the synthetic seed is designed so
                // scan and ADL never target the same account from
                // one tick's snapshot.
                let has_liq = !report.liquidation.records.is_empty();
                let has_adl = report
                    .adl
                    .as_ref()
                    .is_some_and(|a| !a.records.is_empty());
                if has_liq || has_adl {
                    bridge_for_hook.with_accounts_mut(|accts| {
                        for rec in &report.liquidation.records {
                            if let Some(acct) = accts.get_mut(&rec.close_order.account) {
                                let prev_coll = acct.collateral.0;
                                match rec.outcome {
                                    CloseOutcomeKind::Solvent(sc) => {
                                        acct.position_size = PositionSize(0);
                                        acct.collateral = Notional(sc.residual_to_account);
                                        println!(
                                            "  liquidation apply: account {} closed (solvent) coll {} → {} (fee {} to fund)",
                                            acct.account.0,
                                            prev_coll,
                                            sc.residual_to_account,
                                            sc.fee_to_fund,
                                        );
                                    }
                                    CloseOutcomeKind::Underwater(uc) => {
                                        acct.position_size = PositionSize(0);
                                        acct.collateral = Notional(0);
                                        println!(
                                            "  liquidation apply: account {} closed (underwater) coll {} → 0 (fund covered shortfall {}, fee {})",
                                            acct.account.0,
                                            prev_coll,
                                            uc.shortfall_to_fund,
                                            uc.fee_to_fund,
                                        );
                                    }
                                }
                            }
                        }
                        if let Some(ref ar) = report.adl {
                            for rec in &ar.records {
                                if let Some(acct) = accts.get_mut(&rec.close_order.account) {
                                    let prev_size = acct.position_size.0;
                                    let prev_coll = acct.collateral.0;
                                    let qty = i64::try_from(rec.close_order.qty.0)
                                        .unwrap_or(i64::MAX);
                                    let new_size = match rec.close_order.side {
                                        Side::Sell => prev_size.saturating_sub(qty),
                                        Side::Buy => prev_size.saturating_add(qty),
                                    };
                                    acct.position_size = PositionSize(new_size);
                                    acct.collateral =
                                        Notional(prev_coll.saturating_add(rec.pnl_paid));
                                    println!(
                                        "  adl apply: account {} size {} → {} coll {} → {} (pnl_paid={}, haircut={})",
                                        acct.account.0,
                                        prev_size,
                                        new_size,
                                        prev_coll,
                                        prev_coll.saturating_add(rec.pnl_paid),
                                        rec.pnl_paid,
                                        rec.haircut,
                                    );
                                }
                            }
                        }
                    });
                }

                let _ = hash; // hash currently unused; future stages may want it
                Ok(())
            },
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

    // Stage 14e: persist the coordinator's load-bearing state alongside
    // the bridge snapshot so the next boot resumes the insurance fund,
    // vault, and oracle refresh marker.
    let coordinator_snap = coordinator
        .lock()
        .map_err(|_| eyre::eyre!("coordinator mutex poisoned"))?
        .snapshot();
    if let Some(parent) = coordinator_state_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        &coordinator_state_path,
        serde_json::to_vec_pretty(&coordinator_snap)?,
    )?;
    println!(
        "persisted coordinator snapshot (fund={}, vault_shares={}, vault_assets={}) → {}",
        coordinator_snap.insurance_fund_balance,
        coordinator_snap.vault_total_shares,
        coordinator_snap.vault_total_assets,
        coordinator_state_path.display()
    );

    // Stage 16c: per-account state is now persisted inside the
    // bridge snapshot above. The standalone `accounts/state.json`
    // file from 15c is no longer written — the bridge owns the
    // map.
    println!(
        "      (accounts now persisted inside bridge snapshot — see `.accounts`)"
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

/// Stage 14b synthetic publisher. In real production, external
/// publishers (Binance/Coinbase/OKX) run their own signing services
/// and the bridge only ingests + verifies. For the v0 reference
/// devnet we generate observations in-process from deterministic
/// seeds so every validator computes identical signed bytes for the
/// same `(feed, price, timestamp)` tuple — that's the determinism the
/// oracle relies on.
///
/// The seed byte is repeated 32 times to form the secp256k1 secret
/// scalar; this is the same trick the oracle's `test_signing_key`
/// helper uses internally, lifted into the binary so the bridge-
/// simulator code stays out of the oracle crate's production
/// surface.
struct SyntheticPublisher {
    feed: FeedId,
    signing_key: SigningKey,
    public_key: PublisherKey,
}

impl SyntheticPublisher {
    fn from_seed(feed_id: u32, seed: u8) -> Self {
        assert!(seed != 0, "seed must be non-zero (scalar must be in 1..n)");
        let signing_key = SigningKey::from_slice(&[seed; 32])
            .expect("repeating seed forms a valid secp256k1 scalar");
        let compressed = signing_key.verifying_key().to_encoded_point(true);
        let mut bytes = [0u8; 33];
        bytes.copy_from_slice(compressed.as_bytes());
        Self {
            feed: FeedId(feed_id),
            signing_key,
            public_key: PublisherKey(bytes),
        }
    }

    fn sign(&self, price: IndexPrice, timestamp: u64) -> PriceObservation {
        let unsigned = PriceObservation::unsigned(self.feed, price, timestamp);
        let signed_bytes = unsigned.signed_bytes();
        let sig: k256::ecdsa::Signature = self.signing_key.sign(&signed_bytes);
        let mut sig_array = [0u8; 64];
        sig_array.copy_from_slice(&sig.to_bytes());
        PriceObservation {
            signature: OracleSignature(sig_array),
            ..unsigned
        }
    }
}

/// Spread of three publishers around a 102-cent anchor. Median is 102.
/// Trivial enough to verify visually in tick logs; rich enough that the
/// oracle's deviation filter is exercised (101/102/103 are all within
/// the 100-bps default deviation cap).
const SYNTHETIC_FEEDS: &[(u32, u8, u64)] = &[
    (1, 1, 101),
    (2, 2, 102),
    (3, 3, 103),
];

/// Stage 17h: five-account market scenario. Replaces the Stage 17a
/// single-MM seed (account 999 taking absurd off-market orders) with
/// a realistic shape: every trade in the seed sequence happens at
/// the same fair price (100), and the cascade-inducing PnL comes
/// from the mark moving in [`seed_mark_orders`] — exactly how real
/// markets generate winners and losers.
///
/// Sequence (deterministic across validators — both nodes execute
/// this identically on boot, every order at price 100):
///
///   1. Dave (40) Sell-limit 10 → Alice (10) Buy-market 10
///   2. Dave (40) Sell-limit 10 → Bob   (20) Buy-market 10
///   3. Dave (40) Sell-limit 30 → Carol (30) Buy-market 30
///   4. Eve  (50) Sell-limit 20 → Carol (30) Buy-market 20
///
/// Resulting positions (avg_entry = 100 for everyone):
///   - Alice (10): long 10  — safe trader with margin to spare
///   - Bob   (20): long 10  — thinly-collateralised, drops below
///                            maintenance once mark moves
///   - Carol (30): long 50  — large position, equity goes negative
///                            once mark moves (the underwater case)
///   - Dave  (40): short 50 — counterparty to rounds 1–3; ADL-
///                            eligible after mark drift gives him
///                            positive uPnL
///   - Eve   (50): short 20 — counterparty to round 4's tail;
///                            also ADL-eligible
///
/// At the post-boot mark (96, see [`seed_mark_orders`]) and the
/// post-boot collateral deposits (200, 50, 100, 300, 200) the
/// `MarginHealth` shakes out to:
///   - Alice: Safe          (MR ≈ 16.7%)
///   - Bob:   Liquidatable  (MR ≈ 1.0%, < 2% maintenance) — scan
///   - Carol: Underwater    (equity = −100)                — ADL
///   - Dave:  Safe          (MR ≈ 10.4%, +200 uPnL)        — ADL ctp
///   - Eve:   Safe          (MR ≈ 14.6%, +80  uPnL)        — ADL ctp
///
/// **Disjoint-target invariant.** Scan targets {Bob}, ADL targets
/// {Carol}, ADL counterparties are drawn from {Dave, Eve}. All
/// three sets are disjoint, so the per-tick `apply records` logic
/// in `main.rs` (which assumes scan and ADL don't double-touch one
/// account) keeps its precondition.
///
/// Returns the total number of fills produced.
#[allow(clippy::too_many_lines)]
fn seed_accounts_via_fills<P>(bridge: &LiveRethEvmBridge<P>) -> usize {
    let mut fills = 0;

    // Round 1 — Dave makes Sell 10 @ 100; Alice takes long 10.
    let r = bridge.submit_order(Order {
        id: OrderId(1001),
        account: ClobAccountId(40),
        side: Side::Sell,
        qty: Qty(10),
        order_type: OrderType::Limit { price: Price(100) },
    });
    fills += r.fills.len();
    let r = bridge.submit_order(Order {
        id: OrderId(1002),
        account: ClobAccountId(10),
        side: Side::Buy,
        qty: Qty(10),
        order_type: OrderType::Market,
    });
    fills += r.fills.len();

    // Round 2 — Dave makes another Sell 10 @ 100; Bob takes long 10.
    let r = bridge.submit_order(Order {
        id: OrderId(1003),
        account: ClobAccountId(40),
        side: Side::Sell,
        qty: Qty(10),
        order_type: OrderType::Limit { price: Price(100) },
    });
    fills += r.fills.len();
    let r = bridge.submit_order(Order {
        id: OrderId(1004),
        account: ClobAccountId(20),
        side: Side::Buy,
        qty: Qty(10),
        order_type: OrderType::Market,
    });
    fills += r.fills.len();

    // Round 3 — Dave makes Sell 30 @ 100; Carol takes long 30.
    // Dave is now short 50 total, all at avg 100.
    let r = bridge.submit_order(Order {
        id: OrderId(1005),
        account: ClobAccountId(40),
        side: Side::Sell,
        qty: Qty(30),
        order_type: OrderType::Limit { price: Price(100) },
    });
    fills += r.fills.len();
    let r = bridge.submit_order(Order {
        id: OrderId(1006),
        account: ClobAccountId(30),
        side: Side::Buy,
        qty: Qty(30),
        order_type: OrderType::Market,
    });
    fills += r.fills.len();

    // Round 4 — Eve makes Sell 20 @ 100; Carol tops up to long 50.
    let r = bridge.submit_order(Order {
        id: OrderId(1007),
        account: ClobAccountId(50),
        side: Side::Sell,
        qty: Qty(20),
        order_type: OrderType::Limit { price: Price(100) },
    });
    fills += r.fills.len();
    let r = bridge.submit_order(Order {
        id: OrderId(1008),
        account: ClobAccountId(30),
        side: Side::Buy,
        qty: Qty(20),
        order_type: OrderType::Market,
    });
    fills += r.fills.len();

    fills
}

/// Stage 17h: place two token resting orders so `current_mark()`
/// has a bid + ask to compute a midpoint. With trade-time price
/// 100 and these mark orders at 95/97 (mid 96), every position
/// opened by [`seed_accounts_via_fills`] picks up a 4-point uPnL
/// drift — what generates the cascade. The
/// `seed_accounts_via_fills` sequence exhausts all matched
/// liquidity, leaving the book empty, so neither of these orders
/// crosses anything.
fn seed_mark_orders<P>(bridge: &LiveRethEvmBridge<P>) {
    let _ = bridge.submit_order(Order {
        id: OrderId(2001),
        account: ClobAccountId(1),
        side: Side::Buy,
        qty: Qty(1),
        order_type: OrderType::Limit { price: Price(95) },
    });
    let _ = bridge.submit_order(Order {
        id: OrderId(2002),
        account: ClobAccountId(2),
        side: Side::Sell,
        qty: Qty(1),
        order_type: OrderType::Limit { price: Price(97) },
    });
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
    match &report.funding {
        Some(f) => print!(
            "funding(premium={}, rate={}, settlements={}) ",
            f.premium.0,
            f.rate.0,
            f.settlements.len(),
        ),
        None => print!("funding=skip "),
    }
    println!(
        "vault(shares={}, assets={}, price_bps={:?})",
        report.vault_total_shares, report.vault_total_assets, report.vault_share_price_bps
    );
}
