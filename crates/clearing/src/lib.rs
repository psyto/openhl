//! `openhl-clearing` — per-account position bookkeeping under CLOB fills.
//!
//! Pure state machine. Given an [`Account`] and a fill at `(price, qty,
//! side)`, [`apply_fill`] mutates the account in place and returns the
//! signed quote-currency PnL realized by the fill (`0` if the fill only
//! opened or increased the position, non-zero only when contracts were
//! closed).
//!
//! ### What this crate is, in one paragraph
//!
//! Hyperliquid-shape perpetuals settle in two parallel mechanisms:
//! continuous price exposure via the *position* (size + avg_entry) and
//! periodic cash transfers via funding ([`openhl_funding`]). When a
//! CLOB fill matches a maker and a taker, both accounts' positions
//! must update: the avg_entry of any increase is a volume-weighted
//! mean, and any decrease realizes PnL at `qty × (fill_price −
//! avg_entry)`, signed by the side being decreased. This crate
//! formalizes that math as a pure function so every validator that
//! consumes the same fill sequence reaches the same per-account
//! state — the determinism guarantee the rest of openhl already
//! depends on.
//!
//! ### Why a separate crate
//!
//! `openhl-clob` produces fills; `openhl-funding` consumes positions;
//! `openhl-liquidation` consumes account snapshots; `openhl-vault`
//! holds collateral. None of them owns the "what does a fill do to my
//! position?" rule — that gap is what this crate fills.
//!
//! ### What this crate is NOT
//!
//! - **A holder of mutable account state.** The owning layer (bridge,
//!   eventual clearing house) is responsible for keeping accounts on
//!   disk + replaying fills against them. This crate is the pure
//!   `(state, input) → state` math.
//! - **A fee model.** Maker rebates, taker fees, exchange fees — all
//!   bridge-layer concerns. `apply_fill` returns the raw realized PnL;
//!   the caller subtracts fees if applicable.
//! - **A funding settler.** Funding settlement adjusts collateral on
//!   a separate cadence; see [`openhl_funding::FundingClock`].

use openhl_clob::{AccountId, Price, Qty, Side};
use openhl_funding::{MarkPrice, Notional, PositionSize};
use serde::{Deserialize, Serialize};

/// Scale factor for basis-point margin rates: 10⁴, so 1000 bps = 10%.
/// Matches `openhl_liquidation::MARGIN_SCALE`; duplicated here so the
/// margin helper below doesn't pull in the liquidation crate as a
/// dependency of clearing's consumers.
pub const MARGIN_SCALE: i64 = 10_000;

/// Default initial-margin rate for v0: 10% (1000 bps), matching
/// [`openhl_liquidation::LiquidationParams::hyperliquid_default`]. The
/// bridge and the deposit/withdraw precompiles need a margin rate to
/// enforce margin-aware withdrawal but can't easily reach the
/// integration coordinator's `LiquidationParams` at v0 — a constant
/// scoped to clearing closes that gap until param plumbing lands.
pub const DEFAULT_INITIAL_MARGIN_BPS: u32 = 1_000;

/// One account's persistent perp state. Same shape as
/// `openhl_liquidation::AccountSnapshot` by design — the snapshot is a
/// per-tick read of this. We don't re-use that type directly because
/// it lives in the liquidation crate's dep tree and conceptually models
/// a *view*, not the owning record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub account: AccountId,
    /// Net signed contracts. Positive = long, negative = short, zero
    /// = flat.
    pub position_size: PositionSize,
    /// Volume-weighted average entry price of the current open
    /// position. Undefined when `position_size == 0`; we keep the
    /// previous value for telemetry but [`apply_fill`] re-initializes
    /// it on the next open.
    pub avg_entry: MarkPrice,
    /// Quote-currency collateral balance. Funding settlements and
    /// realized PnL deltas accumulate here; the field is **not**
    /// touched by `apply_fill` itself — the caller adds the returned
    /// realized PnL if it wants the bookkeeping closed inside one
    /// crate.
    pub collateral: Notional,
}

