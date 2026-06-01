//! `openhl-node` — integration coordinator for the openhl L1 (Stage 13).
//!
//! No new state machines, no new pure-compute primitives. This crate
//! is the **composition layer**: it owns one [`OracleState`], one
//! [`LiquidationScanner`] (with its [`InsuranceFund`]), and one
//! [`VaultState`], and runs them through the per-block tick that
//! `crates/liquidation/src/lib.rs` documents as "the bridge's
//! per-block flow." Stage 13 lifts that comment into actual code.
//!
//! ### What `tick` does
//!
//! Each block the bridge calls [`OpenHlNode::tick`] with:
//!   - `block_time` / `block_height` — current chain time/height.
//!   - `mark` — the current top-of-book mark price (from the CLOB).
//!   - `account_snapshots` — every non-flat account in the market
//!     (the bridge assembles these from its position table).
//!   - `vault_total_assets` — the vault's current asset value
//!     (collateral + marked `PnL`), computed off-tick by the bridge
//!     from the vault's own positions.
//!
//! The tick then:
//!   1. **Refreshes the oracle** (if the configured interval has
//!      elapsed since the last refresh). Stale-feed filter + median
//!      + deviation guard from `openhl_oracle`.
//!   2. **Scans for liquidations** using [`LiquidationScanner::scan`].
//!      Liquidatable / Underwater accounts produce close orders and
//!      mutate the insurance fund.
//!   3. **Runs ADL** if `ScanReport::unfilled_deficit > 0` and the
//!      config opted in. Profitable counter-positions are ranked and
//!      haircut via [`execute_adl`].
//!   4. **Marks the vault to market** by pushing the bridge-computed
//!      `vault_total_assets` into [`VaultState::mark_to_market`]. No
//!      shares are minted or burned — only NAV per share moves.
//!
//! Funding settlement is **not** part of `tick` — it's per-position
//! and happens on the funding clock's own cadence, called by the
//! bridge separately. The bridge layer composes both as it sees fit.
//!
//! ### What `tick` does NOT do
//!
//! - **Submit close orders to the CLOB.** `tick` produces a
//!   `ScanReport` whose `records` carry close-order specs; the bridge
//!   submits them to the matching engine. Keeping the coordinator
//!   side-effect-free against the CLOB lets it stay a pure
//!   state-machine driver.
//! - **Apply ADL bookkeeping mutations.** Same reason — `tick`
//!   produces an `AdlReport` whose records the bridge applies to its
//!   own position/balance tables.
//! - **Halt the chain on unresolvable deficit.** If `tick` returns
//!   `adl.deficit_remaining > 0`, the bridge decides whether to halt
//!   or accept protocol loss per deployment policy. Stage 13 doesn't
//!   make that policy call.
//!
//! ### Why no Reth boot here
//!
//! Booting Reth + the consensus bridge is `crates/evm`'s
//! `LiveRethEvmBridge` (in production-shape since Stage 9d).
//! `openhl-node` is one level above that: the per-block state-machine
//! driver that the bridge calls into. Splitting the Reth-side
//! composition (in `evm`) from the openhl-side composition (here)
//! keeps each layer independently testable. The `bin/openhl` binary
//! will own wiring of these two layers together.

use openhl_funding::{FundingClock, FundingParams, FundingTick, MarkPrice, Position};
use openhl_liquidation::{
    execute_adl, AccountSnapshot, AdlReport, InsuranceFund, LiquidationParams,
    LiquidationScanner, ScanReport,
};
use openhl_oracle::{
    AggregatedPrice, AggregationError, FeedId, ObservationError, OracleParams, OracleState,
    PriceObservation, PublisherKey,
};
use openhl_vault::{VaultParams, VaultState};
use serde::{Deserialize, Serialize};

