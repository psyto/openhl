//! Core types for the oracle (Stage 11).
//!
//! Pure data — every type is `Copy`-friendly so the aggregation engine
//! can be invoked on stack-allocated observation slices without lifetime
//! gymnastics. Follows the openhl convention: the crate never owns
//! mutable state in its types; mutation lives in [`crate::state`].
//!
//! ### What an oracle does, in one paragraph
//!
//! A perpetual-DEX index oracle aggregates spot-price observations from
//! several external publishers (typically major-CEX feeds) into one
//! canonical index price the rest of the system trusts. The aggregation
//! must be deterministic across validators (every node arrives at the
//! same number from the same inputs), robust to single-feed manipulation
//! (one bad publisher shouldn't move the index), and bounded against
//! stale data (a feed that hasn't updated in N seconds is dropped). The
//! result is what `openhl_funding::compute_premium` consumes against the
//! CLOB-derived mark price to compute the per-interval funding rate.

use openhl_funding::IndexPrice;

/// Bps scale factor. 1 bp = 0.01%; `DEVIATION_SCALE` = 10⁴ means
/// `100% = 10_000 bps`. Mirrors `MARGIN_SCALE` from `openhl-liquidation`
/// to keep all openhl `× / 10_000` arithmetic at the same magnitude.
pub const DEVIATION_SCALE: u32 = 10_000;

/// Identifier for a price publisher. The integer carries no semantics —
/// the bridge picks per-deployment IDs (e.g., 1 = Binance spot, 2 =
/// Coinbase spot, 3 = OKX spot). Determinism only requires that all
/// validators agree on the mapping; openhl doesn't dictate it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FeedId(pub u32);

/// SEC1-compressed secp256k1 public key (33 bytes).
///
/// Registered against a [`FeedId`] in [`crate::state::OracleState`].
/// Stage 11b verifies each [`PriceObservation::signature`] against the
/// registered key for that feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PublisherKey(pub [u8; 33]);

/// IEEE P1363 fixed-format ECDSA signature: `r || s` concatenated, 64
/// bytes. `Signature::ZERO` is the placeholder for unsigned observations
/// (those ingested via [`crate::state::OracleState::ingest`], the
/// unsigned-trust path — production callers should use
/// [`crate::state::OracleState::ingest_signed`] which verifies the
/// signature against the publisher registry).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Signature(pub [u8; 64]);

impl Signature {
    /// All-zero signature, used as the placeholder in unsigned
    /// observations and as a default for serialization round-trips.
    pub const ZERO: Self = Self([0u8; 64]);
}

impl Default for Signature {
    fn default() -> Self {
        Self::ZERO
    }
}

/// One price observation from one publisher, at one timestamp.
///
/// The `signature` field is verified against the registered
/// [`PublisherKey`] for the feed by
/// [`crate::state::OracleState::ingest_signed`] (Stage 11b). The
/// unsigned path ([`crate::state::OracleState::ingest`]) ignores the
/// field entirely — useful for tests and trusted-bridge deployments.
///
/// The bytes the publisher signs are the canonical big-endian
/// concatenation of `(feed_id, price, timestamp)`, hashed by the ECDSA
/// implementation's configured digest (SHA-256 with k256's default).
/// See [`Self::signed_bytes`] for the exact byte layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PriceObservation {
    pub feed: FeedId,
    pub price: IndexPrice,
    /// Publisher-reported unix seconds. The oracle uses this against
    /// its own `now` parameter to detect staleness.
    pub timestamp: u64,
    /// ECDSA signature over [`Self::signed_bytes`]. Use
    /// [`Signature::ZERO`] for the unsigned/trusted-bridge path.
    pub signature: Signature,
}

impl PriceObservation {
    /// Construct an observation with [`Signature::ZERO`], for the
    /// unsigned/trusted-bridge ingest path and for tests.
    #[must_use]
    pub const fn unsigned(feed: FeedId, price: IndexPrice, timestamp: u64) -> Self {
        Self {
            feed,
            price,
            timestamp,
            signature: Signature::ZERO,
        }
    }

    /// The exact bytes the publisher signs.
    ///
    /// Layout (20 bytes total):
    /// ```text
    ///   [ 0..  4]  feed_id   (u32, big-endian)
    ///   [ 4.. 12]  price     (u64, big-endian)
    ///   [12.. 20]  timestamp (u64, big-endian)
    /// ```
    ///
    /// Big-endian and fixed-width so every validator computes the
    /// same digest from the same inputs.
    #[must_use]
    pub fn signed_bytes(&self) -> [u8; 20] {
        let mut buf = [0u8; 20];
        buf[0..4].copy_from_slice(&self.feed.0.to_be_bytes());
        buf[4..12].copy_from_slice(&self.price.0.to_be_bytes());
        buf[12..20].copy_from_slice(&self.timestamp.to_be_bytes());
        buf
    }
}

