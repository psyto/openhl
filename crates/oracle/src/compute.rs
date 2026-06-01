//! Pure aggregation math (Stage 11).
//!
//! Four building blocks, all stateless:
//!   - [`compute_median`] — deterministic median of a slice of prices
//!   - [`deviation_bps`] — `|price - reference| / reference`, in bps
//!   - [`filter_by_deviation`] — drop feeds outside the deviation cap
//!   - [`aggregate_index`] — the full pipeline: filter → median →
//!     refilter → final median
//!
//! Determinism rules: no floats, no `HashMap` iteration, no time-based
//! ordering. Median sorts a local `Vec<u64>` ascending; ties resolve by
//! "first occurrence in the input slice." Two validators feeding the
//! same observation slice arrive at byte-identical results.

use crate::types::{AggregationError, DEVIATION_SCALE, OracleParams};
use openhl_funding::IndexPrice;

/// Deterministic median of a non-empty slice of prices.
///
/// Sorts a local copy ascending and returns the middle element for
/// odd-length slices, or the integer mean of the two middle elements
/// for even-length slices. Uses `u128` intermediates so even
/// `u64::MAX + u64::MAX` doesn't overflow.
///
/// Returns `None` for an empty input (median is undefined).
#[must_use]
pub fn compute_median(prices: &[IndexPrice]) -> Option<IndexPrice> {
    if prices.is_empty() {
        return None;
    }
    let mut sorted: Vec<u64> = prices.iter().map(|p| p.0).collect();
    sorted.sort_unstable();
    let n = sorted.len();
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        let lo = u128::from(sorted[n / 2 - 1]);
        let hi = u128::from(sorted[n / 2]);
        // `u128::midpoint` is the overflow-safe equivalent of `(lo + hi) / 2`.
        let avg = u128::midpoint(lo, hi);
        u64::try_from(avg).unwrap_or(u64::MAX)
    };
    Some(IndexPrice(median))
}

/// Deviation of `price` from `reference`, in basis points.
///
/// `deviation_bps = |price − reference| × DEVIATION_SCALE / reference`.
///
/// Returns `u32::MAX` for a zero `reference` (deviation is undefined; we
/// treat zero-reference as "everything deviates infinitely"). Saturates
/// at `u32::MAX` for pathological inputs; in practice the bridge would
/// have rejected a u64-overflowing diff long before it reached the
/// oracle.
#[must_use]
pub fn deviation_bps(price: IndexPrice, reference: IndexPrice) -> u32 {
    if reference.0 == 0 {
        return u32::MAX;
    }
    let diff = price.0.abs_diff(reference.0);
    let bps = u128::from(diff).saturating_mul(u128::from(DEVIATION_SCALE)) / u128::from(reference.0);
    u32::try_from(bps).unwrap_or(u32::MAX)
}

/// Filter feeds to those whose price is within `max_deviation_bps` of
/// `reference`. Returns a fresh `Vec` — Stage 11 prioritises clarity
/// over alloc-free hot paths; the bridge's per-block oracle refresh is
/// not a hot path.
#[must_use]
pub fn filter_by_deviation(
    prices: &[IndexPrice],
    reference: IndexPrice,
    max_deviation_bps: u32,
) -> Vec<IndexPrice> {
    prices
        .iter()
        .copied()
        .filter(|p| deviation_bps(*p, reference) <= max_deviation_bps)
        .collect()
}

