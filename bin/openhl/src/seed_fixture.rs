// Stage 19b: bin-crate `unreachable_pub` triggers on every public item
// in this module since `openhl` has no library surface. Silence at the
// module level — same pattern as `rpc.rs`.
#![allow(unreachable_pub)]

//! `--seed-fixture` JSON loader + replayer (Stage 19b).
//!
//! `bin/openhl reth-devnet`'s default boot scenario is hardcoded in
//! [`crate::seed_accounts_via_fills`] / [`crate::seed_mark_orders`] +
//! a deposit loop. That's deterministic and load-bearing — see the
//! `openhl-synthetic-seed-contract` memory — but it locks operators
//! into one demo shape. This module lets `--seed-fixture <path>`
//! point at a JSON fixture that describes:
//!
//!   - **Trades** — `submit_order` calls in order, including the
//!     mark-book resting orders that drive `current_mark`.
//!   - **Deposits** — `bridge.deposit(account, amount)` calls.
//!
//! Together the two describe everything `bin/openhl` does between
//! Reth boot and the first `tick`. A fixture that replays the
//! current hardcoded seed lives at `examples/seed-default.json`.
//!
//! ### Determinism contract
//!
//! Fixture-driven seeds are subject to the same cross-validator
//! determinism rules as the hardcoded seed: every validator MUST
//! load the same fixture file, or their bridge states diverge and
//! the chain forks. Operators running an N-validator network are
//! responsible for distributing the fixture out-of-band (same way
//! they distribute `validators.json` today).

use std::path::Path;

use openhl_clob::{AccountId as ClobAccountId, Order, OrderId, OrderType, Price, Qty, Side};
use openhl_evm::LiveRethEvmBridge;
use serde::{Deserialize, Serialize};

/// Wire shape of the fixture file. JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedFixture {
    /// `submit_order` calls, applied in order. Includes both
    /// account-creating trades AND the mark-book resting orders
    /// that give the bridge a midpoint.
    pub trades: Vec<TradeOp>,
    /// `bridge.deposit(account, amount)` calls. Applied after all
    /// trades so positions exist when collateral is credited.
    #[serde(default)]
    pub deposits: Vec<DepositOp>,
}

/// One submit_order call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeOp {
    /// `OrderId` for the order. Must be unique across the fixture
    /// — the matching engine rejects duplicates.
    pub id: u64,
    /// `AccountId` of the order placer.
    pub account: u64,
    /// `Buy` or `Sell`. Case-sensitive (matches `openhl_clob::Side`
    /// variant names).
    pub side: SideRpc,
    /// Order quantity.
    pub qty: u64,
    /// `Limit` or `Market`.
    pub kind: OrderKindRpc,
    /// Required for `Limit`, ignored for `Market`. Missing on a
    /// Limit entry → parse error.
    #[serde(default)]
    pub price: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SideRpc {
    Buy,
    Sell,
}

