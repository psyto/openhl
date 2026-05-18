//! Core types for the funding state machine.
//!
//! Pure data — no I/O, no allocation beyond what's needed for settlements.
//! Every type is `Copy`-friendly (or, in the case of `Position`, `Clone +
//! Copy`) so callers can pass snapshots without lifetime gymnastics.
//!
//! ### Why fixed-point integers, not floats
//!
//! Consensus determinism — every validator must compute the *same* funding
//! rate from the *same* inputs. Float arithmetic gives different bit patterns
//! across compilers and CPUs (FMA, rounding mode, denormal handling); the
//! moment two validators disagree on a single LSB they fork. We use signed
//! integers scaled by [`RATE_SCALE`] (parts-per-billion) for rates and
//! premiums, and a separate `Notional` type for quote-currency deltas.

use openhl_clob::AccountId;

/// Scale factor for [`FundingRate`] and [`Premium`]. A raw value of
/// `RATE_SCALE` represents `1.0` (i.e., 100%). With `1e9` we get 9 decimal
/// digits of precision — more than enough for funding rates that typically
/// sit in the ±0.01% to ±0.05% per interval band.
pub const RATE_SCALE: i64 = 1_000_000_000;

/// Mark price in minor units. Same scale convention as `clob::Price`, but a
/// distinct type so callers can't accidentally feed an orderbook price into
/// the funding math where an index/oracle price is expected.
///
/// `MarkPrice` is a single u64 not a signed-fixed-point, because prices are
/// always positive (zero or negative price would be a system invariant
/// violation handled upstream, not here).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MarkPrice(pub u64);

/// Index price (off-chain oracle reference). Same scale as `MarkPrice`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexPrice(pub u64);

/// Premium = `(mark - index) / index`, scaled by [`RATE_SCALE`].
///
/// Sign convention: positive when mark > index (longs are overpaying,
/// funding will be positive → longs pay shorts).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Premium(pub i64);

/// Per-interval funding rate. Same scale as [`Premium`]; positive means
/// longs pay shorts. A rate of `RATE_SCALE / 100` = 1% per interval.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FundingRate(pub i64);

/// Signed position size in base units. Positive = long, negative = short,
/// zero = flat. Accounts with zero size aren't included in settlement
/// snapshots — see [`Position`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PositionSize(pub i64);

/// Signed quote-currency delta. Positive = account receives, negative =
/// account pays. Funding settlement produces one [`Notional`] per non-flat
/// position per tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Notional(pub i64);

/// A single account's net position on the market. The funding state machine
/// treats positions as a per-tick *snapshot* — it never owns or mutates
/// them. The owning layer (vault / clearing) is responsible for tracking
/// `Position` over time and producing snapshots at each tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub account: AccountId,
    pub size: PositionSize,
}

/// Output of applying a funding rate to one position. The bridge layer
/// translates these into balance updates against each account's quote
/// balance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settlement {
    pub account: AccountId,
    pub delta: Notional,
}

/// Network parameters that govern funding cadence and magnitude.
///
/// `divisor` represents "settlements per day": HL settles 8 times per day,
/// so `premium / 8` is the per-interval rate. Higher divisor → smaller rate
/// per tick (and inverse: lower divisor concentrates the same daily target
/// rate into fewer payments).
///
/// `rate_cap` is the absolute maximum |rate| per interval. Production
/// networks set this to bound the worst-case payment an extreme oracle
/// dislocation can produce. Zero `rate_cap` disables funding entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FundingParams {
    pub interval_secs: u64,
    pub rate_cap: FundingRate,
    pub divisor: u32,
}

impl FundingParams {
    /// Hyperliquid-style defaults: 1-hour interval, ±4%/hour cap, 8× divisor.
    /// 8× divisor with a 1-hour interval means the *target* daily premium
    /// would be applied across 24 hours' worth of ticks at 1/8 of the premium
    /// each — i.e., 24/8 = 3× the premium per day. That asymmetry is
    /// intentional: HL caps more aggressively than the divisor alone implies.
    #[must_use]
    pub const fn hyperliquid_default() -> Self {
        Self {
            interval_secs: 3600,
            // 4% per interval = 40_000_000 ppb (since 0.04 × 1e9 = 4e7).
            rate_cap: FundingRate(40_000_000),
            divisor: 8,
        }
    }
}