/// Static configuration for the node. Set once at chain genesis;
/// changing values mid-chain would fork the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpenHlNodeConfig {
    /// Seconds between automatic oracle refreshes. The tick triggers
    /// a refresh when `block_time >= last_refresh + interval`.
    pub oracle_refresh_interval_secs: u64,
    /// Liquidation engine parameters (initial / maintenance margin,
    /// liquidation fee).
    pub liquidation_params: LiquidationParams,
    /// Oracle aggregator parameters (staleness window, min feeds,
    /// deviation cap).
    pub oracle_params: OracleParams,
    /// Vault parameters (deposit floor).
    pub vault_params: VaultParams,
    /// Funding clock parameters (interval, rate cap, divisor).
    pub funding_params: FundingParams,
    /// When `true`, the tick auto-runs ADL on any
    /// `ScanReport::unfilled_deficit > 0`. When `false`, the bridge
    /// inspects the scan report itself and decides what to do.
    pub run_adl_on_unfilled_deficit: bool,
}

impl OpenHlNodeConfig {
    /// Hyperliquid-shape defaults that match the worked examples in
    /// the rethlab Perp Primer course. Real deployments override.
    #[must_use]
    pub const fn hyperliquid_default() -> Self {
        Self {
            oracle_refresh_interval_secs: 12,
            liquidation_params: LiquidationParams::hyperliquid_default(),
            oracle_params: OracleParams::hyperliquid_default(),
            vault_params: VaultParams::production_default(),
            funding_params: FundingParams::hyperliquid_default(),
            run_adl_on_unfilled_deficit: true,
        }
    }
}

/// Per-tick input the bridge hands the coordinator.
#[derive(Debug, Clone, Copy)]
pub struct TickInput<'a> {
    pub block_height: u64,
    pub block_time: u64,
    /// Current top-of-book mark from the CLOB. The coordinator does
    /// not read the CLOB itself — the bridge supplies it.
    pub mark: MarkPrice,
    /// Snapshots of every non-flat account in the market. The bridge
    /// is responsible for deterministic ordering (typically
    /// `account_id`-sorted).
    pub account_snapshots: &'a [AccountSnapshot],
    /// Vault's current total assets (collateral + marked `PnL`)
    /// computed off-tick by the bridge from the vault's own perp
    /// positions.
    pub vault_total_assets: i64,
}

/// Snapshot of the [`OpenHlNode`]'s runtime state, for restart-time
/// resume (Stage 14e). Saved to disk by the binary alongside the
/// bridge's chain snapshot; restored via [`OpenHlNode::load_snapshot`]
/// on the next boot before the engine app loop starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoordinatorSnapshot {
    /// Insurance fund balance carried over from the last run. The
    /// scanner re-instantiates with this on load.
    pub insurance_fund_balance: i64,
    /// Total vault shares outstanding.
    pub vault_total_shares: u64,
    /// Total vault assets under management. May be negative if the
    /// vault is insolvent — see [`VaultState::is_insolvent`].
    pub vault_total_assets: i64,
    /// Last block_time at which the oracle successfully refreshed.
    /// Restoring this prevents an unnecessary refresh on the first
    /// tick after restart when the interval hasn't elapsed.
    pub last_oracle_refresh_at: Option<u64>,
    /// Last block_time at which the funding clock successfully
    /// settled (Stage 15a). Restoring this prevents an unintended
    /// extra settlement on the first tick after restart when the
    /// interval hasn't elapsed.
    pub funding_last_settled_at: u64,
    /// Cached oracle aggregate from the last successful refresh
    /// (Stage 16d). Persisting it means the funding clock and any
    /// other consumer of `oracle.current_price()` keep working
    /// across restart instead of silently pausing until the next
    /// refresh interval elapses. `#[serde(default)]` so older
    /// on-disk coordinator snapshots deserialize as `None`.
    #[serde(default)]
    pub cached_oracle_price: Option<AggregatedPrice>,
}

