// Stage 21: bin-crate `unreachable_pub` triggers on every public item
// in this module since `openhl` has no library surface. Silence at the
// module level — same pattern as `rpc.rs` / `seed_fixture.rs`.
#![allow(unreachable_pub)]

//! `--chain-history` JSON loader + per-tick applier (Stage 21).
//!
//! Stage 19b's [`crate::seed_fixture`] runs the entire seed
//! ([`SeedFixture`] in that module) once before consensus
//! starts — every account, fill, mark-book order applied
//! all-at-once. The boot cascade then springs fully-formed on
//! tick 1.
//!
//! Chain-history takes the same kinds of events
//! ([`crate::seed_fixture::TradeOp`] / [`crate::seed_fixture::DepositOp`])
//! and groups them per block height. At the start of every tick,
//! the applier reads the events for that height and runs them
//! against the bridge BEFORE the rest of the tick (oracle ingest
//! → liquidation scan → ADL → vault mark-to-market → funding
//! settlement) fires. So the cascade emerges naturally on the
//! block where the constellation of positions + mark drift +
//! oracle aggregate first crosses the liquidation thresholds —
//! rather than at block 1 from a hand-crafted snapshot.
//!
//! ### When to use which
//!
//! - **No flag** (hardcoded path): the demo's pre-tuned cascade,
//!   in one block. Best for the README / docs / first-look demo.
//! - **`--seed-fixture <path>`** (Stage 19b): operator-supplied
//!   replacement for the hardcoded path, still all-at-once.
//!   Useful for "what if the seed had different shape" demos.
//! - **`--chain-history <path>`** (Stage 21, this module): events
//!   apply per block height during consensus. Useful when you
//!   want the chain to "live a little" before the cascade — e.g.,
//!   recording a multi-block trade sequence from a live exchange
//!   and replaying it deterministically.
//!
//! `--chain-history` and `--seed-fixture` are mutually exclusive
//! — both modify the boot scenario and combining them would make
//! the determinism contract ambiguous.
//!
//! ### Determinism contract
//!
//! Same as [`crate::seed_fixture`]: every validator MUST load the
//! same chain-history file, or their bridge states diverge.
//! Validators distribute the file out-of-band exactly as they do
//! for [`crate::seed_fixture::SeedFixture`].
//!
//! ### Restart caveat (MVP)
//!
//! The chain-history applier runs once per tick callback. On
//! restart-resume, the consensus driver picks up at the next
//! height (Stage 13i), and only that height's events apply —
//! events from prior blocks are NOT replayed (their effects are
//! already in the bridge snapshot). Side-effects that DON'T
//! persist in the snapshot (the CLOB book itself is the load-
//! bearing example — Stage 16b notes resting orders aren't
//! snapshotted) won't survive restart unless the chain-history
//! file lists them in every block that needs them, or you wipe
//! the data-dir on each demo run.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use openhl_clob::{AccountId as ClobAccountId, Order, OrderId, OrderType, Price, Qty};
use openhl_evm::LiveRethEvmBridge;
use serde::{Deserialize, Serialize};

use crate::seed_fixture::{DepositOp, OrderKindRpc, TradeOp};

/// Wire shape of a chain-history file. JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainHistory {
    /// Per-block events, in any order in the JSON file — the
    /// loader sorts by `height` and rejects duplicates. Each
    /// block's events apply at the START of the tick callback
    /// for that height.
    pub blocks: Vec<HistoryBlock>,
}

/// One block's worth of events. Trades apply first, then
/// deposits — same order as [`crate::seed_fixture::replay`] uses
/// for the all-at-once seed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryBlock {
    /// 1-indexed block height these events belong to. Events
    /// for height `N` apply when consensus delivers its `N`th
    /// block.
    pub height: u64,
    /// `submit_order` calls — same shape as
    /// [`crate::seed_fixture::TradeOp`].
    #[serde(default)]
    pub trades: Vec<TradeOp>,
    /// `bridge.deposit` calls — same shape as
    /// [`crate::seed_fixture::DepositOp`].
    #[serde(default)]
    pub deposits: Vec<DepositOp>,
}

/// Loaded + indexed chain-history. Wrap in `Arc<ChainHistoryApplier>`
/// so the tick callback can clone-and-move it across threads. The
/// inner mutex guards the "blocks already applied" set so an
/// idempotent re-apply at the same height is a no-op (defensive,
/// not load-bearing — consensus normally fires each height once).
#[derive(Debug)]
pub struct ChainHistoryApplier {
    /// Block height → events. Sorted-by-key for stable iteration.
    by_height: BTreeMap<u64, HistoryBlock>,
    /// Heights whose events have been applied so far. The applier
    /// is a no-op for a repeated height — a defensive guard, since
    /// consensus may fire a tick callback for a height that
    /// already ran during restart-resume in some edge cases.
    applied: Mutex<std::collections::BTreeSet<u64>>,
}

impl ChainHistoryApplier {
    /// Construct from a loaded [`ChainHistory`]. Rejects duplicate
    /// heights — the JSON shouldn't list block 7 twice.
    pub fn new(history: ChainHistory) -> eyre::Result<Self> {
        let mut by_height = BTreeMap::new();
        for b in history.blocks {
            if by_height.insert(b.height, b).is_some() {
                return Err(eyre::eyre!(
                    "chain history: duplicate block height entry",
                ));
            }
        }
        Ok(Self {
            by_height,
            applied: Mutex::new(std::collections::BTreeSet::new()),
        })
    }

    /// Total number of distinct heights in the history.
    pub fn total_blocks(&self) -> usize {
        self.by_height.len()
    }

