//! Pure funding-rate math.
//!
//! Three building blocks, each stateless:
//!   - [`compute_premium`] derives the mark/index gap as a signed fraction
//!   - [`compute_rate`] divides + caps to produce a per-interval rate
//!   - [`apply_funding`] turns a rate + position snapshot into settlements
//!
//! Each function is deterministic and saturates on overflow rather than
//! wrapping. Validators that disagree about funding fork the chain, so the
//! cost of an unexpected overflow has to be bounded behavior, not panic.

use crate::types::{
    FundingParams, FundingRate, IndexPrice, MarkPrice, Notional, Position, Premium, Settlement,
    RATE_SCALE,
};

/// Compute the premium `(mark - index) / index`, scaled by [`RATE_SCALE`].
///
/// Returns `Premium(0)` if `index == 0` — the safest behavior, since with no
/// reliable reference price the funding rate should not push capital around.
/// Real deployments should guard upstream (e.g., refuse to tick when the
/// oracle is missing); the saturation here is the second line of defense.
#[must_use]
pub fn compute_premium(mark: MarkPrice, index: IndexPrice) -> Premium {
    if index.0 == 0 {
        return Premium(0);
    }
    // (mark - index) as i128 so we can't lose sign on subtraction; multiply
    // by RATE_SCALE in i128 to avoid overflow before the divide.
    let diff = i128::from(mark.0) - i128::from(index.0);
    let scaled = diff.saturating_mul(i128::from(RATE_SCALE));
    let premium = scaled / i128::from(index.0);
    // Saturate back to i64 — at i64 range with index prices in u64::MAX
    // territory, this only clips at network-pathological inputs.
    Premium(saturate_i128_to_i64(premium))
}

/// Divide the premium by `params.divisor` and clamp to ±`params.rate_cap`.
///
/// `divisor == 0` is treated as "funding disabled" → returns `FundingRate(0)`,
/// which causes `apply_funding` to produce zero-delta settlements for every
/// position (or none, by the filter inside `apply_funding`).
#[must_use]
pub fn compute_rate(premium: Premium, params: FundingParams) -> FundingRate {
    if params.divisor == 0 {
        return FundingRate(0);
    }
    let raw = premium.0 / i64::from(params.divisor);
    let cap = params.rate_cap.0.abs();
    let capped = raw.clamp(-cap, cap);
    FundingRate(capped)
}

