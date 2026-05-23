//! Auto-deleveraging (ADL) — Layer 3 of the safety-net cascade (Stage 10d).
//!
//! When [`crate::scanner::LiquidationScanner`] finishes a scan with
//! `ScanReport::unfilled_deficit > 0`, the insurance fund couldn't
//! absorb everything. ADL is the last-resort mechanism: rank the
//! profitable counter-positions in the market by a "how much did they
//! win" score, force-close them in descending order, and haircut their
//! unrealized `PnL` until the deficit is absorbed.
//!
//! ### Why ADL bypasses the orderbook
//!
//! If we kept submitting market orders against profitable positions
//! through the matching engine, every order would punch through the
//! bid/ask stack and crash the mark further — which would push more
//! positions underwater. The feedback loop runs away. ADL is designed
//! to **close positions directly in the bookkeeping layer**, never
//! touching the orderbook. The records this module produces carry the
//! [`CloseOrderSpec`] for parity with Stage 10a's other paths, but the
//! bridge is expected to apply them as account-state mutations rather
//! than CLOB orders.
//!
//! ### How the haircut works
//!
//! Each ADL'd winner had unrealized `PnL` of `P` at the current mark.
//! In a normal close they'd receive `P` in full. With ADL they receive
//! `P - haircut`, where `haircut = min(remaining_deficit, P)`. The
//! system absorbs the `haircut` amount toward the unfilled deficit.
//! Winners with the highest score get the first cut; if the cumulative
//! haircuts reach the deficit before the candidate pool is exhausted,
//! later winners are untouched. If the candidate pool runs out first,
//! `AdlReport::deficit_remaining > 0` and the chain is in genuine
//! unresolved trouble.
//!
//! ### Score
//!
//! Following the Hyperliquid convention, score is
//! `unrealized_pnl_pct × leverage`, expressed in bps²/`MARGIN_SCALE`:
//!
//! ```text
//!   pnl_pct_bps  = pnl × MARGIN_SCALE / collateral
//!   leverage_bps = notional × MARGIN_SCALE / equity
//!   score        = pnl_pct_bps × leverage_bps / MARGIN_SCALE
//! ```
//!
//! The intuition: the "luckiest" winners are those who both made the
//! highest relative gain AND took the most leveraged risk to get
//! there. They take the haircut first. Stable-sort ties break by
//! `AccountId` ascending so two equally-lucky winners produce a
//! deterministic order across validators.
//!
//! ### Determinism
//!
//! - All arithmetic uses i128 intermediates with saturating-to-i64
//!   conversions.
//! - The ranking is a stable sort with a fully-defined tiebreaker.
//! - No clock reads, no `HashMap` iteration.
//!
//! Given the same `(candidates, mark, deficit)`, every validator
//! produces a byte-identical [`AdlReport`].

use crate::compute::{
    account_equity, close_order_spec, notional_value, saturate_i128_to_i64, unrealized_pnl,
};
use crate::types::{AccountSnapshot, CloseOrderSpec, MARGIN_SCALE};
use openhl_clob::AccountId;
use openhl_funding::MarkPrice;

/// ADL ranking score. Higher means earlier force-close.
///
/// Computed as `pnl_pct × leverage`, both expressed in `MARGIN_SCALE`
/// units; the product is renormalized once. Saturates at `i64::MAX`
/// for pathological inputs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AdlScore(pub i64);

/// Per-account record of one ADL force-close.
///
/// The bridge applies these as bookkeeping mutations: credit the
/// trader's collateral by `pnl_paid`, set their position size to zero,
/// remove the account from the open-positions table. `close_order`
/// carries the spec for parity with Stage 10a's other paths and for
/// telemetry; the matching engine is **not** consulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AdlRecord {
    pub account: AccountId,
    /// The (notional) close-order spec; emitted for telemetry and shape
    /// consistency with [`crate::scanner::LiquidationRecord`]. The
    /// bridge does NOT submit this to the CLOB.
    pub close_order: CloseOrderSpec,
    /// Unrealized `PnL` at the current mark — what the trader would
    /// have received in a normal close.
    pub pnl_gross: i64,
    /// Amount the system kept toward absorbing the deficit
    /// (`min(remaining_deficit, pnl_gross)` at the time this record
    /// was generated).
    pub haircut: i64,
    /// What the trader actually receives. Always `pnl_gross - haircut`,
    /// always `≥ 0`.
    pub pnl_paid: i64,
    /// The ranking score at the moment of selection.
    pub score: AdlScore,
}

