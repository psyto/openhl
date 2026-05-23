//! Oracle state machine (Stage 11).
//!
//! Owned by the bridge, mutated on per-block oracle ticks. Holds:
//!   - The per-feed last-observation table (`feeds`).
//!   - The current canonical [`AggregatedPrice`] (`current`).
//!   - The [`OracleParams`] that govern aggregation.
//!
//! ### Invariants
//!
//! - `feeds[k].timestamp` is the publisher-reported time of the
//!   stored observation, not when openhl ingested it. Stored as-is
//!   for staleness checking against block time.
//! - `current` is `Some` only after the first successful [`Self::refresh`].
//!   Until then, the bridge has no trusted price and must either halt
//!   or wait.
//! - All mutations are deterministic functions of `(prior state, input)`;
//!   no clock reads, no map iteration ordering hazards (`BTreeMap`).
//!
//! ### Determinism
//!
//! Two validators with the same prior `OracleState` and the same sequence
//! of `(ingest, refresh)` calls produce byte-identical end states. The
//! caller (bridge) is responsible for delivering the same calls in the
//! same order across the network — typically by including the
//! observations in the block proposal.

use crate::compute::aggregate_index;
use crate::types::{
    AggregatedPrice, AggregationError, FeedId, ObservationError, OracleParams, PriceObservation,
};
use openhl_funding::IndexPrice;
use std::collections::BTreeMap;

/// One feed's most-recent observation, retained between blocks until
/// either replaced by a fresher one or aged out by [`OracleState::refresh`]'s
/// staleness check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedRecord {
    pub last_price: IndexPrice,
    pub last_timestamp: u64,
}

/// Oracle state machine.
///
/// Lifecycle:
///   1. `new(params)` — empty feeds, no current price.
///   2. `ingest(obs, now)` once per inbound observation. Updates the
///      per-feed record, returns `Err` if the observation is stale or
///      malformed.
///   3. `refresh(now)` once per block (or per oracle tick). Drops stale
///      feeds, aggregates the rest, updates `current` on success.
///   4. `current_price()` / `current()` — read the cached aggregate.
///
/// `ingest` accepts duplicate `FeedId` — later observations replace
/// earlier ones from the same publisher. Out-of-order timestamps (older
/// observations arriving after newer ones from the same feed) are
/// rejected; the bridge should order observations before submitting.
#[derive(Clone, Debug)]
pub struct OracleState {
    params: OracleParams,
    feeds: BTreeMap<FeedId, FeedRecord>,
    current: Option<AggregatedPrice>,
}

impl OracleState {
    /// Construct an oracle with no feeds and no current price.
    #[must_use]
    pub const fn new(params: OracleParams) -> Self {
        Self {
            params,
            feeds: BTreeMap::new(),
            current: None,
        }
    }

    /// Borrow the oracle's params.
    #[must_use]
    pub const fn params(&self) -> &OracleParams {
        &self.params
    }

    /// Total number of distinct feeds that have ever submitted an
    /// observation (including ones that may now be stale).
    #[must_use]
    pub fn feed_count(&self) -> usize {
        self.feeds.len()
    }

    /// Number of feeds whose last observation is still inside the
    /// staleness window at `now`.
    #[must_use]
    pub fn fresh_feed_count(&self, now: u64) -> usize {
        self.feeds
            .values()
            .filter(|r| !is_stale(r.last_timestamp, now, self.params.staleness_window_secs))
            .count()
    }

    /// The most recently published [`AggregatedPrice`], if any.
    #[must_use]
    pub const fn current(&self) -> Option<AggregatedPrice> {
        self.current
    }

    /// Convenience accessor: just the index price from `current`.
    #[must_use]
    pub fn current_price(&self) -> Option<IndexPrice> {
        self.current.map(|c| c.index)
    }