/// The aggregator's output — one canonical index price plus the
/// metadata callers need to audit/decide whether to use it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AggregatedPrice {
    /// The aggregated index price (median of fresh non-deviating feeds).
    pub index: IndexPrice,
    /// Block-time (unix seconds) at which the aggregation was computed.
    pub computed_at: u64,
    /// Number of feeds that contributed to the final median (post
    /// deviation filter). Always ≥ `OracleParams::min_feeds_required`
    /// when this struct is produced successfully.
    pub feeds_used: u8,
}

/// Oracle parameters: aggregation policy + circuit breakers.
///
/// All thresholds are deterministic (integer-valued); no floats anywhere
/// in the oracle's hot path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OracleParams {
    /// Observations older than `now - staleness_window_secs` are dropped
    /// before aggregation. A typical mainnet setting is 30-60 seconds:
    /// long enough for normal CEX feed jitter, short enough that a
    /// frozen publisher noticeably stops contributing.
    pub staleness_window_secs: u64,
    /// Minimum number of fresh, non-deviating feeds required to publish
    /// an aggregated price. If fewer feeds qualify, the aggregator
    /// returns an [`AggregationError`] and the caller must decide
    /// whether to halt the chain or fall back to the last known price.
    pub min_feeds_required: u8,
    /// Single-feed deviation cap (bps from the median). A feed whose
    /// price differs from the initial median by more than this is
    /// dropped before the final median is recomputed. Hyperliquid-style
    /// default: 50 bps (0.5%); openhl's `hyperliquid_default` uses 100
    /// bps for a slightly looser v0.
    pub max_deviation_bps: u32,
}

impl OracleParams {
    /// Hyperliquid-shape defaults: 60-second staleness window, 2 feeds
    /// minimum, 100 bps single-feed deviation cap.
    #[must_use]
    pub const fn hyperliquid_default() -> Self {
        Self {
            staleness_window_secs: 60,
            min_feeds_required: 2,
            max_deviation_bps: 100,
        }
    }
}

/// Why a single observation was rejected at ingestion. Returned by
/// [`crate::state::OracleState::ingest`] and
/// [`crate::state::OracleState::ingest_signed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationError {
    /// The observation's timestamp is older than `now -
    /// staleness_window_secs`. The bridge should not have submitted it.
    Stale {
        observation_ts: u64,
        now: u64,
        window: u64,
    },
    /// The observation's timestamp is in the future relative to `now`.
    /// Possible causes: publisher clock skew, malicious feed. Rejected
    /// to keep the staleness check well-defined.
    FromFuture { observation_ts: u64, now: u64 },
    /// The observation reports a zero price. Always a publisher error
    /// (zero spot price is non-physical for any tradable asset).
    ZeroPrice,
    /// `ingest_signed` was called for a feed that has no
    /// [`PublisherKey`] registered. The bridge needs to call
    /// [`crate::state::OracleState::register_publisher`] before this
    /// feed can be ingested via the signed path.
    UnknownFeed { feed: FeedId },
    /// The observation's signature did not verify against the
    /// registered publisher key for its feed. Either the signature is
    /// malformed (not a valid encoding of `(r, s)`), the signed message
    /// doesn't match, or the publisher's private key has been swapped
    /// without a registry update.
    InvalidSignature { feed: FeedId },
}

/// Why an aggregation attempt failed. Returned by
/// [`crate::state::OracleState::refresh`].
///
/// On any variant the caller must decide whether to halt the chain
/// (conservative) or reuse the previous price (permissive). v0 openhl
/// callers should halt; production hardening can layer policy on top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggregationError {
    /// Fewer feeds were fresh than `min_feeds_required`. No median
    /// computed.
    TooFewFreshFeeds { fresh: u8, required: u8 },
    /// After the deviation filter dropped outlier feeds, the remaining
    /// count fell below `min_feeds_required`. This is the "two CEXs
    /// agree, one is wildly off, but we only have three feeds" case —
    /// dropping the outlier leaves us under-quorum.
    TooFewAfterDeviationFilter { remaining: u8, required: u8 },
}