    /// Heights in the history, ascending.
    pub fn heights(&self) -> Vec<u64> {
        self.by_height.keys().copied().collect()
    }

    /// Apply the events for `height` to `bridge`. Returns
    /// `Ok(Some((trades_run, fills, deposits_run)))` on apply,
    /// `Ok(None)` if there's no entry at `height` OR it's already
    /// been applied. Errors are surfaced (e.g., a `Limit` trade
    /// without a `price`) but bubble up to the caller — the tick
    /// callback decides whether to halt or log-and-continue.
    pub fn apply_for_height<P>(
        &self,
        bridge: &LiveRethEvmBridge<P>,
        height: u64,
    ) -> eyre::Result<Option<(usize, usize, usize)>> {
        let block = match self.by_height.get(&height) {
            Some(b) => b,
            None => return Ok(None),
        };
        {
            let mut applied = self
                .applied
                .lock()
                .map_err(|_| eyre::eyre!("chain history applied-set mutex poisoned"))?;
            if applied.contains(&height) {
                return Ok(None);
            }
            applied.insert(height);
        }

        let mut total_fills = 0usize;
        for (idx, t) in block.trades.iter().enumerate() {
            let order_type = match t.kind {
                OrderKindRpc::Limit => {
                    let price = t.price.ok_or_else(|| {
                        eyre::eyre!(
                            "chain history height {height} trade #{idx} (id={}) is Limit but has no `price`",
                            t.id,
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
        for d in &block.deposits {
            let _ = bridge.deposit(ClobAccountId(d.account), d.amount);
        }
        Ok(Some((block.trades.len(), total_fills, block.deposits.len())))
    }
}

/// Parse a chain-history file from a path.
pub fn load_from_path(path: &Path) -> eyre::Result<ChainHistory> {
    let bytes = std::fs::read(path)
        .map_err(|e| eyre::eyre!("chain history {}: {e}", path.display()))?;
    let history: ChainHistory = serde_json::from_slice(&bytes)
        .map_err(|e| eyre::eyre!("chain history {} parse: {e}", path.display()))?;
    Ok(history)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seed_fixture::SideRpc;
    use reth_chainspec::ChainSpec;
    use std::sync::Arc;

    /// Parsing happy path: two blocks with overlapping account
    /// activity round-trip through serde.
    #[test]
    fn chain_history_round_trips() {
        let json = r#"{
            "blocks": [
                {
                    "height": 1,
                    "trades": [
                        {"id": 1, "account": 40, "side": "Sell", "qty": 50, "kind": "Limit", "price": 110}
                    ],
                    "deposits": [
                        {"account": 10, "amount": 200}
                    ]
                },
                {
                    "height": 2,
                    "trades": [
                        {"id": 2, "account": 10, "side": "Buy", "qty": 50, "kind": "Market"}
                    ]
                }
            ]
        }"#;
        let h: ChainHistory = serde_json::from_str(json).expect("parse");
        assert_eq!(h.blocks.len(), 2);
        assert_eq!(h.blocks[0].height, 1);
        assert_eq!(h.blocks[0].trades.len(), 1);
        assert_eq!(h.blocks[0].deposits.len(), 1);
        assert_eq!(h.blocks[1].height, 2);
        assert_eq!(h.blocks[1].trades.len(), 1);
        assert!(h.blocks[1].deposits.is_empty());
    }

    /// Duplicate heights → `new()` errors. The matching engine
    /// would tolerate it (each block runs independently), but the
    /// invariant lets the applier guarantee one-set-of-events-per-
    /// height, which downstream logging assumes.
    #[test]
    fn applier_rejects_duplicate_heights() {
        let history = ChainHistory {
            blocks: vec![
                HistoryBlock { height: 5, trades: vec![], deposits: vec![] },
                HistoryBlock { height: 5, trades: vec![], deposits: vec![] },
            ],
        };
        let err = ChainHistoryApplier::new(history).expect_err("dup must fail");
        assert!(
            err.to_string().contains("duplicate block height"),
            "unexpected error: {err}",
        );
    }

    /// Apply at a height present in the history → events fire
    /// (one trade resting → no fill, but the order goes into the
    /// book). Apply at a height NOT in the history → no-op.
    /// Re-apply at the same height → no-op (defensive idempotency).
    #[test]
    fn apply_for_height_runs_then_is_idempotent() {
        let chain_spec = Arc::new(ChainSpec::default());
        let bridge = LiveRethEvmBridge::new((), chain_spec);

        let history = ChainHistory {
            blocks: vec![HistoryBlock {
                height: 3,
                trades: vec![TradeOp {
                    id: 1,
                    account: 40,
                    side: SideRpc::Sell,
                    qty: 10,
                    kind: OrderKindRpc::Limit,
                    price: Some(110),
                }],
                deposits: vec![DepositOp { account: 40, amount: 500 }],
            }],
        };
        let applier = ChainHistoryApplier::new(history).expect("construct");

        // Height 1 / 2 → no entry → None.
        assert!(applier.apply_for_height(&bridge, 1).unwrap().is_none());
        assert!(applier.apply_for_height(&bridge, 2).unwrap().is_none());

        // Height 3 → entry → applies. Returns counts.
        let counts = applier
            .apply_for_height(&bridge, 3)
            .unwrap()
            .expect("entry at height 3");
        assert_eq!(counts, (1, 0, 1), "1 trade, 0 fills (resting), 1 deposit");

        // Second time at height 3 → no-op (idempotent).
        assert!(applier.apply_for_height(&bridge, 3).unwrap().is_none());

        // Subsequent heights past the history → no-op.
        assert!(applier.apply_for_height(&bridge, 4).unwrap().is_none());
    }
}
