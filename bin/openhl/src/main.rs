//! openhl — Hyperliquid-shape L1 reference implementation.
//!
//! Two subcommands:
//!
//!   - `info` (default) — print the node's static config + initial state.
//!   - `devnet [N]` — drive `N` single-validator consensus rounds through an
//!     in-memory EVM bridge, calling `OpenHlNode::tick` between blocks.
//!     This is the smallest runnable demo of the full per-block flow at the
//!     binary level. Full Reth + actor-engine integration (Stage 13c) will
//!     replace `InMemoryEvmBridge` with `LiveRethEvmBridge` and
//!     `run_single_validator` with `run_engine_app`.
//!
//! Examples:
//!   $ openhl                       # equivalent to `openhl info`
//!   $ openhl info
//!   $ openhl devnet                # one consensus round
//!   $ openhl devnet 5              # five consensus rounds

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use openhl_consensus::run_single_validator;
use openhl_evm::InMemoryEvmBridge;
use openhl_funding::MarkPrice;
use openhl_node::{OpenHlNode, OpenHlNodeConfig, TickInput, TickReport};
use openhl_types::BlockHash;

fn main() -> eyre::Result<()> {
    let mut args = std::env::args().skip(1);
    let subcommand = args.next();

    match subcommand.as_deref() {
        None | Some("info") => {
            print_info();
            Ok(())
        }
        Some("devnet") => {
            let rounds: u64 = args
                .next()
                .map(|s| s.parse())
                .transpose()
                .map_err(|e: std::num::ParseIntError| eyre::eyre!("invalid rounds: {e}"))?
                .unwrap_or(1);
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(run_devnet(rounds))
        }
        Some(other) => {
            eprintln!("openhl: unknown subcommand `{other}`");
            eprintln!("usage: openhl [info | devnet [N]]");
            std::process::exit(2);
        }
    }
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

/// Drive `rounds` single-validator consensus rounds, calling
/// `OpenHlNode::tick` between blocks.
///
/// Uses [`InMemoryEvmBridge`] as the EL side — no Reth boot needed.
/// Each round:
///   1. `run_single_validator` drives one consensus round to a decision
///      (commits a fresh `ExecutedBlock` through the bridge).
///   2. We extract the chain's wall-clock-style "block time" and call
///      `OpenHlNode::tick` with empty account snapshots and a dummy
///      mark. The point is to **demonstrate composition**, not to
///      simulate a market — full simulation lives in the crate-level
///      proptest harnesses, not in this binary.
async fn run_devnet(rounds: u64) -> eyre::Result<()> {
    let mut coordinator = OpenHlNode::new(OpenHlNodeConfig::hyperliquid_default());
    let bridge = Arc::new(InMemoryEvmBridge::new());

    // First "parent" is the all-zero genesis hash. The single-validator
    // runner produces a real `BlockHash` we feed into the next round.
    let mut parent = BlockHash([0u8; 32]);

    println!(
        "openhl v{} — driving {} single-validator devnet round{}",
        env!("CARGO_PKG_VERSION"),
        rounds,
        if rounds == 1 { "" } else { "s" }
    );

    for round in 0..rounds {
        let block_height = round + 1;
        let block_time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            // Add `round` so consecutive rounds advance time by at least
            // a second — keeps the oracle-refresh cadence test happy in
            // tight loops.
            .saturating_add(round);

        // 1. One consensus round: produce + decide + commit a block.
        let decided = run_single_validator(bridge.as_ref(), parent).await?;
        println!(
            "round {}: decided {} via in-memory bridge",
            block_height,
            short_hash(&decided)
        );

        // 2. Run the coordinator's per-block tick. Empty snapshots and a
        // dummy mark — this binary is the composition demo, not a market
        // simulator. The Stage 13 tests exercise the real liquidation +
        // ADL paths with synthetic snapshots.
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

fn short_hash(h: &BlockHash) -> String {
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
