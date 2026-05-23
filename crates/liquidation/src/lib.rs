//! `openhl-liquidation` — perpetual-position liquidation engine.
//!
//! Pure compute through Stage 10b's compute extensions: no I/O, no async,
//! no networking. Liquidation decisions are deterministic functions over
//! `(account_snapshot, mark, params)`. Every validator on the chain must
//! reach the same [`MarginHealth`] from the same inputs; if two validators
//! classify the same account differently, the chain forks. Stage 10b adds
//! a single stateful primitive — [`InsuranceFund`] — that the bridge owns
//! and mutates on liquidation events; Stage 10c adds the
//! [`LiquidationScanner`] orchestrator; Stage 10d adds [`execute_adl`] as
//! the Layer-3 fallback when the insurance fund can't cover everything.
//! All deterministic by construction (no floats, saturating integer math).
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
//!   - **Stage 10a** — margin math, per-account classification,
//!     single-account close-order generation. Pure compute, no state.
//!   - **Stage 10b** — insurance fund state machine ([`InsuranceFund`]),
//!     deficit absorption, fee credit. Adds [`compute::liquidation_fee`],
//!     [`compute::solvent_close_outcome`], and
//!     [`compute::underwater_close_outcome`] for the per-close
//!     credit/debit decomposition.
//!   - **Stage 10c** — multi-account scanner ([`LiquidationScanner`])
//!     that iterates over `&[AccountSnapshot]`, classifies each,
//!     generates close orders for the CLOB, applies insurance-fund
//!     deposits / withdraws, and surfaces any unfilled deficit via
//!     [`ScanReport::unfilled_deficit`].
//!   - **Stage 10d (this commit)** — auto-deleveraging
//!     ([`execute_adl`]) as the Layer-3 fallback when the insurance
//!     fund can't absorb the entire shortfall. Ranks profitable
//!     counter-positions by `(pnl_pct × leverage)`, force-closes them
//!     in order, applies a haircut to each winner's unrealized `PnL`
//!     until the deficit is absorbed.
//!
//! The composition pattern the bridge follows each block:
//!   1. `scanner.scan(&accounts, mark)` → `ScanReport`.
//!   2. Apply each `record.close_order` against the CLOB.
//!   3. If `report.unfilled_deficit > 0`, call
//!      `execute_adl(&remaining_accounts, mark, report.unfilled_deficit)`
//!      and apply each [`AdlRecord`] as a bookkeeping mutation (NOT
//!      orderbook; ADL bypasses matching).
//!   4. If the [`AdlReport::deficit_remaining`] is still positive, the
//!      chain has reached an unresolvable state — halt or accept the
//!      residual as protocol loss per the deployment's policy.
//!
//! ### Why fixed-point integers, not floats
//!
//! Same answer as `openhl-funding`: consensus determinism. We use signed
//! integers scaled by [`MARGIN_SCALE`] (10⁴, i.e. basis points) for margin
//! ratios, and the `i64 + saturating arithmetic` discipline from the
//! funding crate for all intermediate products.

pub mod adl;
pub mod compute;
pub mod insurance;
pub mod scanner;
pub mod types;

pub use adl::{adl_score, execute_adl, AdlRecord, AdlReport, AdlScore};
pub use compute::{
    account_equity, close_order_spec, liquidation_fee, margin_health, margin_ratio,
    notional_value, solvent_close_outcome, underwater_close_outcome, unrealized_pnl,
};
pub use insurance::{InsuranceFund, WithdrawOutcome};
pub use scanner::{CloseOutcomeKind, LiquidationRecord, LiquidationScanner, ScanReport};
pub use types::{
    AccountSnapshot, CloseOrderSpec, LiquidationParams, MarginHealth, MarginRatio, SolventClose,
    UnderwaterClose, MARGIN_SCALE,
};
