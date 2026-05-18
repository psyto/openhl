//! `openhl-funding` — funding-rate state machine.
//!
//! Pure state machine: no I/O, no async, no networking. Funding is applied
//! deterministically on a fixed cadence (see [`FundingClock`]); every tick is
//! a pure function over `(now, mark, index, positions)` → settlements.
//!
//! ### Hyperliquid-shape funding, in one paragraph
//!
//! Perpetual contracts don't expire, so the mark price can drift arbitrarily
//! from the spot ("index") price. Funding payments push it back: when mark >
//! index (longs are overpaying), longs pay shorts; when mark < index, shorts
//! pay longs. The premium `(mark - index) / index` is divided by a
//! per-day-interval count (HL: 8 — one settlement every 3 hours) to derive a
//! per-interval rate, capped at a network-set absolute max. At each tick
//! every account with an open position settles `position_size * mark * rate`
//! in quote currency.
//!
//! Integration with the rest of openhl happens at the EVM bridge: settlement
//! deltas become balance updates that the bridge bundles into payloads. That
//! integration lives in `crates/evm/`; the rate math and tick gating are here.

pub mod clock;
pub mod compute;
pub mod types;

pub use clock::{FundingClock, FundingTick};
pub use compute::{apply_funding, compute_premium, compute_rate};
pub use types::{
    FundingParams, FundingRate, IndexPrice, MarkPrice, Notional, Position, PositionSize,
    Premium, Settlement, RATE_SCALE,
};
