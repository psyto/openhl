//! Core types for the vault primitive (Stage 12).
//!
//! Pure data — every type is `Copy`-friendly so the engine can be
//! invoked on stack-allocated values without lifetime gymnastics.
//! Follows the openhl convention: the crate never owns mutable state
//! in its types; mutation lives in [`crate::state`].
//!
//! ### What a vault is, in one paragraph
//!
//! A vault pools many depositors' collateral into one fungible "share"
//! token. Each depositor's claim on the underlying assets is denominated
//! in shares: their slice of the vault grows or shrinks proportionally
//! as the vault's positions accrue `PnL`. New depositors mint shares at
//! the current NAV (net asset value per share); withdrawers burn shares
//! and receive their pro-rata slice of current assets. The vault
//! manager runs trading strategies on the pooled collateral; the
//! depositors never see individual positions, only share-denominated
//! exposure to the manager's performance.
//!
//! ### Share accounting, in one paragraph
//!
//! At inception a vault has zero shares and zero assets. The first
//! depositor's `assets` get them an equal number of shares (1:1
//! mint). Every subsequent depositor's shares are minted in proportion
//! to the **current** assets-per-share ratio:
//!
//! ```text
//!   shares_minted = assets × total_shares / total_assets
//! ```
//!
//! After a withdrawal,
//!
//! ```text
//!   assets_returned = shares × total_assets / total_shares
//! ```
//!
//! Both formulas round toward zero (floor for positive numerators),
//! which slightly favors the *remaining* shareholders — the standard
//! ERC-4626 convention. Stage 12 v0 implements this directly with
//! i128 intermediates to avoid overflow on `assets × total_shares`.

/// A claim on a vault's pooled assets, denominated in fungible share
/// units. Shares are `u64` — non-negative by construction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Shares(pub u64);

/// Quote-currency assets held by the vault — collateral + `PnL`.
/// Signed because the vault can be transiently underwater (total
/// position losses exceed total deposited collateral). When negative,
/// deposits and withdrawals are rejected as
/// [`DepositError::InsolventVault`] / [`WithdrawError::InsolventVault`];
/// the manager must wind down or recapitalize.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Assets(pub i64);

/// Scale factor for share-price expressed in basis points.
/// `SHARE_PRICE_BPS_SCALE = 10_000` means `share_price_bps = 10_000`
/// corresponds to "1 share = 1 asset unit" (the inception ratio).
pub const SHARE_PRICE_BPS_SCALE: i64 = 10_000;

/// Vault parameters: deposit/withdraw policy. Stage 12 v0 has one
/// knob (the minimum initial deposit, an anti-dust guard); future
/// stages will add performance-fee rates, high-water-mark policy,
/// lock-up windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VaultParams {
    /// Smallest `Assets` value any deposit must equal or exceed.
    /// Defends against dust deposits that round to zero shares.
    /// Hyperliquid-style vaults set this around 100 USDC; openhl's
    /// `production_default` mirrors that magnitude.
    pub min_deposit: i64,
}

impl VaultParams {
    /// Production defaults — 100-unit minimum deposit, matching the
    /// Hyperliquid vault floor.
    #[must_use]
    pub const fn production_default() -> Self {
        Self { min_deposit: 100 }
    }

    /// Permissive defaults for tests — zero minimum so unit tests can
    /// exercise tiny deposits without tripping the anti-dust guard.
    #[must_use]
    pub const fn permissive() -> Self {
        Self { min_deposit: 0 }
    }
}

/// Why a deposit was rejected. Returned by
/// [`crate::state::VaultState::deposit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepositError {
    /// The deposit amount is zero or negative. Always a caller bug.
    NonPositiveAssets { provided: i64 },
    /// The deposit is below the vault's configured minimum.
    BelowMinimum { provided: i64, minimum: i64 },
    /// The vault's `total_assets` is non-positive but `total_shares`
    /// is positive — share price is undefined. The vault must be
    /// wound down or recapitalized by the manager before new deposits
    /// can resume.
    InsolventVault { total_assets: i64, total_shares: u64 },
    /// Computed `shares_minted` is zero. Happens when the deposit is
    /// vanishingly small relative to current `total_assets` — the
    /// floor-division rounds the share allocation away to nothing.
    /// Defends against deposits that would mint silently-zero shares.
    SharesRoundToZero { provided: i64 },
}

/// Why a withdrawal was rejected. Returned by
/// [`crate::state::VaultState::withdraw`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WithdrawError {
    /// The withdrawal amount is zero. Caller bug.
    ZeroShares,
    /// The withdrawal exceeds the vault's outstanding shares.
    InsufficientShares { requested: u64, available: u64 },
    /// Vault has shares outstanding but `total_assets ≤ 0`. Share
    /// price is undefined. Same recourse as [`DepositError::InsolventVault`].
    InsolventVault { total_assets: i64, total_shares: u64 },
}

/// Successful deposit outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DepositResult {
    pub shares_minted: u64,
    pub new_total_shares: u64,
    pub new_total_assets: i64,
}

/// Successful withdrawal outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WithdrawResult {
    pub assets_returned: i64,
    pub new_total_shares: u64,
    pub new_total_assets: i64,
}