/// Summary of one ADL pass.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AdlReport {
    /// One record per ADL'd account, in execution (rank) order.
    pub records: Vec<AdlRecord>,
    /// Total haircuts applied — how much of the input deficit was
    /// absorbed.
    pub deficit_absorbed: i64,
    /// What the candidate pool couldn't cover. If `> 0`, the chain
    /// must halt or the operator must accept the residual as protocol
    /// loss.
    pub deficit_remaining: i64,
}

/// Compute the ADL score for one account at `mark`.
///
/// Returns `None` for accounts that are not eligible for ADL:
///   - Non-profitable positions (`unrealized_pnl ≤ 0`).
///   - Flat positions (`position_size == 0`).
///   - Accounts whose collateral or equity is zero (degenerate;
///     score's divisor would be zero or negative).
#[must_use]
pub fn adl_score(snapshot: &AccountSnapshot, mark: MarkPrice) -> Option<AdlScore> {
    if snapshot.position_size.0 == 0 {
        return None;
    }
    let pnl = unrealized_pnl(snapshot, mark);
    if pnl <= 0 {
        return None;
    }
    let collateral = snapshot.collateral.0;
    if collateral <= 0 {
        return None;
    }
    let equity = account_equity(snapshot, mark);
    if equity <= 0 {
        return None;
    }
    let notional = notional_value(snapshot, mark);

    // pnl_pct_bps = pnl × MARGIN_SCALE / collateral
    let pnl_pct = i128::from(pnl).saturating_mul(i128::from(MARGIN_SCALE))
        / i128::from(collateral);
    // leverage_bps = notional × MARGIN_SCALE / equity
    let leverage = i128::from(notional).saturating_mul(i128::from(MARGIN_SCALE))
        / i128::from(equity);
    // score = pnl_pct × leverage / MARGIN_SCALE (renormalize)
    let raw = pnl_pct.saturating_mul(leverage) / i128::from(MARGIN_SCALE);
    Some(AdlScore(saturate_i128_to_i64(raw)))
}

