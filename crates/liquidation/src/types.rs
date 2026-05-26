//! Core types for the liquidation engine.
//!
//! Pure data — no I/O, no allocation. Every type is `Copy`-friendly so the
//! engine can be invoked on snapshots taken at the bridge layer without
//! lifetime gymnastics. The convention follows `openhl-funding`: the
//! liquidation crate never owns mutable state in Stage 10a; it computes
//! over snapshots that the caller assembled.
//!
//! ### Why fixed-point integers, not floats
//!
//! Same answer as `openhl-funding`: consensus determinism. Every validator
//! must reach the same `MarginHealth` from the same inputs, and float
//! arithmetic varies bit-for-bit across compilers and CPUs. We use signed
//! integers scaled by [`MARGIN_SCALE`] (basis points, 10⁴) for margin
//! ratios.

use openhl_clob::{AccountId, Qty, Side};
use openhl_funding::{MarkPrice, Notional, PositionSize};
use serde::{Deserialize, Serialize};

/// Scale factor for [`MarginRatio`] — basis points (1 bp = 0.01%).
///
/// A `MarginRatio(1000)` represents a 10% ratio; `MarginRatio(MARGIN_SCALE)`
/// represents 100%. Bps is the conventional unit for margin in TradFi and
/// in crypto perp venues (Hyperliquid, Binance, Drift all express margin
/// requirements in bps).
pub const MARGIN_SCALE: i64 = 10_000;

/// Account margin ratio = equity / notional, scaled by [`MARGIN_SCALE`].
///
/// Sign: usually non-negative; can be negative when the account is
/// "underwater" — accumulated losses have driven equity below zero, and
/// liquidating the position alone cannot cover the deficit. The insurance
/// fund absorbs that shortfall (Stage 10b).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MarginRatio(pub i64);

/// Margin health classification given the account's current margin ratio
/// and the network's params. Four states, in decreasing health order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MarginHealth {
    /// Margin ratio ≥ initial margin requirement. Healthy: the account
    /// can open new positions or increase existing ones.
    Safe,
    /// Margin ratio ∈ [maintenance, initial). Allowed to hold existing
    /// positions but not to add risk. Production UIs typically warn the
    /// user.
    AtRisk,
    /// Margin ratio < maintenance, equity still ≥ 0. The engine should
    /// liquidate the position at market; the account's remaining equity
    /// (after the liquidation fee) returns to the account.
    Liquidatable,
    /// Margin ratio < 0 (equity is negative). Closing the position at
    /// any price won't fully cover losses. The insurance fund absorbs
    /// the shortfall — handled in Stage 10b.
    Underwater,
}

/// Snapshot of one account's perpetual-market state, assembled by the
/// bridge layer before invoking the liquidation engine. Same "snapshot"
/// model as `openhl_funding::Position`: the engine treats this as a
/// per-tick read-only view, never mutates it.
///
/// `avg_entry` is the volume-weighted average price at which the account
/// opened its current net position. The owning layer (vault / clearing)
/// is responsible for maintaining this across fills.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub account: AccountId,
    pub position_size: PositionSize,
    pub avg_entry: MarkPrice,
    pub collateral: Notional,
}

/// Network parameters governing the margin model.
///
/// Bps convention: `initial_margin_bps = 1000` means a 10% initial margin
/// requirement. Maintenance must be ≤ initial; if a misconfigured network
/// sets them equal, every position at exactly that threshold classifies as
/// `Liquidatable` (the conservative default).
///
/// `liquidation_fee_bps` is charged on the notional being closed, paid
/// out of the account's collateral, and credited to the insurance fund
/// (Stage 10b). A typical HL-style value is 1–2% (100–200 bps).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiquidationParams {
    /// Initial margin requirement in bps (e.g., 1000 = 10%).
    pub initial_margin_bps: u32,
    /// Maintenance margin requirement in bps (e.g., 200 = 2%).
    pub maintenance_margin_bps: u32,
    /// Liquidation fee in bps, charged on closed notional.
    pub liquidation_fee_bps: u32,
}

impl LiquidationParams {
    /// Hyperliquid-style defaults: 10% initial, 2% maintenance, 1.5% fee.
    /// Real production deployments use tiered maintenance (higher margin
    /// for larger position sizes) — out of scope for Stage 10a.
    #[must_use]
    pub const fn hyperliquid_default() -> Self {
        Self {
            initial_margin_bps: 1_000,
            maintenance_margin_bps: 200,
            liquidation_fee_bps: 150,
        }
    }

    #[must_use]
    pub const fn initial_margin_bps(&self) -> u32 {
        self.initial_margin_bps
    }

    #[must_use]
    pub const fn maintenance_margin_bps(&self) -> u32 {
        self.maintenance_margin_bps
    }

    #[must_use]
    pub const fn liquidation_fee_bps(&self) -> u32 {
        self.liquidation_fee_bps
    }
}

/// Specification for a single liquidation close order, generated by the
/// engine and consumed by the bridge layer. The bridge encodes this as
/// `openhl_clob::Action::SubmitMarket` and routes it through the matching
/// engine.
///
/// Always a market order — liquidation accepts any available price.
/// Always the opposite side of the position: a long position closes via
/// `Side::Sell`, a short via `Side::Buy`. Quantity is the absolute value
/// of the position size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloseOrderSpec {
    pub account: AccountId,
    pub side: Side,
    pub qty: Qty,
}

/// Solvent-close outcome (Stage 10b).
///
/// Produced by [`crate::compute::solvent_close_outcome`] for a Liquidatable
/// account whose post-close equity covers the liquidation fee in full.
/// Both fields are non-negative.
///
/// `fee_to_fund` is credited to the insurance fund; `residual_to_account`
/// is returned to the trader's collateral balance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SolventClose {
    /// Fee deducted from collateral and credited to the insurance fund.
    pub fee_to_fund: i64,
    /// What's returned to the trader's collateral after the close + fee.
    pub residual_to_account: i64,
}

/// Underwater-close outcome (Stage 10b).
///
/// Produced by [`crate::compute::underwater_close_outcome`] when the
/// account's post-close equity cannot cover the full liquidation fee.
/// Covers two sub-cases under one shape:
///   - Post-close equity is positive but smaller than the desired fee
///     (Liquidatable account whose close + fee turned underwater): the
///     remaining equity is paid as a partial fee, the uncollected portion
///     becomes the shortfall.
///   - Post-close equity is already negative (Underwater account): no fee
///     is collected, the full desired fee plus the negative equity becomes
///     the shortfall.
///
/// Both fields are non-negative; `fee_to_fund` may be `0` in the
/// negative-equity case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnderwaterClose {
    /// Partial fee collected from any positive post-close equity, credited
    /// to the insurance fund. May be `0`.
    pub fee_to_fund: i64,
    /// What the insurance fund must absorb so the close completes. The
    /// caller hands this to [`crate::insurance::InsuranceFund::withdraw_shortfall`].
    pub shortfall_to_fund: i64,
}
