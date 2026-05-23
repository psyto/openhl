//! openhl — Hyperliquid-shape L1 reference implementation.
//!
//! Thin entry point. Constructs an [`OpenHlNode`] at hyperliquid-shape
//! defaults and prints a one-line banner. The real bootstrap (Reth
//! node, consensus bridge, P2P stack) lives in `crates/evm`'s
//! `LiveRethEvmBridge` and `crates/consensus` — `bin/openhl` will own
//! the wiring once those layers are coordinated end-to-end in a
//! follow-up stage.

use openhl_node::{OpenHlNode, OpenHlNodeConfig};

fn main() {
    let config = OpenHlNodeConfig::hyperliquid_default();
    let node = OpenHlNode::new(config);

    println!("openhl v{} (Hyperliquid-shape L1 reference)", env!("CARGO_PKG_VERSION"));
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