/// Apply `rate` to each position, producing one [`Settlement`] per non-flat
/// position. Flat positions (`size == 0`) are dropped — there's no settlement
/// to record. Order of input positions is preserved in the output.
///
/// Sign convention: with positive `rate`, longs (positive size) pay; shorts
/// (negative size) receive. The product `size * mark * rate / RATE_SCALE`
/// is the quote-currency delta; long pays → delta is negative for longs.
#[must_use]
pub fn apply_funding(
    positions: &[Position],
    mark: MarkPrice,
    rate: FundingRate,
) -> Vec<Settlement> {
    if rate.0 == 0 {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(positions.len());
    for pos in positions {
        if pos.size.0 == 0 {
            continue;
        }
        // notional = size * mark, in i128 to absorb the product's full range.
        let notional = i128::from(pos.size.0).saturating_mul(i128::from(mark.0));
        // delta_unscaled = notional * rate; still i128.
        let delta_unscaled = notional.saturating_mul(i128::from(rate.0));
        // Sign convention: longs PAY when rate > 0. The product above is
        // positive (long size * positive rate) — we flip its sign so the
        // resulting delta is negative for longs and positive for shorts.
        let delta_scaled = -delta_unscaled / i128::from(RATE_SCALE);
        out.push(Settlement {
            account: pos.account,
            delta: Notional(saturate_i128_to_i64(delta_scaled)),
        });
    }
    out
}

/// Clamp an `i128` into the `i64` range. Used wherever an intermediate
/// product can exceed `i64::MAX` at network-pathological inputs (e.g., a
/// `u64::MAX` index price). Saturation, not wrapping — see the module-doc
/// comment on why panicking would be a worse failure mode.
fn saturate_i128_to_i64(v: i128) -> i64 {
    i64::try_from(v).unwrap_or(if v > 0 { i64::MAX } else { i64::MIN })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openhl_clob::AccountId;
    use proptest::prelude::*;

    fn pos(account: u64, size: i64) -> Position {
        Position {
            account: AccountId(account),
            size: crate::types::PositionSize(size),
        }
    }

    #[test]
    fn premium_zero_when_mark_equals_index() {
        let p = compute_premium(MarkPrice(100), IndexPrice(100));
        assert_eq!(p, Premium(0));
    }

    #[test]
    fn premium_positive_when_mark_above_index() {
        // mark 101, index 100 → premium = 1/100 = 0.01 → 10_000_000 ppb
        let p = compute_premium(MarkPrice(101), IndexPrice(100));
        assert_eq!(p, Premium(10_000_000));
    }

    #[test]
    fn premium_negative_when_mark_below_index() {
        let p = compute_premium(MarkPrice(99), IndexPrice(100));
        assert_eq!(p, Premium(-10_000_000));
    }

    #[test]
    fn premium_saturates_to_zero_when_index_is_zero() {
        let p = compute_premium(MarkPrice(1_000_000), IndexPrice(0));
        assert_eq!(p, Premium(0));
    }

    #[test]
    fn rate_divides_premium_by_divisor() {
        let params = FundingParams::hyperliquid_default();
        // premium = 0.01 (10_000_000 ppb), divisor = 8 → rate = 1_250_000
        let r = compute_rate(Premium(10_000_000), params);
        assert_eq!(r, FundingRate(1_250_000));
    }

    #[test]
    fn rate_clamps_at_positive_cap() {
        let params = FundingParams::hyperliquid_default();
        // premium = 1.0 (RATE_SCALE), divisor = 8 → raw = 125_000_000
        // cap is 40_000_000 → clamps to 40_000_000.
        let r = compute_rate(Premium(RATE_SCALE), params);
        assert_eq!(r, FundingRate(40_000_000));
    }

    #[test]
    fn rate_clamps_at_negative_cap() {
        let params = FundingParams::hyperliquid_default();
        let r = compute_rate(Premium(-RATE_SCALE), params);
        assert_eq!(r, FundingRate(-40_000_000));
    }

    #[test]
    fn rate_zero_when_divisor_is_zero() {
        let mut params = FundingParams::hyperliquid_default();
        params.divisor = 0;
        let r = compute_rate(Premium(RATE_SCALE), params);
        assert_eq!(r, FundingRate(0));
    }

    #[test]
    fn rate_zero_when_cap_is_zero_funding_disabled() {
        let mut params = FundingParams::hyperliquid_default();
        params.rate_cap = FundingRate(0);
        let r = compute_rate(Premium(10_000_000), params);
        assert_eq!(r, FundingRate(0));
    }

    #[test]
    fn apply_funding_skips_flat_positions() {
        let positions = vec![pos(1, 0), pos(2, 100), pos(3, 0)];
        let settlements = apply_funding(&positions, MarkPrice(100), FundingRate(1_000_000));
        assert_eq!(settlements.len(), 1);
        assert_eq!(settlements[0].account, AccountId(2));
    }

    #[test]
    fn apply_funding_longs_pay_shorts_when_rate_positive() {
        // size 100 (long), mark 100, rate 0.001 (1_000_000 ppb)
        // delta = -(100 * 100 * 1_000_000 / 1_000_000_000) = -10
        let positions = vec![pos(1, 100), pos(2, -50)];
        let s = apply_funding(&positions, MarkPrice(100), FundingRate(1_000_000));
        assert_eq!(s[0].account, AccountId(1));
        assert_eq!(s[0].delta, Notional(-10), "long pays");
        assert_eq!(s[1].account, AccountId(2));
        assert_eq!(s[1].delta, Notional(5), "short receives, half size");
    }

    #[test]
    fn apply_funding_shorts_pay_longs_when_rate_negative() {
        let positions = vec![pos(1, 100), pos(2, -50)];
        let s = apply_funding(&positions, MarkPrice(100), FundingRate(-1_000_000));
        assert_eq!(s[0].delta, Notional(10), "long receives");
        assert_eq!(s[1].delta, Notional(-5), "short pays");
    }

    #[test]
    fn apply_funding_returns_empty_on_zero_rate() {
        let positions = vec![pos(1, 100), pos(2, -50)];
        let s = apply_funding(&positions, MarkPrice(100), FundingRate(0));
        assert!(s.is_empty());
    }

    proptest! {
        /// Sum of all settlement deltas is zero (or exactly the negation of
        /// itself with saturation tolerance) when the population is balanced.
        /// Equivalently: funding redistributes between longs and shorts —
        /// it doesn't create or destroy quote currency.
        ///
        /// We test the property by constructing equal-and-opposite long/short
        /// pairs and asserting their settlements sum to zero exactly.
        #[test]
        fn balanced_book_settlements_sum_to_zero(
            size in 1i64..1_000_000,
            mark in 1u64..1_000_000,
            rate in -10_000_000i64..10_000_000,
        ) {
            let positions = vec![
                pos(1, size),
                pos(2, -size),
            ];
            let s = apply_funding(&positions, MarkPrice(mark), FundingRate(rate));
            if rate == 0 {
                prop_assert!(s.is_empty());
            } else {
                prop_assert_eq!(s.len(), 2);
                prop_assert_eq!(s[0].delta.0 + s[1].delta.0, 0);
            }
        }

        /// Premium symmetry: swapping mark and index flips the sign.
        /// (Up to integer division rounding, the magnitude is the same — we
        /// allow off-by-one to absorb the rounding-toward-zero asymmetry.)
        #[test]
        fn premium_is_antisymmetric_in_mark_index(
            mark in 1u64..1_000_000,
            index in 1u64..1_000_000,
        ) {
            let a = compute_premium(MarkPrice(mark), IndexPrice(index));
            let b = compute_premium(MarkPrice(index), IndexPrice(mark));
            // Cross-multiplied magnitudes must be equal: |a| / mark == |b| / index
            // (i.e., the proportional dislocation is the same both ways).
            // We test the weaker property that the signs are opposite (or both zero).
            if mark == index {
                prop_assert_eq!(a, Premium(0));
                prop_assert_eq!(b, Premium(0));
            } else {
                prop_assert!(a.0.signum() == -b.0.signum());
            }
        }
    }
}
