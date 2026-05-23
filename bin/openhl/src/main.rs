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

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy_genesis::Genesis;
use clap::{Parser, Subcommand};
use informalsystems_malachitebft_app::node::{Node, NodeHandle};
use informalsystems_malachitebft_signing_ed25519::PrivateKey;
use openhl_consensus::run_engine_app;
use openhl_consensus::run_single_validator;
use openhl_evm::{InMemoryEvmBridge, LiveRethEvmBridge, OpenHlExecutorBuilder};
use openhl_funding::MarkPrice;
use openhl_node::{OpenHlNode, OpenHlNodeConfig, TickInput, TickReport};
use openhl_types::BlockHash;
use rand::rngs::OsRng;
use reth_chainspec::ChainSpec;
use reth_node_builder::{NodeBuilder, NodeHandle as RethNodeHandle};
use reth_node_core::node_config::NodeConfig;
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

        /// Data directory for Reth's MDBX database and the consensus home
        /// dir. Defaults to a tempdir (cleaned up at process exit).
        ///
        /// **Caveat**: even when set, the underlying `testing_node`
        /// runtime still cleans up the datadir when the node handle drops.
        /// True cross-restart persistence requires Stage 13f's production
        /// `NodeBuilder` path. Until then, `--data-dir` is mostly useful for
        /// observing what Reth writes during a single run, or for running
        /// inside a tmpfs.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
}

fn main() -> eyre::Result<()> {
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
        } => tokio_rt()?.block_on(run_reth_devnet(rounds, moniker, data_dir)),
    }
}

fn tokio_rt() -> eyre::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Into::into)
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
async fn run_reth_devnet(
    rounds: u64,
    moniker: String,
    data_dir: Option<PathBuf>,
) -> eyre::Result<()> {
    println!(
        "openhl v{} — driving {} reth-backed decision{}",
        env!("CARGO_PKG_VERSION"),
        rounds,
        if rounds == 1 { "" } else { "s" }
    );

    // 1. Reth boot.
    let chain_spec = dev_chain_spec();
    let node_config = NodeConfig::test().dev().with_chain(chain_spec.clone());
    let runtime = Runtime::test();

    println!("[1/6] booting Reth EthereumNode with OpenHlExecutorBuilder…");
    if let Some(ref path) = data_dir {
        println!("      Reth data dir = {}", path.display());
    }
    // testing_node_with_datadir lets callers control where MDBX data lives
    // during this run. testing_node (no datadir) uses an OS tempdir. Both
    // paths are still test-utils flavored — true cross-process persistence
    // requires Stage 13f's production NodeBuilder path.
    let launch_context = if let Some(ref path) = data_dir {
        NodeBuilder::new(node_config).testing_node_with_datadir(runtime, path.clone())
    } else {
        NodeBuilder::new(node_config).testing_node(runtime)
    };
    let RethNodeHandle {
        node,
        node_exit_future: _,
    } = launch_context
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
        "      genesis hash = 0x{}…{}",
        hex_prefix(&genesis_hash_bytes, 4),
        hex_suffix(&genesis_hash_bytes, 4),
    );

    // 3. Consensus node with single-validator set (fresh keypair).
    println!("[3/6] generating Ed25519 keypair + single-validator set…");
    let private = PrivateKey::generate(OsRng);
    let public = private.public_key();
    let digest = Sha256::digest(public.as_bytes());
    let mut addr_bytes = [0u8; 20];
    addr_bytes.copy_from_slice(&digest[12..32]);
    let address = openhl_consensus::types::OpenHlAddress(addr_bytes);
    let validator_set = openhl_consensus::types::OpenHlValidatorSet::new(vec![
        openhl_consensus::types::OpenHlValidator::new(address, public, 1),
    ]);

    // Consensus home dir: if the user gave --data-dir, use a subdir under
    // it for consensus state (separate from Reth's MDBX directory). If not,
    // fall back to a tempdir that drops at process exit.
    let (consensus_home, _home_tmp_guard) = if let Some(ref base) = data_dir {
        let path = base.join("consensus");
        std::fs::create_dir_all(&path)?;
        (path, None)
    } else {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().to_path_buf();
        (path, Some(tmp))
    };
    println!("      consensus home dir = {}", consensus_home.display());
    let consensus_node = openhl_consensus::OpenHlNode::new(
        private,
        validator_set.clone(),
        consensus_home,
        moniker.clone(),
    );
    println!("      moniker = {moniker}");

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
    println!("[6/6] driving run_engine_app for {rounds} decision(s)…");
    let bridge_for_engine = bridge.clone();
    let validator_set_for_engine = validator_set.clone();
    let rounds_usize = usize::try_from(rounds)
        .map_err(|_| eyre::eyre!("rounds value too large for usize on this target"))?;
    let app_task = tokio::spawn(async move {
        run_engine_app(
            bridge_for_engine,
            channels,
            validator_set_for_engine,
            genesis_parent,
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

    // Clean shutdown regardless of the result above — proves the
    // teardown path works even when block production stops short.
    println!("shutting down consensus actor system…");
    handle.kill(None).await?;
    println!("reth-devnet teardown complete");

    Ok(())
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