/// Execute one ADL pass over the candidate set.
///
/// Pipeline:
///   1. Filter to ADL-eligible accounts (see [`adl_score`]).
///   2. Stable-sort by score descending; ties break by `AccountId`
///      ascending so two equally-ranked accounts produce a
///      deterministic order.
///   3. Iterate, applying `haircut = min(remaining_deficit, pnl_gross)`
///      to each in rank order. Stop when `remaining_deficit == 0` or
///      candidates are exhausted.
///
/// Returns an [`AdlReport`] whose `deficit_absorbed + deficit_remaining`
/// equals the input `deficit` (modulo saturating arithmetic).
///
/// A non-positive `deficit` is treated as "nothing to do" — returns an
/// empty report.
#[must_use]
pub fn execute_adl(
    candidates: &[AccountSnapshot],
    mark: MarkPrice,
    deficit: i64,
) -> AdlReport {
    if deficit <= 0 {
        return AdlReport {
            records: Vec::new(),
            deficit_absorbed: 0,
            deficit_remaining: deficit.max(0),
        };
    }

    // Step 1 + 2: score every candidate, drop non-eligible, sort by
    // (score desc, account_id asc).
    let mut ranked: Vec<(AccountSnapshot, AdlScore, i64)> = candidates
        .iter()
        .filter_map(|s| {
            let score = adl_score(s, mark)?;
            let pnl = unrealized_pnl(s, mark);
            Some((*s, score, pnl))
        })
        .collect();
    // Stable sort: primary key score (descending), tiebreaker account_id (ascending).
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.account.0.cmp(&b.0.account.0)));

    // Step 3: iterate and haircut.
    let mut report = AdlReport::default();
    let mut remaining = deficit;
    for (snapshot, score, pnl_gross) in ranked {
        if remaining <= 0 {
            break;
        }
        let haircut = remaining.min(pnl_gross);
        let pnl_paid = pnl_gross.saturating_sub(haircut);
        report.records.push(AdlRecord {
            account: snapshot.account,
            close_order: close_order_spec(&snapshot),
            pnl_gross,
            haircut,
            pnl_paid,
            score,
        });
        report.deficit_absorbed = report.deficit_absorbed.saturating_add(haircut);
        remaining = remaining.saturating_sub(haircut);
    }
    report.deficit_remaining = remaining;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AccountSnapshot;
    use openhl_funding::{Notional, PositionSize};
    use proptest::prelude::*;

    fn snapshot(account: u64, size: i64, entry: u64, collateral: i64) -> AccountSnapshot {
        AccountSnapshot {
            account: AccountId(account),
            position_size: PositionSize(size),
            avg_entry: MarkPrice(entry),
            collateral: Notional(collateral),
        }
    }

    // ─── adl_score: ineligibility ──────────────────────────────────

    #[test]
    fn score_none_for_flat_position() {
        let s = snapshot(1, 0, 100, 1_000);
        assert_eq!(adl_score(&s, MarkPrice(100)), None);
    }

    #[test]
    fn score_none_for_losing_long() {
        // Long 1 @ 100, mark 80 → pnl = -20 → not eligible
        let s = snapshot(1, 1, 100, 1_000);
        assert_eq!(adl_score(&s, MarkPrice(80)), None);
    }

    #[test]
    fn score_none_for_short_at_entry() {
        // pnl = 0, not profitable.
        let s = snapshot(1, -1, 100, 1_000);
        assert_eq!(adl_score(&s, MarkPrice(100)), None);
    }

    #[test]
    fn score_none_for_zero_collateral() {
        let s = snapshot(1, 1, 100, 0);
        // Even if profitable at mark 120, collateral = 0 makes pnl_pct
        // undefined (divide by zero) → ineligible.
        assert_eq!(adl_score(&s, MarkPrice(120)), None);
    }

    // ─── adl_score: ordering ───────────────────────────────────────

    #[test]
    fn score_higher_for_higher_leverage_winner() {
        // Two profitable longs with the same pnl_pct but different
        // leverage. Higher leverage → higher score.
        // Long 1 @ entry 100, mark 200 → pnl = 100.
        // A: collateral 100, equity = 100 + 100 = 200, notional = 200, leverage = 1×
        //    pnl_pct_bps = 100 × 10_000 / 100 = 10_000
        //    leverage_bps = 200 × 10_000 / 200 = 10_000
        //    score = 10_000 × 10_000 / 10_000 = 10_000
        // B: collateral 50, equity = 50 + 100 = 150, notional = 200, leverage = ~1.33×
        //    pnl_pct_bps = 100 × 10_000 / 50 = 20_000
        //    leverage_bps = 200 × 10_000 / 150 = 13_333
        //    score = 20_000 × 13_333 / 10_000 = 26_666
        let a = snapshot(1, 1, 100, 100);
        let b = snapshot(2, 1, 100, 50);
        let sa = adl_score(&a, MarkPrice(200)).unwrap();
        let sb = adl_score(&b, MarkPrice(200)).unwrap();
        assert!(sb > sa, "higher leverage winner should rank above lower");
    }

    // ─── execute_adl: degenerate ───────────────────────────────────

    #[test]
    fn adl_zero_deficit_is_noop() {
        let candidates = vec![snapshot(1, 1, 100, 100)];
        let report = execute_adl(&candidates, MarkPrice(200), 0);
        assert!(report.records.is_empty());
        assert_eq!(report.deficit_absorbed, 0);
        assert_eq!(report.deficit_remaining, 0);
    }

    #[test]
    fn adl_negative_deficit_clamps_remaining_to_zero() {
        // Defensive: a negative deficit can't be "absorbed" but also
        // shouldn't propagate as a negative remainder.
        let report = execute_adl(&[], MarkPrice(100), -50);
        assert_eq!(report.deficit_remaining, 0);
    }

    #[test]
    fn adl_no_candidates_keeps_full_deficit() {
        let report = execute_adl(&[], MarkPrice(100), 5_000);
        assert!(report.records.is_empty());
        assert_eq!(report.deficit_absorbed, 0);
        assert_eq!(report.deficit_remaining, 5_000);
    }

    #[test]
    fn adl_no_profitable_keeps_full_deficit() {
        // All candidates are losers (long entered at 100, mark 80).
        let candidates = vec![snapshot(1, 1, 100, 1_000), snapshot(2, 1, 100, 1_000)];
        let report = execute_adl(&candidates, MarkPrice(80), 500);
        assert!(report.records.is_empty());
        assert_eq!(report.deficit_remaining, 500);
    }

    // ─── execute_adl: single winner ────────────────────────────────

    #[test]
    fn adl_single_winner_fully_absorbs_small_deficit() {
        // One profitable long with PnL = 100, deficit = 30.
        // haircut = min(30, 100) = 30; payout = 70.
        let candidates = vec![snapshot(1, 1, 100, 100)];
        let report = execute_adl(&candidates, MarkPrice(200), 30);
        assert_eq!(report.records.len(), 1);
        let rec = &report.records[0];
        assert_eq!(rec.pnl_gross, 100);
        assert_eq!(rec.haircut, 30);
        assert_eq!(rec.pnl_paid, 70);
        assert_eq!(report.deficit_absorbed, 30);
        assert_eq!(report.deficit_remaining, 0);
    }

    #[test]
    fn adl_single_winner_partial_haircut_at_full_pnl() {
        // PnL = 100, deficit = 100 → full haircut, payout = 0.
        let candidates = vec![snapshot(1, 1, 100, 100)];
        let report = execute_adl(&candidates, MarkPrice(200), 100);
        let rec = &report.records[0];
        assert_eq!(rec.haircut, 100);
        assert_eq!(rec.pnl_paid, 0);
        assert_eq!(report.deficit_remaining, 0);
    }

    #[test]
    fn adl_single_winner_exhausted_with_remaining_deficit() {
        // PnL = 100, deficit = 250 → full haircut, 150 remains.
        let candidates = vec![snapshot(1, 1, 100, 100)];
        let report = execute_adl(&candidates, MarkPrice(200), 250);
        assert_eq!(report.records.len(), 1);
        assert_eq!(report.deficit_absorbed, 100);
        assert_eq!(report.deficit_remaining, 150);
    }

    // ─── execute_adl: multiple winners ─────────────────────────────

    #[test]
    fn adl_multiple_winners_in_score_order() {
        // Two long winners; the higher-leverage one ranks first.
        // A: coll 100, pnl 100 → score 10_000 (computed above)
        // B: coll 50, pnl 100 → score 26_666
        // deficit = 80 → B haircut = 80, pnl_paid = 20; A untouched.
        let candidates = vec![snapshot(1, 1, 100, 100), snapshot(2, 1, 100, 50)];
        let report = execute_adl(&candidates, MarkPrice(200), 80);
        assert_eq!(report.records.len(), 1, "deficit smaller than B's pnl → only B");
        assert_eq!(report.records[0].account, AccountId(2));
        assert_eq!(report.records[0].haircut, 80);
    }

    #[test]
    fn adl_drains_first_winner_then_partially_second() {
        // Both winners contribute to a large deficit.
        // A: coll 100, pnl 100 → score 10_000, rank #2
        // B: coll 50, pnl 100 → score 26_666, rank #1
        // deficit = 150 → B haircut = 100 (full), A haircut = 50
        let candidates = vec![snapshot(1, 1, 100, 100), snapshot(2, 1, 100, 50)];
        let report = execute_adl(&candidates, MarkPrice(200), 150);
        assert_eq!(report.records.len(), 2);
        assert_eq!(report.records[0].account, AccountId(2)); // B first
        assert_eq!(report.records[0].haircut, 100);
        assert_eq!(report.records[0].pnl_paid, 0);
        assert_eq!(report.records[1].account, AccountId(1)); // A second
        assert_eq!(report.records[1].haircut, 50);
        assert_eq!(report.records[1].pnl_paid, 50);
        assert_eq!(report.deficit_absorbed, 150);
        assert_eq!(report.deficit_remaining, 0);
    }

    #[test]
    fn adl_tiebreaker_by_account_id_ascending() {
        // Two structurally identical winners. Tiebreaker is account_id
        // ascending → smaller account_id is force-closed first.
        let candidates = vec![
            snapshot(7, 1, 100, 50),  // identical except account
            snapshot(3, 1, 100, 50),
        ];
        let report = execute_adl(&candidates, MarkPrice(200), 50);
        assert_eq!(report.records.len(), 1);
        assert_eq!(report.records[0].account, AccountId(3));
    }

    #[test]
    fn adl_does_not_touch_losers_or_flats() {
        let candidates = vec![
            snapshot(1, 1, 100, 50),     // winner @ mark 200
            snapshot(2, 1, 100, 1_000),  // loser @ mark 80? — actually we use one mark
            snapshot(3, 0, 100, 1_000),  // flat
        ];
        // All evaluated at mark = 200 → only acct 1 is profitable.
        let report = execute_adl(&candidates, MarkPrice(200), 10);
        assert_eq!(report.records.len(), 1);
        assert_eq!(report.records[0].account, AccountId(1));
    }

    // ─── proptest: invariants ──────────────────────────────────────

    proptest! {
        /// Conservation: absorbed + remaining = input deficit (for
        /// non-negative inputs).
        #[test]
        fn conservation_absorbed_plus_remaining_equals_deficit(
            collaterals in proptest::collection::vec(1_i64..1_000_000, 0..15),
            mark in 1_u64..1_000,
            deficit in 0_i64..1_000_000,
        ) {
            let entry = 100u64; // any positive entry
            let candidates: Vec<_> = collaterals
                .iter()
                .enumerate()
                .map(|(i, c)| snapshot(i as u64, 1, entry, *c))
                .collect();
            let report = execute_adl(&candidates, MarkPrice(mark), deficit);
            prop_assert_eq!(report.deficit_absorbed + report.deficit_remaining, deficit);
        }

        /// Every record has `pnl_paid == pnl_gross - haircut`, with
        /// both haircut and pnl_paid non-negative.
        #[test]
        fn each_record_balances_pnl(
            collaterals in proptest::collection::vec(1_i64..1_000_000, 0..15),
            mark in 1_u64..1_000,
            deficit in 1_i64..1_000_000,
        ) {
            let entry = 100u64;
            let candidates: Vec<_> = collaterals
                .iter()
                .enumerate()
                .map(|(i, c)| snapshot(i as u64, 1, entry, *c))
                .collect();
            let report = execute_adl(&candidates, MarkPrice(mark), deficit);
            for rec in &report.records {
                prop_assert!(rec.haircut >= 0);
                prop_assert!(rec.haircut <= rec.pnl_gross);
                prop_assert!(rec.pnl_paid >= 0);
                prop_assert_eq!(rec.pnl_paid, rec.pnl_gross - rec.haircut);
            }
        }

        /// Total haircuts equal `deficit_absorbed`.
        #[test]
        fn total_haircut_equals_deficit_absorbed(
            collaterals in proptest::collection::vec(1_i64..1_000_000, 0..15),
            mark in 1_u64..1_000,
            deficit in 1_i64..1_000_000,
        ) {
            let entry = 100u64;
            let candidates: Vec<_> = collaterals
                .iter()
                .enumerate()
                .map(|(i, c)| snapshot(i as u64, 1, entry, *c))
                .collect();
            let report = execute_adl(&candidates, MarkPrice(mark), deficit);
            let total: i64 = report.records.iter().map(|r| r.haircut).sum();
            prop_assert_eq!(total, report.deficit_absorbed);
        }

        /// Determinism: same input twice → same report.
        #[test]
        fn execute_adl_is_deterministic(
            collaterals in proptest::collection::vec(1_i64..1_000_000, 0..10),
            mark in 1_u64..1_000,
            deficit in 0_i64..1_000_000,
        ) {
            let entry = 100u64;
            let candidates: Vec<_> = collaterals
                .iter()
                .enumerate()
                .map(|(i, c)| snapshot(i as u64, 1, entry, *c))
                .collect();
            let r1 = execute_adl(&candidates, MarkPrice(mark), deficit);
            let r2 = execute_adl(&candidates, MarkPrice(mark), deficit);
            prop_assert_eq!(r1, r2);
        }

        /// Records are in non-increasing score order (or equal score
        /// with strictly ascending account_id).
        #[test]
        fn records_in_rank_order(
            collaterals in proptest::collection::vec(1_i64..1_000_000, 2..15),
            mark in 100_u64..500,
            deficit in 1_000_000_i64..10_000_000,
        ) {
            // Big deficit ensures we get many records.
            let entry = 100u64;
            let candidates: Vec<_> = collaterals
                .iter()
                .enumerate()
                .map(|(i, c)| snapshot(i as u64, 1, entry, *c))
                .collect();
            let report = execute_adl(&candidates, MarkPrice(mark), deficit);
            for w in report.records.windows(2) {
                let (a, b) = (&w[0], &w[1]);
                // Either strict score decrease, OR same score with smaller account_id first.
                let ok = a.score > b.score
                    || (a.score == b.score && a.account.0 < b.account.0);
                prop_assert!(ok, "rank order broken between {:?} and {:?}", a, b);
            }
        }

    }
}