    /// Validate and store one observation. Returns an [`ObservationError`]
    /// if the observation is stale, from the future, or has a zero
    /// price.
    ///
    /// On success, replaces the prior observation from the same feed.
    /// An observation older than the stored one for that feed is
    /// rejected as [`ObservationError::Stale`] — this defends against
    /// replay/reordering attacks at the bridge layer.
    pub fn ingest(
        &mut self,
        obs: PriceObservation,
        now: u64,
    ) -> Result<(), ObservationError> {
        // 1. Future timestamps are always rejected (defensive against
        //    clock skew or malicious publishers).
        if obs.timestamp > now {
            return Err(ObservationError::FromFuture {
                observation_ts: obs.timestamp,
                now,
            });
        }
        // 2. Stale: older than the staleness window.
        if is_stale(obs.timestamp, now, self.params.staleness_window_secs) {
            return Err(ObservationError::Stale {
                observation_ts: obs.timestamp,
                now,
                window: self.params.staleness_window_secs,
            });
        }
        // 3. Zero price is non-physical.
        if obs.price.0 == 0 {
            return Err(ObservationError::ZeroPrice);
        }
        // 4. Reject older-than-stored for the same feed (replay guard).
        if let Some(prior) = self.feeds.get(&obs.feed)
            && obs.timestamp < prior.last_timestamp
        {
            return Err(ObservationError::Stale {
                observation_ts: obs.timestamp,
                now,
                window: self.params.staleness_window_secs,
            });
        }

        self.feeds.insert(
            obs.feed,
            FeedRecord {
                last_price: obs.price,
                last_timestamp: obs.timestamp,
            },
        );
        Ok(())
    }

    /// Aggregate the current fresh feeds into a new
    /// [`AggregatedPrice`] and store it in `current`.
    ///
    /// Filters out feeds whose last observation is older than the
    /// staleness window relative to `now`, then runs
    /// [`crate::compute::aggregate_index`] over the survivors.
    ///
    /// On success, returns the new [`AggregatedPrice`] and updates
    /// `current`. On failure, returns the [`AggregationError`]; `current`
    /// is **not** modified — the caller can still read the previous
    /// price via [`Self::current`] if it chooses to be permissive.
    pub fn refresh(&mut self, now: u64) -> Result<AggregatedPrice, AggregationError> {
        let fresh: Vec<IndexPrice> = self
            .feeds
            .values()
            .filter(|r| !is_stale(r.last_timestamp, now, self.params.staleness_window_secs))
            .map(|r| r.last_price)
            .collect();

        let index = aggregate_index(&fresh, &self.params)?;
        let feeds_used = u8::try_from(fresh.len()).unwrap_or(u8::MAX);

        let agg = AggregatedPrice {
            index,
            computed_at: now,
            feeds_used,
        };
        self.current = Some(agg);
        Ok(agg)
    }
}