/// Per-tick output — aggregated reports plus a snapshot of post-tick
/// vault state for telemetry. Every field is structured so the bridge
/// can pick the parts it needs without re-reading the coordinator's
/// internal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickReport {
    pub block_height: u64,
    pub block_time: u64,
    /// `Some(Ok(price))` if the oracle refreshed this tick and
    /// succeeded; `Some(Err(...))` if it tried and failed (insufficient
    /// fresh feeds, quorum failed after deviation filter); `None` if
    /// the refresh interval hadn't elapsed.
    pub oracle: Option<Result<AggregatedPrice, AggregationError>>,
    /// The liquidation scan report.
    pub liquidation: ScanReport,
    /// `Some(report)` when the scan surfaced an `unfilled_deficit > 0`
    /// AND the config opted into auto-ADL; `None` otherwise.
    pub adl: Option<AdlReport>,
    /// Vault state after `mark_to_market`. Bridge uses this for
    /// telemetry / accounting reconciliation.
    pub vault_total_shares: u64,
    pub vault_total_assets: i64,
    pub vault_share_price_bps: Option<i64>,
    /// `Some(tick)` when the funding clock fired this block — i.e.,
    /// the oracle had a cached index price AND the interval since the
    /// last settlement had elapsed. `None` otherwise.
    ///
    /// Stage 15a only surfaces the rate / settlements as telemetry;
    /// applying the settlements to account balances is the clearing
    /// layer's job and lands in a later stage.
    pub funding: Option<FundingTick>,
}

/// The integration coordinator. One [`OpenHlNode`] per deployed
/// market — multi-market deployments instantiate one per market.
#[derive(Debug, Clone)]
pub struct OpenHlNode {
    config: OpenHlNodeConfig,
    oracle: OracleState,
    scanner: LiquidationScanner,
    vault: VaultState,
    funding_clock: FundingClock,
    last_oracle_refresh_at: Option<u64>,
}

impl OpenHlNode {
    /// Construct a fresh node from config. The oracle, scanner, and
    /// vault all start in their empty states (no feeds, no insurance
    /// fund, no shares).
    #[must_use]
    pub fn new(config: OpenHlNodeConfig) -> Self {
        let oracle = OracleState::new(config.oracle_params);
        let scanner = LiquidationScanner::with_empty_fund(config.liquidation_params);
        let vault = VaultState::new(config.vault_params);
        // Genesis time 0: first tick at any block_time ≥ interval_secs fires.
        let funding_clock = FundingClock::new(config.funding_params, 0);
        Self {
            config,
            oracle,
            scanner,
            vault,
            funding_clock,
            last_oracle_refresh_at: None,
        }
    }

    /// Construct a node from an existing insurance-fund balance —
    /// supports resuming from a snapshot or genesis-seeding the fund.
    #[must_use]
    pub fn with_insurance_fund(config: OpenHlNodeConfig, fund: InsuranceFund) -> Self {
        let oracle = OracleState::new(config.oracle_params);
        let scanner = LiquidationScanner::new(config.liquidation_params, fund);
        let vault = VaultState::new(config.vault_params);
        let funding_clock = FundingClock::new(config.funding_params, 0);
        Self {
            config,
            oracle,
            scanner,
            vault,
            funding_clock,
            last_oracle_refresh_at: None,
        }
    }

    /// Borrow the config.
    #[must_use]
    pub const fn config(&self) -> &OpenHlNodeConfig {
        &self.config
    }

    /// Borrow the oracle (read-only).
    #[must_use]
    pub const fn oracle(&self) -> &OracleState {
        &self.oracle
    }

    /// Mutable access to the oracle. The bridge uses this to register
    /// publisher keys, ingest signed observations, etc. — operations
    /// that happen between ticks rather than inside one.
    pub const fn oracle_mut(&mut self) -> &mut OracleState {
        &mut self.oracle
    }

    /// Borrow the liquidation scanner (read-only).
    #[must_use]
    pub const fn scanner(&self) -> &LiquidationScanner {
        &self.scanner
    }

    /// Borrow the vault (read-only).
    #[must_use]
    pub const fn vault(&self) -> &VaultState {
        &self.vault
    }

    /// Mutable access to the vault. The bridge uses this for deposit
    /// / withdraw operations that happen between ticks.
    pub const fn vault_mut(&mut self) -> &mut VaultState {
        &mut self.vault
    }

    /// Register a publisher key, passthrough to the oracle. Stage 11b
    /// path; the bridge calls this once per publisher at chain
    /// configuration time (and again for each rotation).
    pub fn register_publisher(&mut self, feed: FeedId, key: PublisherKey) {
        self.oracle.register_publisher(feed, key);
    }