impl Account {
    /// Construct a flat account with zero collateral. Tests and the
    /// bridge use this for account creation on first sight.
    #[must_use]
    pub const fn flat(account: AccountId) -> Self {
        Self {
            account,
            position_size: PositionSize(0),
            avg_entry: MarkPrice(0),
            collateral: Notional(0),
        }
    }
}

/// Apply one CLOB fill to one account's position. Returns the
/// realized PnL (signed quote-currency) the fill produced — `0` when
/// the fill only opened or increased the position, non-zero when any
/// contracts were closed.
///
/// `side` is **this account's effective side on the fill**, not the
/// fill's "aggressor side" or the order book's mechanical side.
/// Concretely: when the order book records a fill between a Buy
/// resting order and a Sell taker, the maker's `side` is `Buy`
/// (their contracts long-increase) and the taker's `side` is `Sell`
/// (their contracts short-increase). Callers (the bridge) translate
/// the maker/taker roles into per-account `side` before calling.
///
/// ### Four cases the function handles
///
/// 1. **Open from flat** (`position_size == 0`): position becomes
///    `±qty`, `avg_entry = fill_price`. No realized PnL.
///
/// 2. **Increase same direction** (`sign(position) == sign(side)`):
///    position grows; `avg_entry` is updated to the volume-weighted
///    mean `(|old_size| × old_avg + qty × price) / (|old_size| +
///    qty)`. No realized PnL.
///
/// 3. **Decrease opposite direction, partial close** (`sign(position)
///    != sign(side)` and `qty < |position|`): position shrinks by
///    `qty`; `avg_entry` stays at the existing basis (closing
///    contracts doesn't change the entry basis of the remainder).
///    Realized PnL = `qty × (fill_price − avg_entry)` for a long
///    being closed (Sell against a long); `qty × (avg_entry −
///    fill_price)` for a short being closed (Buy against a short).
///
/// 4. **Flip** (`sign(position) != sign(side)` and `qty >
///    |position|`): existing position closes fully (realize PnL on
///    the old size), then a new position opens in the opposite
///    direction with the remaining quantity (`qty − |position|`) at
///    `avg_entry = fill_price`.
///
/// ### Integer arithmetic
///
/// All operations are `i128`-widened and `saturating_*` so a
/// malicious overflow can't fork the chain. The final result is
/// narrowed back to `i64`/`u64`. Same conventions as
/// `openhl-funding` and `openhl-liquidation`.
pub fn apply_fill(
    account: &mut Account,
    fill_price: Price,
    fill_qty: Qty,
    side: Side,
) -> i64 {
    let qty_i = i128::try_from(fill_qty.0).unwrap_or(i128::MAX);
    let price_i = i128::from(fill_price.0);
    let old_size = i128::from(account.position_size.0);
    let old_avg = i128::from(account.avg_entry.0);

    // The signed delta this fill contributes to position_size.
    // Buy increases (positive), Sell decreases (negative).
    let side_sign: i128 = match side {
        Side::Buy => 1,
        Side::Sell => -1,
    };
    let signed_delta = qty_i.saturating_mul(side_sign);

    // Decompose into "closed contracts" and "opened contracts".
    //
    // closed_qty = min(|old_size|, qty) if old_size has the opposite
    //              sign to side; 0 otherwise.
    // opened_qty = qty - closed_qty.
    let opposite_sign = old_size.signum() != 0 && old_size.signum() != side_sign;
    let closed_qty = if opposite_sign {
        old_size.unsigned_abs().min(qty_i.unsigned_abs() as u128) as i128
    } else {
        0
    };
    let opened_qty = qty_i - closed_qty;

    // ---- Realized PnL on the closed portion ---------------------
    //
    // For a long being closed by a Sell: realized = closed × (price - avg)
    // For a short being closed by a Buy: realized = closed × (avg - price)
    //
    // Equivalently: realized = sign_of_old_position × closed × (price - avg).
    let realized = if closed_qty > 0 {
        let basis_delta = price_i - old_avg;
        let signed = i128::from(old_size.signum() as i64)
            .saturating_mul(closed_qty)
            .saturating_mul(basis_delta);
        signed.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
    } else {
        0
    };

    // ---- New position_size --------------------------------------
    let new_size = old_size.saturating_add(signed_delta);

    // ---- New avg_entry ------------------------------------------
    //
    // Three sub-cases:
    //   (a) Result is flat (new_size == 0): avg_entry is undefined;
    //       we leave the old value in place for telemetry.
    //   (b) The fill opened or grew on the same side: weighted-mean
    //       update.
    //   (c) The fill flipped the position: the remaining (opened)
    //       quantity is on the opposite side, all at fill_price.
    let new_avg = if new_size == 0 {
        // (a) — keep old_avg
        old_avg
    } else if old_size == 0 {
        // (a') opening from flat
        price_i
    } else if old_size.signum() == new_size.signum() {
        if opposite_sign {
            // (b') partial close on the same side as old_size's sign
            // means same direction (long shrinking but not flipping):
            // avg_entry of remainder stays as old_avg.
            old_avg
        } else {
            // (b) increase on same direction — weighted mean.
            let old_abs = old_size.unsigned_abs() as i128;
            let new_notional = old_abs
                .saturating_mul(old_avg)
                .saturating_add(opened_qty.saturating_mul(price_i));
            let total_abs = old_abs.saturating_add(opened_qty);
            if total_abs == 0 {
                old_avg
            } else {
                new_notional / total_abs
            }
        }
    } else {
        // (c) flip — new position is the opened qty leftover after
        // closing, all at fill_price.
        price_i
    };

    account.position_size = PositionSize(
        new_size.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64,
    );
    account.avg_entry = MarkPrice(
        new_avg.clamp(0, i128::from(u64::MAX)) as u64,
    );
    realized
}

