//! `openhl-oracle` — index-price aggregation for the perpetual market.
//!
//! Pure compute + small state machine, same architectural shape as
//! `openhl-funding` and `openhl-liquidation`: `types → compute → state`.
//! Every validator must arrive at the same [`AggregatedPrice`] from the
//! same observation stream, so all arithmetic is integer + saturating
//! and every aggregation choice is deterministic.
//!
//! ### What an oracle does, in one paragraph
//!
//! Multiple external publishers (typically major-CEX spot feeds) submit
//! [`PriceObservation`]s for the same asset. The oracle filters out
//! stale and zero-price observations at ingestion, then once per block
//! drops feeds that deviate too far from the median (single-feed
//! manipulation defense) and computes the median over the survivors.
//! The result is the trusted [`IndexPrice`](openhl_funding::IndexPrice)
//! that `openhl_funding` consumes against the CLOB mark to compute
//! funding rates, and that `openhl_liquidation` could optionally use
//! for cross-checking the CLOB's mark in stress scenarios.
//!
//! ### Stage 11 scope
//!
//! - [`compute::compute_median`] — deterministic median (sort + middle).
//! - [`compute::deviation_bps`] — `|p − ref| / ref` in basis points.
//! - [`compute::aggregate_index`] — median → deviation filter → median.
//! - [`state::OracleState`] — per-feed observation table + cached
//!   `current` [`AggregatedPrice`], updated by `ingest` and `refresh`.
//!
//! ### Stage 11b additions
//!
//! - **Signed observations** ([`crate::verify::verify_observation`]) —
//!   each [`PriceObservation`] now carries a 64-byte fixed-format
//!   ECDSA signature (secp256k1) over the canonical
//!   `feed_id || price || timestamp` byte sequence.
//! - **Publisher registry** — [`OracleState`] holds a
//!   `BTreeMap<FeedId, PublisherKey>` populated via
//!   [`OracleState::register_publisher`]. [`OracleState::ingest_signed`]
//!   verifies each observation against the registered key before any
//!   staleness / replay checks; unverified observations are rejected
//!   with [`ObservationError::InvalidSignature`].
//! - **Two ingest paths** — [`OracleState::ingest`] remains for
//!   trusted-bridge deployments and tests (signature field ignored);
//!   [`OracleState::ingest_signed`] is the production path. Both paths
//!   can coexist in one [`OracleState`] instance — some feeds signed,
//!   others trusted.
//!
//! ### Out of scope (future work)
//!
//! - **Weighted mean.** Production oracle services often use a
//!   per-feed-weighted mean rather than median. Median is the v0 choice
//!   because (a) it's robust to single-feed manipulation by design and
//!   (b) it needs no per-feed-weight parameters. Adding a `weights`
//!   field to [`OracleParams`] and an `aggregate_weighted_mean`
//!   function is a forward-compatible extension.
//! - **Per-market scoping.** [`OracleState`] is implicitly per-market.
//!   Multi-market deployments instantiate one state per market and the
//!   bridge owns the `(MarketId, OracleState)` mapping.
//! - **Key rotation policy.** [`OracleState::register_publisher`]
//!   replaces the prior key for a feed in one call, supporting
//!   manual rotation. Automated rotation cadence (timestamps,
//!   overlapping validity windows for graceful key handoff) is a
//!   future hardening item.
//!
//! ### Why fixed-point integers, not floats
//!
//! Same answer as `openhl-funding` and `openhl-liquidation`: consensus
//! determinism. Every validator must arrive at the same aggregated
//! index from the same observations, and float arithmetic varies
//! bit-for-bit across compilers and CPUs.

pub mod compute;
pub mod state;
pub mod types;
pub mod verify;

pub use compute::{aggregate_index, compute_median, deviation_bps, filter_by_deviation};
pub use state::{FeedRecord, OracleState};
pub use types::{
    AggregatedPrice, AggregationError, FeedId, ObservationError, OracleParams, PriceObservation,
    PublisherKey, Signature, DEVIATION_SCALE,
};
pub use verify::verify_observation;