    /// Ingest one observation via the unsigned (trusted-bridge) path.
    /// Returns the same [`ObservationError`]s as the underlying
    /// [`OracleState::ingest`].
    pub fn ingest_observation(
        &mut self,
        obs: PriceObservation,
        now: u64,
    ) -> Result<(), ObservationError> {
        self.oracle.ingest(obs, now)
    }

    /// Ingest one signed observation. Verifies the ECDSA signature
    /// against the registered publisher key before storing.
    pub fn ingest_signed_observation(
        &mut self,
        obs: PriceObservation,
        now: u64,
    ) -> Result<(), ObservationError> {
        self.oracle.ingest_signed(obs, now)
    }

    /// Run one per-block tick.
    ///
    /// Order of operations is fixed (deterministic):
    ///   1. Oracle refresh (if interval elapsed).
    ///   2. Liquidation scan.
    ///   3. ADL (conditional on scan result + config).
    ///   4. Vault mark-to-market.
    ///
    /// Mark sources:
    ///
    /// * **Liquidation scan + ADL** use the oracle's cached
    ///   aggregated index price if one is available **and fresh**
    ///   (within `aggregate_max_age_secs` of `input.block_time`,
    ///   Stage 17q), falling back to `input.mark` (the bridge-
    ///   supplied CLOB midpoint) when no refresh has succeeded or
    ///   the last refresh has aged out. This is the production-
    ///   shape choice — the oracle is the cross-venue reference;
    ///   the bridge's `bridge.margin_health` accessor reads the
    ///   same source, so scan / ADL classifications match what
    ///   users see over RPC and what Solidity contracts get from
    ///   the `openhl_margin_health` precompile (Stage 17o → 17q).
    /// * **Funding rate** uses `input.mark` (CLOB midpoint) as the
    ///   mark and the oracle's fresh aggregate as the index,
    ///   because `premium = mark − index` is the whole point of
    ///   the funding formula — using oracle for both sides would
    ///   collapse the premium to zero. A stale oracle skips the
    ///   funding tick rather than computing premium against an
    ///   aged index (Stage 17q).
    ///
    /// Order of operations is fixed (deterministic):
    ///   1. Oracle refresh (if interval elapsed).
    ///   2. Liquidation scan.
    ///   3. ADL (conditional on scan result + config).
    ///   4. Vault mark-to-market.
    ///   5. Funding tick.
    pub fn tick(&mut self, input: TickInput<'_>) -> TickReport {
        // 1. Oracle refresh — only if the interval has elapsed.
        let oracle_result = self.maybe_refresh_oracle(input.block_time);

        // Mark for solvency classification: prefer the oracle's
        // aggregated index, but only if it's fresh enough
        // (`oracle.params.aggregate_max_age_secs` — Stage 17q).
        // A stalled publisher set degrades to the CLOB midpoint
        // rather than letting an aging oracle delay liquidations.
        let scan_mark = self
            .oracle
            .current_price_fresh_at(input.block_time)
            .map(|i| MarkPrice(i.0))
            .unwrap_or(input.mark);

        // 2. Liquidation scan at the oracle-preferred mark.
        let scan = self.scanner.scan(input.account_snapshots, scan_mark);

        // 3. ADL only if scan surfaced unfilled deficit AND config opts in.
        let adl_report = if self.config.run_adl_on_unfilled_deficit && scan.unfilled_deficit > 0 {
            Some(execute_adl(
                input.account_snapshots,
                scan_mark,
                scan.unfilled_deficit,
            ))
        } else {
            None
        };

        // 4. Vault mark-to-market — no shares move, only NAV.
        self.vault.mark_to_market(input.vault_total_assets);

        // 5. Funding tick — only if the oracle has a cached current
        //    price AND the funding interval has elapsed. The clock's
        //    own gating decides whether a settlement actually fires;
        //    we just supply the inputs. Per the module's "no catch-up"
        //    invariant, a long gap still produces at most one tick.
        // Stage 17q: funding also respects oracle freshness — a
        // stalled oracle skips funding rather than computing
        // `premium` against a stale index.
        let funding = self.oracle.current_price_fresh_at(input.block_time).and_then(|index| {
            let positions: Vec<Position> = input
                .account_snapshots
                .iter()
                .map(|snap| Position {
                    account: snap.account,
                    size: snap.position_size,
                })
                .collect();
            self.funding_clock
                .tick(input.block_time, input.mark, index, &positions)
        });

        TickReport {
            block_height: input.block_height,
            block_time: input.block_time,
            oracle: oracle_result,
            liquidation: scan,
            adl: adl_report,
            vault_total_shares: self.vault.total_shares().0,
            vault_total_assets: self.vault.total_assets().0,
            vault_share_price_bps: self.vault.share_price_bps(),
            funding,
        }
    }

