//! `openhl-liquidation` — perpetual-position liquidation engine.
//!
//! Pure compute in Stage 10a: no I/O, no async, no networking. Liquidation
//! decisions are deterministic functions over `(account_snapshot, mark,
//! params)`. Every validator on the chain must reach the same
//! [`MarginHealth`] from the same inputs; if two validators classify the
//! same account differently, the chain forks.
//!
//! ### Hyperliquid-shape liquidation, in one paragraph
//!
//! Perpetual contracts are levered positions backed by deposited
//! collateral. As the mark price moves against an open position,
//! unrealized PnL eats into the account's equity. When `equity / notional`
//! drops below the network's maintenance-margin requirement, the engine
//! force-closes the position at market — opposite side, full size, no
//! limit price. The liquidation fee is debited from collateral and
//! credited to the insurance fund. Any residual collateral, after fee
//! and PnL settlement, stays with the account. If equity went negative
//! before the close (the account is "underwater"), the insurance fund
//! absorbs the deficit instead of the position closing solvently.
//!
//! ### Stage decomposition
//!
//! Stage 10 ships in three sub-stages, mirroring the funding crate's
//! `types → compute → clock` shape:
//!
//!   - **Stage 10a (this commit)** — margin math, per-account
//!     classification, single-account close-order generation. Pure
//!     compute, no state.
//!   - **Stage 10b (next)** — insurance fund state machine, deficit
//!     absorption, fee credit.
//!   - **Stage 10c** — multi-account scanner that iterates over
//!     `&[AccountSnapshot]`, batches close orders for the CLOB, and
//!     records insurance-fund movements.
//!
//! Auto-deleveraging (ADL), the fallback path when the insurance fund is
//! exhausted, is intentionally out of scope. It can be added as Stage
//! 10d if needed.
//!
//! ### Why fixed-point integers, not floats
//!
//! Same answer as `openhl-funding`: consensus determinism. We use signed
//! integers scaled by [`MARGIN_SCALE`] (10⁴, i.e. basis points) for margin
//! ratios, and the `i64 + saturating arithmetic` discipline from the
//! funding crate for all intermediate products.

pub mod compute;
pub mod types;

pub use compute::{
    account_equity, close_order_spec, margin_health, margin_ratio, notional_value, unrealized_pnl,
};
pub use types::{
    AccountSnapshot, CloseOrderSpec, LiquidationParams, MarginHealth, MarginRatio, MARGIN_SCALE,
};