/// Aggregate a slice of fresh observations into a single index price.
///
/// Two-pass pipeline mirroring real oracle services:
///   1. Initial median over **all** fresh feeds.
///   2. Drop feeds deviating beyond `max_deviation_bps` from the initial
///      median.
///   3. Final median over the remaining feeds.
///
/// The caller is responsible for filtering out **stale** observations
/// before calling — this function trusts every input is fresh.
///
/// Returns an [`AggregationError`] if too few feeds qualify either
/// before (`TooFewFreshFeeds`) or after (`TooFewAfterDeviationFilter`)
/// the deviation step. The caller must decide whether to halt the chain
/// or fall back to the previous price.
pub fn aggregate_index(
    fresh: &[IndexPrice],
    params: &OracleParams,
) -> Result<IndexPrice, AggregationError> {
    let fresh_count = u8::try_from(fresh.len()).unwrap_or(u8::MAX);
    if fresh_count < params.min_feeds_required {
        return Err(AggregationError::TooFewFreshFeeds {
            fresh: fresh_count,
            required: params.min_feeds_required,
        });
    }

    // Step 1: initial median across all fresh feeds. Safe to unwrap —
    // `fresh.len() >= min_feeds_required >= 0`, and if min_feeds_required
    // is 0 the early return above didn't fire so fresh.len() > 0 too…
    // not quite — min_feeds_required = 0 plus fresh = empty would skip
    // the early return. Guard against that explicitly:
    let initial = compute_median(fresh).ok_or(AggregationError::TooFewFreshFeeds {
        fresh: 0,
        required: params.min_feeds_required,
    })?;

    // Step 2: drop deviators.
    let filtered = filter_by_deviation(fresh, initial, params.max_deviation_bps);
    let filtered_count = u8::try_from(filtered.len()).unwrap_or(u8::MAX);
    if filtered_count < params.min_feeds_required {
        return Err(AggregationError::TooFewAfterDeviationFilter {
            remaining: filtered_count,
            required: params.min_feeds_required,
        });
    }

    // Step 3: final median over the filtered set.
    compute_median(&filtered).ok_or(AggregationError::TooFewAfterDeviationFilter {
        remaining: 0,
        required: params.min_feeds_required,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn ip(p: u64) -> IndexPrice {
        IndexPrice(p)
    }

    // ─── compute_median ───────────────────────────────────────────

    #[test]
    fn median_empty_is_none() {
        assert_eq!(compute_median(&[]), None);
    }

    #[test]
    fn median_single() {
        assert_eq!(compute_median(&[ip(100)]), Some(ip(100)));
    }

    #[test]
    fn median_odd_count() {
        // Sorted: 100, 105, 110 → median 105
        assert_eq!(compute_median(&[ip(110), ip(100), ip(105)]), Some(ip(105)));
    }

    #[test]
    fn median_even_count() {
        // Sorted: 100, 110 → (100 + 110) / 2 = 105
        assert_eq!(compute_median(&[ip(110), ip(100)]), Some(ip(105)));
    }

    #[test]
    fn median_even_floor_rounding() {
        // Sorted: 100, 101 → (100 + 101) / 2 = 100 (integer divide toward zero)
        assert_eq!(compute_median(&[ip(100), ip(101)]), Some(ip(100)));
    }

    #[test]
    fn median_robust_to_outlier() {
        // Sorted: 100, 101, 102, 10000 → median = (101+102)/2 = 101
        // A mean would be ~2576; median's robustness is the point.
        assert_eq!(
            compute_median(&[ip(10000), ip(100), ip(102), ip(101)]),
            Some(ip(101))
        );
    }

    #[test]
    fn median_large_values_no_overflow() {
        // Even-length with values near u64::MAX — u128 intermediate must
        // absorb the sum.
        let a = u64::MAX - 1;
        let b = u64::MAX;
        let expected = u64::try_from(u128::midpoint(u128::from(a), u128::from(b)))
            .expect("midpoint of two u64s fits in u64");
        assert_eq!(compute_median(&[ip(b), ip(a)]), Some(ip(expected)));
    }

    // ─── deviation_bps ────────────────────────────────────────────

    #[test]
    fn deviation_zero_when_equal() {
        assert_eq!(deviation_bps(ip(100), ip(100)), 0);
    }

    #[test]
    fn deviation_symmetric_above_and_below() {
        // 1% above and 1% below reference both compute as 100 bps.
        assert_eq!(deviation_bps(ip(101), ip(100)), 100);
        assert_eq!(deviation_bps(ip(99), ip(100)), 100);
    }

    #[test]
    fn deviation_50_percent() {
        // 150 vs 100 → 50% = 5_000 bps
        assert_eq!(deviation_bps(ip(150), ip(100)), 5_000);
    }

    #[test]
    fn deviation_zero_reference_returns_max() {
        assert_eq!(deviation_bps(ip(100), ip(0)), u32::MAX);
    }

    // ─── filter_by_deviation ──────────────────────────────────────

    #[test]
    fn filter_keeps_everything_within_cap() {
        // ref = 100, cap = 500 bps (5%). 95, 100, 105 all within cap.
        let kept = filter_by_deviation(&[ip(95), ip(100), ip(105)], ip(100), 500);
        assert_eq!(kept.len(), 3);
    }

    #[test]
    fn filter_drops_outlier() {
        // ref = 100, cap = 100 bps (1%). 95 deviates 5% → dropped.
        let kept = filter_by_deviation(&[ip(95), ip(100), ip(101)], ip(100), 100);
        assert_eq!(kept, vec![ip(100), ip(101)]);
    }

    #[test]
    fn filter_keeps_exactly_at_cap() {
        // ref = 100, cap = 100 bps (1%). 101 deviates exactly 100 bps.
        // Cap is inclusive (`deviation ≤ cap`).
        let kept = filter_by_deviation(&[ip(101)], ip(100), 100);
        assert_eq!(kept, vec![ip(101)]);
    }

    // ─── aggregate_index ──────────────────────────────────────────

    fn default_params() -> OracleParams {
        OracleParams::hyperliquid_default()
    }

    #[test]
    fn aggregate_three_clean_feeds() {
        // 100, 101, 102 → median 101, all within 100 bps of 101 → 101.
        let agg = aggregate_index(&[ip(100), ip(101), ip(102)], &default_params()).unwrap();
        assert_eq!(agg, ip(101));
    }

    #[test]
    fn aggregate_drops_outlier_and_recomputes() {
        // 100, 101, 105 → initial median 101.
        //   100: 100 bps off ≤ 100 cap → kept
        //   101: 0 bps → kept
        //   105: ~396 bps off > 100 → DROPPED
        // Filtered set: [100, 101] → median (100+101)/2 = 100
        let agg = aggregate_index(&[ip(100), ip(101), ip(105)], &default_params()).unwrap();
        assert_eq!(agg, ip(100));
    }

    #[test]
    fn aggregate_rejects_too_few_fresh() {
        // params.min_feeds_required = 2; only 1 input.
        let result = aggregate_index(&[ip(100)], &default_params());
        assert_eq!(
            result,
            Err(AggregationError::TooFewFreshFeeds {
                fresh: 1,
                required: 2,
            })
        );
    }

    #[test]
    fn aggregate_rejects_when_filter_strands_quorum() {
        // 100, 200, 300 with min 2 and cap 100 bps.
        //   initial median = 200 (middle of sorted [100, 200, 300]).
        //   100: 5000 bps off > 100 → dropped
        //   200: 0 bps → kept
        //   300: 5000 bps off → dropped
        //   Filtered: [200] → only 1 left, < required 2.
        let result = aggregate_index(&[ip(100), ip(200), ip(300)], &default_params());
        assert_eq!(
            result,
            Err(AggregationError::TooFewAfterDeviationFilter {
                remaining: 1,
                required: 2,
            })
        );
    }

    #[test]
    fn aggregate_with_zero_min_feeds_handles_empty() {
        // Defensive: even with min_feeds_required = 0, an empty input
        // can't produce a median, so the function must surface
        // TooFewFreshFeeds.
        let params = OracleParams {
            staleness_window_secs: 60,
            min_feeds_required: 0,
            max_deviation_bps: 100,
            aggregate_max_age_secs: 60,
        };
        let result = aggregate_index(&[], &params);
        assert!(matches!(
            result,
            Err(AggregationError::TooFewFreshFeeds { .. })
        ));
    }

    // ─── proptest: determinism + monotonicity ─────────────────────

    proptest! {
        /// `compute_median` is invariant under permutations of the input.
        #[test]
        fn median_is_permutation_invariant(
            mut prices in proptest::collection::vec(1_u64..1_000_000_000, 1..20),
        ) {
            let original: Vec<IndexPrice> = prices.iter().map(|p| ip(*p)).collect();
            let med1 = compute_median(&original);

            // Reverse the input — median must not change.
            prices.reverse();
            let permuted: Vec<IndexPrice> = prices.iter().map(|p| ip(*p)).collect();
            let med2 = compute_median(&permuted);

            prop_assert_eq!(med1, med2);
        }

        /// `deviation_bps(p, p) == 0` for any non-zero `p`.
        #[test]
        fn deviation_self_is_zero(p in 1_u64..1_000_000_000) {
            prop_assert_eq!(deviation_bps(ip(p), ip(p)), 0);
        }

        /// Determinism: same input twice → same aggregate.
        #[test]
        fn aggregate_is_deterministic(
            prices in proptest::collection::vec(1_u64..1_000_000, 2..10),
        ) {
            let feeds: Vec<IndexPrice> = prices.iter().map(|p| ip(*p)).collect();
            let r1 = aggregate_index(&feeds, &default_params());
            let r2 = aggregate_index(&feeds, &default_params());
            prop_assert_eq!(r1, r2);
        }
    }
}