    /// Capture the load-bearing fields for cross-restart resume
    /// (Stage 14e). Mirrors the bridge's [`BridgeSnapshot`] —
    /// deliberately small, covering only the fields that the next
    /// boot can't reconstruct from config + per-block inputs.
    ///
    /// Excluded by design:
    ///   - `config`: comes from the binary at boot.
    ///   - `oracle.feeds` / `oracle.publishers`: feeds are re-ingested
    ///     every block; publishers are re-registered by the binary at
    ///     boot (the bridge owns the registry).
    ///   - `scanner.params` / `vault.params`: come from config.
    #[must_use]
    pub fn snapshot(&self) -> CoordinatorSnapshot {
        CoordinatorSnapshot {
            insurance_fund_balance: self.scanner.fund_balance(),
            vault_total_shares: self.vault.total_shares().0,
            vault_total_assets: self.vault.total_assets().0,
            last_oracle_refresh_at: self.last_oracle_refresh_at,
            funding_last_settled_at: self.funding_clock.last_settled_at(),
            cached_oracle_price: self.oracle.current(),
        }
    }

    /// Apply a [`CoordinatorSnapshot`] to this node's runtime state.
    /// Used immediately after `OpenHlNode::new` to resume from a prior
    /// run's persisted snapshot. Publisher registrations and oracle
    /// feed observations are NOT restored — those flow back through
    /// `register_publisher` and `ingest_signed_observation` at boot.
    pub fn load_snapshot(&mut self, snap: CoordinatorSnapshot) {
        self.scanner = LiquidationScanner::new(
            self.config.liquidation_params,
            InsuranceFund::new(snap.insurance_fund_balance),
        );
        self.vault = VaultState::restore(
            self.config.vault_params,
            snap.vault_total_shares,
            snap.vault_total_assets,
        );
        self.funding_clock = FundingClock::new(
            self.config.funding_params,
            snap.funding_last_settled_at,
        );
        self.last_oracle_refresh_at = snap.last_oracle_refresh_at;
        if let Some(price) = snap.cached_oracle_price {
            self.oracle.restore_current(price);
        }
    }