impl From<SideRpc> for Side {
    fn from(s: SideRpc) -> Self {
        match s {
            SideRpc::Buy => Self::Buy,
            SideRpc::Sell => Self::Sell,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderKindRpc {
    Limit,
    Market,
}

/// One `bridge.deposit(account, amount)` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepositOp {
    pub account: u64,
    /// Signed quote-currency amount. Positive credits, negative
    /// debits (same as the bridge's `deposit` method).
    pub amount: i64,
}

/// Parse a fixture from a path.
pub fn load_from_path(path: &Path) -> eyre::Result<SeedFixture> {
    let bytes = std::fs::read(path)
        .map_err(|e| eyre::eyre!("seed fixture {}: {e}", path.display()))?;
    let fixture: SeedFixture = serde_json::from_slice(&bytes)
        .map_err(|e| eyre::eyre!("seed fixture {} parse: {e}", path.display()))?;
    Ok(fixture)
}

/// Replay every trade then every deposit against the bridge.
/// Returns total fills produced (sum across all trades) so the
/// caller can log a number comparable to what the hardcoded seed
/// reports.
pub fn replay<P>(bridge: &LiveRethEvmBridge<P>, fixture: &SeedFixture) -> eyre::Result<usize> {
    let mut total_fills = 0usize;
    for (idx, t) in fixture.trades.iter().enumerate() {
        let order_type = match t.kind {
            OrderKindRpc::Limit => {
                let price = t.price.ok_or_else(|| {
                    eyre::eyre!(
                        "seed fixture trade #{idx} (id={}) is Limit but has no `price`",
                        t.id
                    )
                })?;
                OrderType::Limit { price: Price(price) }
            }
            OrderKindRpc::Market => OrderType::Market,
        };
        let r = bridge.submit_order(Order {
            id: OrderId(t.id),
            account: ClobAccountId(t.account),
            side: t.side.into(),
            qty: Qty(t.qty),
            order_type,
        });
        total_fills += r.fills.len();
    }
    for d in &fixture.deposits {
        let _ = bridge.deposit(ClobAccountId(d.account), d.amount);
    }
    Ok(total_fills)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parsing happy path: a small two-trade + one-deposit fixture
    /// round-trips through serde.
    #[test]
    fn fixture_round_trips() {
        let json = r#"{
            "trades": [
                {"id": 1, "account": 40, "side": "Sell", "qty": 10, "kind": "Limit", "price": 110},
                {"id": 2, "account": 10, "side": "Buy",  "qty": 10, "kind": "Market"}
            ],
            "deposits": [
                {"account": 10, "amount": 200}
            ]
        }"#;
        let fx: SeedFixture = serde_json::from_str(json).expect("parse");
        assert_eq!(fx.trades.len(), 2);
        assert_eq!(fx.deposits.len(), 1);
        assert_eq!(fx.trades[0].side, SideRpc::Sell);
        assert_eq!(fx.trades[0].kind, OrderKindRpc::Limit);
        assert_eq!(fx.trades[0].price, Some(110));
        assert_eq!(fx.trades[1].kind, OrderKindRpc::Market);
        assert_eq!(fx.trades[1].price, None);
        assert_eq!(fx.deposits[0].account, 10);
        assert_eq!(fx.deposits[0].amount, 200);

        // Re-serialise and re-parse to confirm symmetry.
        let s = serde_json::to_string(&fx).expect("serialise");
        let again: SeedFixture = serde_json::from_str(&s).expect("re-parse");
        assert_eq!(again.trades.len(), 2);
        assert_eq!(again.deposits.len(), 1);
    }

    /// Empty deposits list is allowed (mark-book-only fixtures).
    #[test]
    fn fixture_with_no_deposits_parses() {
        let json = r#"{ "trades": [] }"#;
        let fx: SeedFixture = serde_json::from_str(json).expect("parse");
        assert!(fx.trades.is_empty());
        assert!(fx.deposits.is_empty());
    }

    /// A Limit trade without a `price` field is a config error,
    /// caught at replay time (not parse time — `price` is
    /// `Option<u64>` in the wire format because Market orders
    /// don't need one).
    #[test]
    fn replay_errors_on_limit_without_price() {
        use openhl_evm::LiveRethEvmBridge;
        use reth_chainspec::ChainSpec;
        use std::sync::Arc;

        let chain_spec = Arc::new(ChainSpec::default());
        let bridge = LiveRethEvmBridge::new((), chain_spec);
        let fx = SeedFixture {
            trades: vec![TradeOp {
                id: 1,
                account: 40,
                side: SideRpc::Sell,
                qty: 10,
                kind: OrderKindRpc::Limit,
                price: None,
            }],
            deposits: vec![],
        };
        let err = replay(&bridge, &fx).expect_err("must reject Limit without price");
        assert!(
            err.to_string().contains("Limit but has no `price`"),
            "unexpected error: {err}",
        );
    }

    /// A two-trade fixture (Sell limit + Buy market crossing it)
    /// produces one fill, exactly mirroring what the hardcoded
    /// seed's per-round shape does.
    #[test]
    fn replay_round_trips_a_single_fill() {
        use openhl_evm::LiveRethEvmBridge;
        use reth_chainspec::ChainSpec;
        use std::sync::Arc;

        let chain_spec = Arc::new(ChainSpec::default());
        let bridge = LiveRethEvmBridge::new((), chain_spec);
        let fx = SeedFixture {
            trades: vec![
                TradeOp {
                    id: 1,
                    account: 40,
                    side: SideRpc::Sell,
                    qty: 10,
                    kind: OrderKindRpc::Limit,
                    price: Some(110),
                },
                TradeOp {
                    id: 2,
                    account: 10,
                    side: SideRpc::Buy,
                    qty: 10,
                    kind: OrderKindRpc::Market,
                    price: None,
                },
            ],
            deposits: vec![DepositOp {
                account: 10,
                amount: 200,
            }],
        };
        let fills = replay(&bridge, &fx).expect("replay ok");
        assert_eq!(fills, 1, "Sell-limit 10 + Buy-market 10 produces exactly one fill");
        let snap = bridge.accounts_snapshot();
        assert_eq!(snap.len(), 2, "the cross creates two accounts (10, 40)");
    }
}