/// Initial-margin requirement for `acct`, in quote-currency units.
///
/// `IM_req = |position_size| × avg_entry × im_bps / MARGIN_SCALE`,
/// saturating on overflow.
///
/// **Why `avg_entry` instead of the current mark.** The owning layer
/// (bridge + EVM precompile) needs a single, locally-computable
/// margin denominator. `avg_entry` is the price at which the
/// position's IM was originally posted, so this is literally
/// "initial margin used to open". The trade-off is that a position
/// at a gain still shows the same IM_req as at entry — withdrawing
/// against unrealized gains is *not* allowed in v0. That matches a
/// conservative reading of "free collateral after IM" and avoids
/// dragging a CLOB-derived mark into the precompile's hot path.
///
/// Returns `0` for a flat position (no exposure → no margin).
#[must_use]
pub fn initial_margin_requirement(acct: &Account, im_bps: u32) -> i64 {
    let abs_size = i128::from(acct.position_size.0.unsigned_abs());
    let avg_entry = i128::from(acct.avg_entry.0);
    let bps = i128::from(im_bps);
    let scaled = abs_size
        .saturating_mul(avg_entry)
        .saturating_mul(bps);
    let req = scaled / i128::from(MARGIN_SCALE);
    i64::try_from(req).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(id: u64) -> Account {
        Account::flat(AccountId(id))
    }

    fn long(id: u64, size: i64, entry: u64) -> Account {
        Account {
            account: AccountId(id),
            position_size: PositionSize(size),
            avg_entry: MarkPrice(entry),
            collateral: Notional(0),
        }
    }

    fn short(id: u64, size: i64, entry: u64) -> Account {
        long(id, size, entry) // size already signed by caller
    }

    // ─── case 1: open from flat ────────────────────────────────────

    #[test]
    fn open_long_from_flat() {
        let mut a = acct(1);
        let pnl = apply_fill(&mut a, Price(100), Qty(5), Side::Buy);
        assert_eq!(a.position_size, PositionSize(5));
        assert_eq!(a.avg_entry, MarkPrice(100));
        assert_eq!(pnl, 0);
    }

    #[test]
    fn open_short_from_flat() {
        let mut a = acct(1);
        let pnl = apply_fill(&mut a, Price(100), Qty(5), Side::Sell);
        assert_eq!(a.position_size, PositionSize(-5));
        assert_eq!(a.avg_entry, MarkPrice(100));
        assert_eq!(pnl, 0);
    }

    // ─── case 2: increase same direction ──────────────────────────

    #[test]
    fn increase_long_weighted_avg() {
        // Was long 2 @ 80; buy 5 more at 100.
        // new avg = (2*80 + 5*100) / 7 = (160 + 500) / 7 = 660/7 = 94 (floor).
        let mut a = long(1, 2, 80);
        let pnl = apply_fill(&mut a, Price(100), Qty(5), Side::Buy);
        assert_eq!(a.position_size, PositionSize(7));
        assert_eq!(a.avg_entry, MarkPrice(94));
        assert_eq!(pnl, 0);
    }

    #[test]
    fn increase_short_weighted_avg() {
        // Was short 3 @ 120; sell 2 more at 110.
        // new avg = (3*120 + 2*110) / 5 = (360 + 220) / 5 = 116.
        let mut a = short(1, -3, 120);
        let pnl = apply_fill(&mut a, Price(110), Qty(2), Side::Sell);
        assert_eq!(a.position_size, PositionSize(-5));
        assert_eq!(a.avg_entry, MarkPrice(116));
        assert_eq!(pnl, 0);
    }

    // ─── case 3: partial close opposite direction ──────────────────

    #[test]
    fn partial_close_long_at_profit() {
        // Long 10 @ 80; sell 3 at 100. Realize 3*(100-80) = 60.
        // avg_entry stays at 80.
        let mut a = long(1, 10, 80);
        let pnl = apply_fill(&mut a, Price(100), Qty(3), Side::Sell);
        assert_eq!(a.position_size, PositionSize(7));
        assert_eq!(a.avg_entry, MarkPrice(80));
        assert_eq!(pnl, 60);
    }

    #[test]
    fn partial_close_long_at_loss() {
        // Long 10 @ 100; sell 3 at 80. Realize 3*(80-100) = -60.
        let mut a = long(1, 10, 100);
        let pnl = apply_fill(&mut a, Price(80), Qty(3), Side::Sell);
        assert_eq!(a.position_size, PositionSize(7));
        assert_eq!(a.avg_entry, MarkPrice(100));
        assert_eq!(pnl, -60);
    }

    #[test]
    fn partial_close_short_at_profit() {
        // Short 10 @ 100; buy 3 at 80. Realize 3*(100-80) = 60.
        let mut a = short(1, -10, 100);
        let pnl = apply_fill(&mut a, Price(80), Qty(3), Side::Buy);
        assert_eq!(a.position_size, PositionSize(-7));
        assert_eq!(a.avg_entry, MarkPrice(100));
        assert_eq!(pnl, 60);
    }

    #[test]
    fn partial_close_short_at_loss() {
        // Short 10 @ 80; buy 3 at 100. Realize 3*(80-100) = -60.
        let mut a = short(1, -10, 80);
        let pnl = apply_fill(&mut a, Price(100), Qty(3), Side::Buy);
        assert_eq!(a.position_size, PositionSize(-7));
        assert_eq!(a.avg_entry, MarkPrice(80));
        assert_eq!(pnl, -60);
    }

    // ─── full close to flat ────────────────────────────────────────

    #[test]
    fn full_close_long_at_profit() {
        // Long 5 @ 80; sell 5 at 100. Realize 5*(100-80)=100.
        let mut a = long(1, 5, 80);
        let pnl = apply_fill(&mut a, Price(100), Qty(5), Side::Sell);
        assert_eq!(a.position_size, PositionSize(0));
        // avg_entry retained as telemetry per docstring.
        assert_eq!(a.avg_entry, MarkPrice(80));
        assert_eq!(pnl, 100);
    }

    // ─── case 4: flip ──────────────────────────────────────────────

    #[test]
    fn flip_long_to_short() {
        // Long 3 @ 80; sell 10 at 100.
        // First closes 3 @ 80: realize 3*(100-80)=60.
        // Then opens short 7 @ 100.
        let mut a = long(1, 3, 80);
        let pnl = apply_fill(&mut a, Price(100), Qty(10), Side::Sell);
        assert_eq!(a.position_size, PositionSize(-7));
        assert_eq!(a.avg_entry, MarkPrice(100));
        assert_eq!(pnl, 60);
    }

    #[test]
    fn flip_short_to_long() {
        // Short 3 @ 100; buy 10 at 80.
        // First closes 3 @ 100: realize 3*(100-80)=60.
        // Then opens long 7 @ 80.
        let mut a = short(1, -3, 100);
        let pnl = apply_fill(&mut a, Price(80), Qty(10), Side::Buy);
        assert_eq!(a.position_size, PositionSize(7));
        assert_eq!(a.avg_entry, MarkPrice(80));
        assert_eq!(pnl, 60);
    }

    // ─── invariants ────────────────────────────────────────────────

    #[test]
    fn flat_account_open_then_close_is_zero_realized_if_round_trip_at_same_price() {
        let mut a = acct(1);
        apply_fill(&mut a, Price(100), Qty(5), Side::Buy);
        let pnl = apply_fill(&mut a, Price(100), Qty(5), Side::Sell);
        assert_eq!(a.position_size, PositionSize(0));
        assert_eq!(pnl, 0);
    }

    #[test]
    fn account_id_preserved_across_fills() {
        let mut a = acct(42);
        apply_fill(&mut a, Price(100), Qty(5), Side::Buy);
        apply_fill(&mut a, Price(110), Qty(2), Side::Sell);
        assert_eq!(a.account, AccountId(42));
    }

    // ─── initial_margin_requirement ───────────────────────────────

    #[test]
    fn im_requirement_zero_for_flat_account() {
        let a = acct(1);
        assert_eq!(initial_margin_requirement(&a, DEFAULT_INITIAL_MARGIN_BPS), 0);
    }

    #[test]
    fn im_requirement_long_position_at_default_bps() {
        // 10 contracts × 100 entry × 10% = 100.
        let a = long(1, 10, 100);
        assert_eq!(
            initial_margin_requirement(&a, DEFAULT_INITIAL_MARGIN_BPS),
            100,
        );
    }

    #[test]
    fn im_requirement_short_position_uses_absolute_size() {
        // Short and long of the same |size| × avg_entry share IM_req.
        let s = short(1, -10, 100);
        let l = long(2, 10, 100);
        assert_eq!(
            initial_margin_requirement(&s, DEFAULT_INITIAL_MARGIN_BPS),
            initial_margin_requirement(&l, DEFAULT_INITIAL_MARGIN_BPS),
        );
    }

    #[test]
    fn im_requirement_zero_bps_is_zero() {
        // With a 0 bps rate (config-only edge), even a giant position
        // requires no IM.
        let a = long(1, 1_000_000, 1_000);
        assert_eq!(initial_margin_requirement(&a, 0), 0);
    }

    #[test]
    fn im_requirement_saturates_on_extreme_inputs() {
        // |size| × avg_entry × bps would overflow i64 but stays in
        // i128 until the final cast; we saturate to i64::MAX rather
        // than panicking. Validators disagreeing on overflow forks
        // the chain — bounded behavior is the contract.
        let a = Account {
            account: AccountId(1),
            position_size: PositionSize(i64::MAX),
            avg_entry: MarkPrice(u64::MAX),
            collateral: Notional(0),
        };
        assert_eq!(initial_margin_requirement(&a, u32::MAX), i64::MAX);
    }
}