    fn maybe_refresh_oracle(
        &mut self,
        block_time: u64,
    ) -> Option<Result<AggregatedPrice, AggregationError>> {
        let should_refresh = match self.last_oracle_refresh_at {
            None => true,
            Some(last) => {
                block_time.saturating_sub(last) >= self.config.oracle_refresh_interval_secs
            }
        };
        if !should_refresh {
            return None;
        }
        let result = self.oracle.refresh(block_time);
        if result.is_ok() {
            self.last_oracle_refresh_at = Some(block_time);
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openhl_funding::{IndexPrice, Notional, PositionSize};

    fn default_node() -> OpenHlNode {
        OpenHlNode::new(OpenHlNodeConfig::hyperliquid_default())
    }

    fn snapshot(account: u64, size: i64, entry: u64, collateral: i64) -> AccountSnapshot {
        AccountSnapshot {
            account: openhl_clob::AccountId(account),
            position_size: PositionSize(size),
            avg_entry: MarkPrice(entry),
            collateral: Notional(collateral),
        }
    }

    // ─── construction ──────────────────────────────────────────────

    #[test]
    fn new_node_is_empty() {
        let node = default_node();
        assert_eq!(node.oracle().feed_count(), 0);
        assert_eq!(node.scanner().fund_balance(), 0);
        assert_eq!(node.vault().total_shares().0, 0);
        assert_eq!(node.vault().total_assets().0, 0);
    }

    #[test]
    fn snapshot_round_trips_load_bearing_state() {
        // Build up some non-default state, snapshot it, restore on a
        // fresh node, and assert the load-bearing fields match.
        let mut node = OpenHlNode::with_insurance_fund(
            OpenHlNodeConfig::hyperliquid_default(),
            InsuranceFund::new(750),
        );
        assert_eq!(node.scanner().fund_balance(), 750);
        // Vault: pretend a depositor put 10_000 in (mints 10_000 shares at inception).
        let _ = node.vault_mut().deposit(10_000).expect("inception deposit");
        // last_oracle_refresh_at is private to this module — fine to
        // set directly inside `mod tests`.
        node.last_oracle_refresh_at = Some(12);

        // Pretend the funding clock has settled once at block_time 9.
        node.funding_clock = FundingClock::new(node.config.funding_params, 9);
        // Stage 16d: also pretend the oracle has a cached price.
        node.oracle.restore_current(AggregatedPrice {
            index: openhl_funding::IndexPrice(102),
            computed_at: 12,
            feeds_used: 3,
        });

        let snap = node.snapshot();
        assert_eq!(snap.insurance_fund_balance, 750);
        assert_eq!(snap.vault_total_shares, 10_000);
        assert_eq!(snap.vault_total_assets, 10_000);
        assert_eq!(snap.last_oracle_refresh_at, Some(12));
        assert_eq!(snap.funding_last_settled_at, 9);
        assert_eq!(
            snap.cached_oracle_price.map(|p| p.index.0),
            Some(102),
        );

        // Round-trip via serde to mirror the real on-disk path.
        let bytes = serde_json::to_vec(&snap).expect("serialize");
        let decoded: CoordinatorSnapshot = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded, snap);

        let mut fresh = default_node();
        assert_eq!(fresh.scanner().fund_balance(), 0);
        assert_eq!(fresh.vault().total_shares().0, 0);
        assert_eq!(fresh.funding_clock.last_settled_at(), 0);
        assert_eq!(fresh.oracle().current_price(), None);
        fresh.load_snapshot(decoded);
        assert_eq!(fresh.scanner().fund_balance(), 750);
        assert_eq!(fresh.vault().total_shares().0, 10_000);
        assert_eq!(fresh.vault().total_assets().0, 10_000);
        assert_eq!(fresh.last_oracle_refresh_at, Some(12));
        assert_eq!(fresh.funding_clock.last_settled_at(), 9);
        assert_eq!(
            fresh.oracle().current_price(),
            Some(openhl_funding::IndexPrice(102)),
        );
    }

    #[test]
    fn with_insurance_fund_seeds_balance() {
        let node = OpenHlNode::with_insurance_fund(
            OpenHlNodeConfig::hyperliquid_default(),
            InsuranceFund::new(50_000),
        );
        assert_eq!(node.scanner().fund_balance(), 50_000);
    }

    // ─── tick: empty market ────────────────────────────────────────

    #[test]
    fn tick_on_empty_market_does_nothing_destructive() {
        let mut node = default_node();
        let report = node.tick(TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(1_000),
            account_snapshots: &[],
            vault_total_assets: 0,
        });
        // Oracle tries to refresh (first tick) but has no feeds → error.
        assert!(matches!(
            report.oracle,
            Some(Err(AggregationError::TooFewFreshFeeds { .. }))
        ));
        assert!(report.liquidation.records.is_empty());
        assert!(report.adl.is_none());
        assert_eq!(report.vault_total_assets, 0);
    }

    // ─── tick: oracle cadence ──────────────────────────────────────