/// True if `obs_ts` is older than `now - window` (i.e., outside the
/// staleness window). Saturating subtraction handles the underflow case
/// where `now < window` (early in chain history): nothing is stale until
/// time has accumulated.
fn is_stale(obs_ts: u64, now: u64, window: u64) -> bool {
    let cutoff = now.saturating_sub(window);
    obs_ts < cutoff
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn obs(feed: u32, price: u64, ts: u64) -> PriceObservation {
        PriceObservation {
            feed: FeedId(feed),
            price: IndexPrice(price),
            timestamp: ts,
        }
    }

    fn default_params() -> OracleParams {
        OracleParams::hyperliquid_default()
    }

    // ─── construction ─────────────────────────────────────────────

    #[test]
    fn new_oracle_is_empty() {
        let o = OracleState::new(default_params());
        assert_eq!(o.feed_count(), 0);
        assert_eq!(o.current_price(), None);
    }

    // ─── ingest: happy path ───────────────────────────────────────

    #[test]
    fn ingest_records_observation() {
        let mut o = OracleState::new(default_params());
        o.ingest(obs(1, 100, 1000), 1000).unwrap();
        assert_eq!(o.feed_count(), 1);
        assert_eq!(o.fresh_feed_count(1000), 1);
    }

    #[test]
    fn ingest_replaces_prior_from_same_feed() {
        let mut o = OracleState::new(default_params());
        o.ingest(obs(1, 100, 1000), 1000).unwrap();
        o.ingest(obs(1, 105, 1010), 1010).unwrap();
        assert_eq!(o.feed_count(), 1);
        let r = o.feeds.get(&FeedId(1)).unwrap();
        assert_eq!(r.last_price, IndexPrice(105));
        assert_eq!(r.last_timestamp, 1010);
    }

    #[test]
    fn ingest_three_distinct_feeds() {
        let mut o = OracleState::new(default_params());
        o.ingest(obs(1, 100, 1000), 1000).unwrap();
        o.ingest(obs(2, 101, 1000), 1000).unwrap();
        o.ingest(obs(3, 99, 1000), 1000).unwrap();
        assert_eq!(o.feed_count(), 3);
    }

    // ─── ingest: rejections ───────────────────────────────────────

    #[test]
    fn ingest_rejects_future_timestamp() {
        let mut o = OracleState::new(default_params());
        let err = o.ingest(obs(1, 100, 2000), 1000).unwrap_err();
        assert!(matches!(err, ObservationError::FromFuture { .. }));
    }

    #[test]
    fn ingest_rejects_stale() {
        // Default staleness window = 60s. Observation at ts=900, now=1000
        // → age 100s > window → reject.
        let mut o = OracleState::new(default_params());
        let err = o.ingest(obs(1, 100, 900), 1000).unwrap_err();
        assert!(matches!(err, ObservationError::Stale { .. }));
    }

    #[test]
    fn ingest_rejects_zero_price() {
        let mut o = OracleState::new(default_params());
        let err = o.ingest(obs(1, 0, 1000), 1000).unwrap_err();
        assert_eq!(err, ObservationError::ZeroPrice);
    }

    #[test]
    fn ingest_rejects_older_than_stored_from_same_feed() {
        // Replay guard: feed 1 submitted ts=1010 then attempts ts=1005.
        let mut o = OracleState::new(default_params());
        o.ingest(obs(1, 105, 1010), 1010).unwrap();
        let err = o.ingest(obs(1, 100, 1005), 1010).unwrap_err();
        assert!(matches!(err, ObservationError::Stale { .. }));
    }

    #[test]
    fn ingest_at_exact_window_boundary_is_accepted() {
        // window = 60; ts = now - 60 should be on the edge.
        // `is_stale` uses `obs_ts < cutoff` (strict), so obs_ts == cutoff
        // passes.
        let mut o = OracleState::new(default_params());
        // now=1000, window=60, cutoff=940. obs_ts=940 → not stale.
        assert!(o.ingest(obs(1, 100, 940), 1000).is_ok());
    }

    // ─── refresh: happy paths ─────────────────────────────────────

    #[test]
    fn refresh_with_three_clean_feeds() {
        let mut o = OracleState::new(default_params());
        o.ingest(obs(1, 100, 1000), 1000).unwrap();
        o.ingest(obs(2, 101, 1000), 1000).unwrap();
        o.ingest(obs(3, 102, 1000), 1000).unwrap();
        let agg = o.refresh(1000).unwrap();
        assert_eq!(agg.index, IndexPrice(101));
        assert_eq!(agg.feeds_used, 3);
        assert_eq!(agg.computed_at, 1000);
        assert_eq!(o.current_price(), Some(IndexPrice(101)));
    }

    #[test]
    fn refresh_filters_stale_feeds() {
        let mut o = OracleState::new(default_params());
        // Three feeds at ts=1000, then refresh at now=1100 (60s after
        // last fresh boundary at 1040). Window=60, so feeds older than
        // ts=1040 are stale.
        o.ingest(obs(1, 100, 1000), 1000).unwrap();
        o.ingest(obs(2, 101, 1000), 1000).unwrap();
        o.ingest(obs(3, 102, 1050), 1050).unwrap();
        // At now=1100, cutoff=1040. Feed 1 (ts 1000) and feed 2 (ts 1000)
        // are stale; only feed 3 (ts 1050) is fresh.
        let result = o.refresh(1100);
        // Only 1 fresh < required 2 → TooFewFreshFeeds.
        assert!(matches!(
            result,
            Err(AggregationError::TooFewFreshFeeds { fresh: 1, required: 2 })
        ));
        // current was None before this attempt; refresh failure must not
        // populate it.
        assert_eq!(o.current_price(), None);
    }

    #[test]
    fn refresh_preserves_prior_current_on_failure() {
        let mut o = OracleState::new(default_params());
        o.ingest(obs(1, 100, 1000), 1000).unwrap();
        o.ingest(obs(2, 101, 1000), 1000).unwrap();
        let first = o.refresh(1000).unwrap();
        assert_eq!(first.index, IndexPrice(100)); // (100+101)/2 = 100

        // Time passes; both feeds go stale. refresh should fail but
        // leave `current` pointing at the prior price (permissive fallback).
        let result = o.refresh(1500);
        assert!(result.is_err());
        assert_eq!(o.current_price(), Some(IndexPrice(100)));
    }

    #[test]
    fn refresh_recomputes_on_filter_dropping_outlier() {
        // Feeds: 100, 101, 200. Initial median = 101.
        //   100: 100 bps off ≤ 100 cap → kept
        //   101: 0 bps → kept
        //   200: 9_900 bps off → DROPPED
        // Final median over [100, 101] = (100+101)/2 = 100.
        let mut o = OracleState::new(default_params());
        o.ingest(obs(1, 100, 1000), 1000).unwrap();
        o.ingest(obs(2, 101, 1000), 1000).unwrap();
        o.ingest(obs(3, 200, 1000), 1000).unwrap();
        let agg = o.refresh(1000).unwrap();
        assert_eq!(agg.index, IndexPrice(100));
        // feeds_used reports total fresh feeds, not post-filter
        assert_eq!(agg.feeds_used, 3);
    }

    // ─── refresh: rejections ──────────────────────────────────────

    #[test]
    fn refresh_empty_oracle_returns_too_few_fresh() {
        let mut o = OracleState::new(default_params());
        let result = o.refresh(1000);
        assert!(matches!(
            result,
            Err(AggregationError::TooFewFreshFeeds { fresh: 0, required: 2 })
        ));
    }

    // ─── proptest: invariants ─────────────────────────────────────

    proptest! {
        /// Successful refresh always sets `current` to the returned
        /// AggregatedPrice.
        #[test]
        fn successful_refresh_sets_current(
            prices in proptest::collection::vec(1_u64..1_000_000, 2..8),
        ) {
            let mut o = OracleState::new(default_params());
            for (i, p) in prices.iter().enumerate() {
                o.ingest(obs(u32::try_from(i).unwrap(), *p, 1000), 1000).unwrap();
            }
            if let Ok(agg) = o.refresh(1000) {
                prop_assert_eq!(o.current(), Some(agg));
            }
        }

        /// `fresh_feed_count(now)` ≤ `feed_count()` for any `now`.
        #[test]
        fn fresh_count_bounded_by_total(
            prices in proptest::collection::vec(1_u64..1_000_000, 0..10),
            now in 0_u64..10_000,
        ) {
            let mut o = OracleState::new(default_params());
            for (i, p) in prices.iter().enumerate() {
                let _ = o.ingest(obs(u32::try_from(i).unwrap(), *p, 1000), 1000);
            }
            prop_assert!(o.fresh_feed_count(now) <= o.feed_count());
        }

        /// Determinism: same params + same ingest sequence + same
        /// refresh time → same end state.
        #[test]
        fn ingest_refresh_is_deterministic(
            prices in proptest::collection::vec(1_u64..1_000_000, 2..6),
        ) {
            let build = || {
                let mut o = OracleState::new(default_params());
                for (i, p) in prices.iter().enumerate() {
                    let _ = o.ingest(obs(u32::try_from(i).unwrap(), *p, 1000), 1000);
                }
                let r = o.refresh(1000);
                (o.current_price(), r)
            };
            prop_assert_eq!(build(), build());
        }
    }
}