    #[test]
    fn tick_refreshes_oracle_at_first_tick_then_waits_interval() {
        let mut node = default_node();
        node.ingest_observation(
            PriceObservation::unsigned(FeedId(1), IndexPrice(100), 100),
            100,
        )
        .unwrap();
        node.ingest_observation(
            PriceObservation::unsigned(FeedId(2), IndexPrice(101), 100),
            100,
        )
        .unwrap();
        // Tick at t=100: first refresh fires.
        let r1 = node.tick(TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(100),
            account_snapshots: &[],
            vault_total_assets: 0,
        });
        assert!(matches!(r1.oracle, Some(Ok(_))));
        // Tick at t=105 (< 12s interval): no refresh.
        let r2 = node.tick(TickInput {
            block_height: 2,
            block_time: 105,
            mark: MarkPrice(100),
            account_snapshots: &[],
            vault_total_assets: 0,
        });
        assert!(r2.oracle.is_none(), "expected no refresh inside interval");
        // Tick at t=112 (exactly at boundary): refresh fires again.
        // We need a fresh observation though — old ones are 12s stale
        // relative to t=112 with the 60s default staleness window, so
        // they're still in range. Refresh should succeed.
        let r3 = node.tick(TickInput {
            block_height: 3,
            block_time: 112,
            mark: MarkPrice(100),
            account_snapshots: &[],
            vault_total_assets: 0,
        });
        assert!(matches!(r3.oracle, Some(Ok(_))));
    }

    // ─── tick: liquidation + ADL composition ───────────────────────

    #[test]
    fn tick_runs_liquidation_then_adl_on_unfilled_deficit() {
        // Mark = 80; entry = 100.
        // Long 1, $10 coll → pnl = -20, equity = -10 → underwater.
        // Short -1, $50 coll → pnl = +20, equity = 70 → profitable ADL victim.
        let mut node = default_node();
        let accounts = vec![snapshot(1, 1, 100, 10), snapshot(2, -1, 100, 50)];
        let report = node.tick(TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(80),
            account_snapshots: &accounts,
            vault_total_assets: 0,
        });

        // Liquidation: underwater long force-closed; fund empty → deficit.
        assert!(report.liquidation.unfilled_deficit > 0);
        // ADL: ran on the deficit, ate into the winner.
        let adl = report.adl.as_ref().expect("ADL should have fired");
        assert!(!adl.records.is_empty(), "ADL should have records");
        assert_eq!(adl.records[0].account, openhl_clob::AccountId(2));
        // Conservation: absorbed + remaining = the original deficit.
        assert_eq!(
            adl.deficit_absorbed + adl.deficit_remaining,
            report.liquidation.unfilled_deficit
        );
    }

    #[test]
    fn tick_skips_adl_when_config_opts_out() {
        let mut config = OpenHlNodeConfig::hyperliquid_default();
        config.run_adl_on_unfilled_deficit = false;
        let mut node = OpenHlNode::new(config);
        let accounts = vec![snapshot(1, 1, 100, 10)]; // underwater
        let report = node.tick(TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(80),
            account_snapshots: &accounts,
            vault_total_assets: 0,
        });
        assert!(report.liquidation.unfilled_deficit > 0);
        assert!(report.adl.is_none());
    }

    /// Stage 17p — scan + ADL prefer the oracle index over the
    /// bridge-supplied CLOB midpoint when one's available. Same
    /// long-1-at-100 / coll-10 position classifies Liquidatable
    /// against a midpoint of 80 (no oracle), then flips to Safe
    /// when the oracle's cached aggregate is restored to 100.
    #[test]
    fn tick_scan_prefers_oracle_index_over_input_mark() {
        use openhl_oracle::AggregatedPrice;

        // No oracle cached yet → fallback to `input.mark` (80).
        // At mark 80 the long is underwater (entry 100, coll 10):
        // uPnL = -20, equity = -10 → scan flags it.
        let mut node = default_node();
        let accounts = vec![snapshot(1, 1, 100, 10)];
        let report = node.tick(TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(80),
            account_snapshots: &accounts,
            vault_total_assets: 0,
        });
        assert!(
            !report.liquidation.records.is_empty(),
            "with no oracle, midpoint 80 makes the position liquidatable",
        );

        // Restore the oracle's cached aggregate to 100 (a "healthy"
        // mark). Same position, same midpoint input — now scan
        // sees mark 100 (uPnL=0, equity=10 → Safe) and produces
        // no records.
        let mut node = default_node();
        node.oracle_mut().restore_current(AggregatedPrice {
            index: openhl_funding::IndexPrice(100),
            computed_at: 50,
            feeds_used: 1,
        });
        let report = node.tick(TickInput {
            block_height: 2,
            block_time: 101,
            mark: MarkPrice(80),
            account_snapshots: &accounts,
            vault_total_assets: 0,
        });
        assert!(
            report.liquidation.records.is_empty(),
            "oracle index 100 overrides midpoint 80 → no liquidation",
        );
    }

    /// Stage 17q — when the oracle's cached aggregate is older
    /// than `aggregate_max_age_secs`, scan falls back to
    /// `input.mark` (CLOB midpoint), not the stale oracle value.
    #[test]
    fn tick_scan_falls_back_to_midpoint_when_oracle_is_stale() {
        use openhl_oracle::AggregatedPrice;

        // Default `aggregate_max_age_secs` is 60s. Restore a
        // cached aggregate computed at t=0 with a "healthy" mark
        // 100 — that price would keep the long Safe.
        let mut node = default_node();
        node.oracle_mut().restore_current(AggregatedPrice {
            index: openhl_funding::IndexPrice(100),
            computed_at: 0,
            feeds_used: 3,
        });

        // Now tick at t=1000 (way past the 60s max age). With
        // input.mark = 80, scan must use 80, not the stale 100.
        // Long 1 @ 100, coll 10: at mark 80 it's flagged
        // (underwater).
        let accounts = vec![snapshot(1, 1, 100, 10)];
        let report = node.tick(TickInput {
            block_height: 1,
            block_time: 1000,
            mark: MarkPrice(80),
            account_snapshots: &accounts,
            vault_total_assets: 0,
        });
        assert!(
            !report.liquidation.records.is_empty(),
            "stale oracle (computed_at=0, block_time=1000) → fall back to midpoint 80 → liquidatable",
        );

        // Sanity-check: a FRESH oracle at the same price would
        // produce the opposite outcome.
        let mut node = default_node();
        node.oracle_mut().restore_current(AggregatedPrice {
            index: openhl_funding::IndexPrice(100),
            computed_at: 990, // within 60s of block_time=1000
            feeds_used: 3,
        });
        let report = node.tick(TickInput {
            block_height: 2,
            block_time: 1000,
            mark: MarkPrice(80),
            account_snapshots: &accounts,
            vault_total_assets: 0,
        });
        assert!(
            report.liquidation.records.is_empty(),
            "fresh oracle 100 wins over midpoint 80 → not liquidatable",
        );
    }

    // ─── tick: vault mark-to-market ────────────────────────────────

    #[test]
    fn tick_marks_vault_to_market() {
        let mut node = default_node();
        node.vault_mut().deposit(1_000).unwrap();
        let report = node.tick(TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(100),
            account_snapshots: &[],
            vault_total_assets: 1_200,
        });
        assert_eq!(report.vault_total_assets, 1_200);
        assert_eq!(report.vault_total_shares, 1_000, "shares unchanged");
        // 1_200 × 10_000 / 1_000 = 12_000 bps (1.2×)
        assert_eq!(report.vault_share_price_bps, Some(12_000));
    }

    #[test]
    fn tick_vault_insolvent_when_marked_negative() {
        let mut node = default_node();
        node.vault_mut().deposit(1_000).unwrap();
        let report = node.tick(TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(100),
            account_snapshots: &[],
            vault_total_assets: -50,
        });
        assert_eq!(report.vault_total_assets, -50);
        assert_eq!(report.vault_share_price_bps, None);
        assert!(node.vault().is_insolvent());
    }

    // ─── determinism ───────────────────────────────────────────────

    #[test]
    fn tick_is_deterministic() {
        let make = || {
            let mut n = OpenHlNode::with_insurance_fund(
                OpenHlNodeConfig::hyperliquid_default(),
                InsuranceFund::new(1_000),
            );
            n.vault_mut().deposit(500).unwrap();
            n
        };
        let mut node_a = make();
        let mut node_b = make();
        let accounts = vec![snapshot(1, 1, 100, 10), snapshot(2, -1, 100, 50)];
        let input = TickInput {
            block_height: 1,
            block_time: 100,
            mark: MarkPrice(80),
            account_snapshots: &accounts,
            vault_total_assets: 500,
        };
        let r_a = node_a.tick(input);
        let r_b = node_b.tick(input);
        assert_eq!(r_a, r_b);
    }
}
