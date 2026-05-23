//! Vault state machine (Stage 12).
//!
//! Owned by the bridge (one [`VaultState`] per vault deployed on the
//! chain), mutated on per-block deposit / withdraw / mark-to-market
//! events. Holds `(total_shares, total_assets)` and the [`VaultParams`]
//! that govern deposit policy.
//!
//! ### Invariants
//!
//! - `total_shares ≥ 0` (the type system enforces this; `Shares` is `u64`).
//! - `total_assets` may be negative (insolvent vault). Deposits and
//!   withdrawals reject in that state until the manager
//!   recapitalizes (calls `mark_to_market` with a positive new value)
//!   or operators wind down the vault.
//! - `total_shares == 0` iff there are no outstanding deposits. The
//!   inception case (`total_shares == 0`) always mints 1:1 regardless
//!   of `total_assets` — preserving the equity invariant when assets
//!   sit in the vault before the first depositor (rare, but possible
//!   if the manager seeds the vault with their own capital).
//!
//! ### What `mark_to_market` is for
//!
//! The vault doesn't compute its own `PnL` — Stage 12 is a pure
//! share-accounting primitive. The bridge layer holds the vault's
//! actual positions, computes their unrealized `PnL` each block, and
//! calls `mark_to_market(new_total_assets)` to update the vault's
//! view. Subsequent deposits/withdrawals price shares against the
//! marked value. No shares are minted or burned by `mark_to_market`;
//! the per-share value just changes.

use crate::compute::{assets_to_shares, share_price_bps, shares_to_assets};
use crate::types::{
    Assets, DepositError, DepositResult, Shares, VaultParams, WithdrawError, WithdrawResult,
};

/// Vault state machine.
///
/// Lifecycle:
///   1. `new(params)` — empty vault: zero shares, zero assets.
///   2. `deposit(assets)` — anyone deposits; vault mints shares
///      proportional to the current NAV.
///   3. `mark_to_market(new_total_assets)` — bridge updates the
///      vault's view of its assets at each block.
///   4. `withdraw(shares)` — burn shares, release pro-rata assets.
///
/// All operations are pure functions of `(prior state, input)` —
/// deterministic across validators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultState {
    params: VaultParams,
    total_shares: u64,
    total_assets: i64,
}

impl VaultState {
    /// Construct an empty vault.
    #[must_use]
    pub const fn new(params: VaultParams) -> Self {
        Self {
            params,
            total_shares: 0,
            total_assets: 0,
        }
    }

    /// Borrow the vault's params.
    #[must_use]
    pub const fn params(&self) -> &VaultParams {
        &self.params
    }

    /// Total shares outstanding.
    #[must_use]
    pub const fn total_shares(&self) -> Shares {
        Shares(self.total_shares)
    }

    /// Total assets under management. Can be negative (insolvent).
    #[must_use]
    pub const fn total_assets(&self) -> Assets {
        Assets(self.total_assets)
    }

    /// NAV per share in basis points (`10_000` bps = 1.0× inception).
    /// Returns `None` for an insolvent vault — same recourse as
    /// [`crate::compute::share_price_bps`].
    #[must_use]
    pub fn share_price_bps(&self) -> Option<i64> {
        share_price_bps(self.total_shares, self.total_assets)
    }

    /// Whether the vault is solvent — has shares outstanding but
    /// non-positive assets. An empty vault is **not** insolvent
    /// (no shareholders to make whole).
    #[must_use]
    pub const fn is_insolvent(&self) -> bool {
        self.total_shares > 0 && self.total_assets <= 0
    }

    /// Deposit `assets` and mint the corresponding shares.
    ///
    /// Validation:
    ///   - `assets > 0` (caller bug otherwise).
    ///   - `assets >= params.min_deposit`.
    ///   - Vault is not insolvent.
    ///   - Computed shares > 0 (deposits that round to zero shares
    ///     are rejected; otherwise tiny deposits would silently
    ///     dilute existing shareholders).
    ///
    /// On success: `total_shares += shares_minted`, `total_assets += assets`.
    pub fn deposit(&mut self, assets: i64) -> Result<DepositResult, DepositError> {
        if assets <= 0 {
            return Err(DepositError::NonPositiveAssets { provided: assets });
        }
        if assets < self.params.min_deposit {
            return Err(DepositError::BelowMinimum {
                provided: assets,
                minimum: self.params.min_deposit,
            });
        }
        if self.is_insolvent() {
            return Err(DepositError::InsolventVault {
                total_assets: self.total_assets,
                total_shares: self.total_shares,
            });
        }

        // The inception / insolvent guards above should already catch
        // the cases where assets_to_shares returns None, but be
        // defensive — surface InsolventVault if it slips through.
        let Some(shares_minted) =
            assets_to_shares(assets, self.total_shares, self.total_assets)
        else {
            return Err(DepositError::InsolventVault {
                total_assets: self.total_assets,
                total_shares: self.total_shares,
            });
        };
        if shares_minted == 0 {
            return Err(DepositError::SharesRoundToZero { provided: assets });
        }

        self.total_shares = self.total_shares.saturating_add(shares_minted);
        self.total_assets = self.total_assets.saturating_add(assets);

        Ok(DepositResult {
            shares_minted,
            new_total_shares: self.total_shares,
            new_total_assets: self.total_assets,
        })
    }

    /// Burn `shares` and release the proportional assets.
    ///
    /// Validation:
    ///   - `shares > 0`.
    ///   - `shares <= total_shares` (the type system already enforces
    ///     `>= 0` since `Shares` wraps `u64`).
    ///   - Vault is not insolvent.
    ///
    /// On success: `total_shares -= shares`, `total_assets -= assets_returned`.
    pub fn withdraw(&mut self, shares: u64) -> Result<WithdrawResult, WithdrawError> {
        if shares == 0 {
            return Err(WithdrawError::ZeroShares);
        }
        if shares > self.total_shares {
            return Err(WithdrawError::InsufficientShares {
                requested: shares,
                available: self.total_shares,
            });
        }
        if self.is_insolvent() {
            return Err(WithdrawError::InsolventVault {
                total_assets: self.total_assets,
                total_shares: self.total_shares,
            });
        }

        let Some(assets_returned) =
            shares_to_assets(shares, self.total_shares, self.total_assets)
        else {
            return Err(WithdrawError::InsolventVault {
                total_assets: self.total_assets,
                total_shares: self.total_shares,
            });
        };

        self.total_shares -= shares;
        self.total_assets = self.total_assets.saturating_sub(assets_returned);

        Ok(WithdrawResult {
            assets_returned,
            new_total_shares: self.total_shares,
            new_total_assets: self.total_assets,
        })
    }

    /// Update the vault's view of its total assets without minting or
    /// burning shares.
    ///
    /// Called by the bridge each block (or each time the underlying
    /// position's value changes). Subsequent deposits and withdrawals
    /// price shares against the marked value. The per-share value
    /// changes; no shares move.
    pub fn mark_to_market(&mut self, new_total_assets: i64) {
        self.total_assets = new_total_assets;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn default_params() -> VaultParams {
        VaultParams::permissive()
    }

    // ─── construction ──────────────────────────────────────────────

    #[test]
    fn new_vault_is_empty() {
        let v = VaultState::new(default_params());
        assert_eq!(v.total_shares().0, 0);
        assert_eq!(v.total_assets().0, 0);
        assert_eq!(v.share_price_bps(), Some(10_000));
        assert!(!v.is_insolvent());
    }

    // ─── deposit happy paths ────────────────────────────────────────

    #[test]
    fn first_deposit_mints_1_to_1() {
        let mut v = VaultState::new(default_params());
        let r = v.deposit(100).unwrap();
        assert_eq!(r.shares_minted, 100);
        assert_eq!(r.new_total_shares, 100);
        assert_eq!(r.new_total_assets, 100);
    }

    #[test]
    fn second_deposit_at_par_mints_proportionally() {
        let mut v = VaultState::new(default_params());
        v.deposit(100).unwrap();
        let r = v.deposit(50).unwrap();
        assert_eq!(r.shares_minted, 50);
        assert_eq!(r.new_total_shares, 150);
        assert_eq!(r.new_total_assets, 150);
    }

    #[test]
    fn deposit_after_markup_mints_fewer_shares() {
        // Initial: 100 assets, 100 shares (par).
        // Mark up to 200 assets (manager's positions doubled).
        // New depositor of 50 assets → 50 × 100 / 200 = 25 shares.
        let mut v = VaultState::new(default_params());
        v.deposit(100).unwrap();
        v.mark_to_market(200);
        let r = v.deposit(50).unwrap();
        assert_eq!(r.shares_minted, 25);
        assert_eq!(r.new_total_shares, 125);
        assert_eq!(r.new_total_assets, 250);
    }

    #[test]
    fn deposit_after_markdown_mints_more_shares() {
        // Initial: 100 assets, 100 shares (par).
        // Mark down to 50 assets (manager's positions halved).
        // New depositor of 50 assets → 50 × 100 / 50 = 100 shares.
        let mut v = VaultState::new(default_params());
        v.deposit(100).unwrap();
        v.mark_to_market(50);
        let r = v.deposit(50).unwrap();
        assert_eq!(r.shares_minted, 100);
        assert_eq!(r.new_total_shares, 200);
        assert_eq!(r.new_total_assets, 100);
    }

    // ─── deposit rejections ─────────────────────────────────────────

    #[test]
    fn deposit_zero_assets_is_rejected() {
        let mut v = VaultState::new(default_params());
        assert_eq!(
            v.deposit(0),
            Err(DepositError::NonPositiveAssets { provided: 0 })
        );
    }

    #[test]
    fn deposit_negative_assets_is_rejected() {
        let mut v = VaultState::new(default_params());
        assert_eq!(
            v.deposit(-5),
            Err(DepositError::NonPositiveAssets { provided: -5 })
        );
    }

    #[test]
    fn deposit_below_minimum_is_rejected() {
        let mut v = VaultState::new(VaultParams { min_deposit: 100 });
        assert_eq!(
            v.deposit(50),
            Err(DepositError::BelowMinimum {
                provided: 50,
                minimum: 100,
            })
        );
    }

    #[test]
    fn deposit_into_insolvent_vault_is_rejected() {
        let mut v = VaultState::new(default_params());
        v.deposit(100).unwrap();
        v.mark_to_market(-50); // insolvent
        let err = v.deposit(100).unwrap_err();
        assert!(matches!(err, DepositError::InsolventVault { .. }));
    }

    #[test]
    fn deposit_that_rounds_to_zero_shares_is_rejected() {
        // 100 shares, 1_000_000 assets → 1 deposit produces
        // 1 × 100 / 1_000_000 = 0 shares.
        let mut v = VaultState::new(default_params());
        v.deposit(100).unwrap();
        v.mark_to_market(1_000_000);
        assert_eq!(
            v.deposit(1),
            Err(DepositError::SharesRoundToZero { provided: 1 })
        );
    }

    // ─── withdraw happy paths ───────────────────────────────────────

    #[test]
    fn withdraw_at_par_returns_proportional() {
        let mut v = VaultState::new(default_params());
        v.deposit(100).unwrap();
        let r = v.withdraw(25).unwrap();
        assert_eq!(r.assets_returned, 25);
        assert_eq!(r.new_total_shares, 75);
        assert_eq!(r.new_total_assets, 75);
    }

    #[test]
    fn withdraw_after_markup_returns_more() {
        // 100 shares, marked up to 200 assets → withdrawing 50 shares
        // returns 50 × 200 / 100 = 100 assets.
        let mut v = VaultState::new(default_params());
        v.deposit(100).unwrap();
        v.mark_to_market(200);
        let r = v.withdraw(50).unwrap();
        assert_eq!(r.assets_returned, 100);
        assert_eq!(r.new_total_shares, 50);
        assert_eq!(r.new_total_assets, 100);
    }

    #[test]
    fn withdraw_all_shares_drains_vault() {
        let mut v = VaultState::new(default_params());
        v.deposit(100).unwrap();
        let r = v.withdraw(100).unwrap();
        assert_eq!(r.assets_returned, 100);
        assert_eq!(r.new_total_shares, 0);
        assert_eq!(r.new_total_assets, 0);
    }

    // ─── withdraw rejections ────────────────────────────────────────

    #[test]
    fn withdraw_zero_shares_is_rejected() {
        let mut v = VaultState::new(default_params());
        v.deposit(100).unwrap();
        assert_eq!(v.withdraw(0), Err(WithdrawError::ZeroShares));
    }

    #[test]
    fn withdraw_more_than_outstanding_is_rejected() {
        let mut v = VaultState::new(default_params());
        v.deposit(100).unwrap();
        assert_eq!(
            v.withdraw(101),
            Err(WithdrawError::InsufficientShares {
                requested: 101,
                available: 100,
            })
        );
    }

    #[test]
    fn withdraw_from_insolvent_vault_is_rejected() {
        let mut v = VaultState::new(default_params());
        v.deposit(100).unwrap();
        v.mark_to_market(-50);
        let err = v.withdraw(50).unwrap_err();
        assert!(matches!(err, WithdrawError::InsolventVault { .. }));
    }

    // ─── mark_to_market ─────────────────────────────────────────────

    #[test]
    fn mark_to_market_changes_assets_not_shares() {
        let mut v = VaultState::new(default_params());
        v.deposit(100).unwrap();
        assert_eq!(v.total_shares().0, 100);
        v.mark_to_market(150);
        assert_eq!(v.total_shares().0, 100, "shares unchanged");
        assert_eq!(v.total_assets().0, 150);
    }

    #[test]
    fn mark_to_market_to_negative_makes_vault_insolvent() {
        let mut v = VaultState::new(default_params());
        v.deposit(100).unwrap();
        v.mark_to_market(-1);
        assert!(v.is_insolvent());
        assert_eq!(v.share_price_bps(), None);
    }

    #[test]
    fn empty_vault_is_never_insolvent() {
        let mut v = VaultState::new(default_params());
        // Even with a negative `mark_to_market` (e.g., chain glitch),
        // an empty vault has no shareholders to make whole.
        v.mark_to_market(-100);
        assert!(!v.is_insolvent());
    }

    // ─── proptest: state-machine invariants ─────────────────────────

    proptest! {
        /// Deposit + withdraw round-trip: the depositor never gets out
        /// strictly more than they put in (rounding may leave dust).
        #[test]
        fn deposit_then_withdraw_no_inflation(
            assets in 1_i64..1_000_000,
            initial_shares in 1_u64..1_000_000,
            initial_assets in 1_i64..1_000_000,
        ) {
            let mut v = VaultState {
                params: default_params(),
                total_shares: initial_shares,
                total_assets: initial_assets,
            };
            let dep = v.deposit(assets).unwrap();
            if dep.shares_minted > 0 {
                let wd = v.withdraw(dep.shares_minted).unwrap();
                prop_assert!(
                    wd.assets_returned <= assets,
                    "withdraw inflated: deposited {}, got back {}",
                    assets,
                    wd.assets_returned
                );
            }
        }

        /// Deposit then withdraw the same shares should leave
        /// total_shares unchanged (returns to prior).
        #[test]
        fn deposit_withdraw_preserves_total_shares(
            assets in 1_i64..1_000_000,
            initial_shares in 1_u64..1_000_000,
            initial_assets in 1_i64..1_000_000,
        ) {
            let mut v = VaultState {
                params: default_params(),
                total_shares: initial_shares,
                total_assets: initial_assets,
            };
            let dep = v.deposit(assets).unwrap();
            if dep.shares_minted > 0 {
                v.withdraw(dep.shares_minted).unwrap();
                prop_assert_eq!(v.total_shares().0, initial_shares);
            }
        }

        /// Determinism: same starting state + same inputs → same outputs.
        #[test]
        fn deposit_is_deterministic(
            assets in 1_i64..1_000_000,
            shares in 0_u64..1_000_000,
            total_assets in 1_i64..1_000_000,
        ) {
            let mut a = VaultState {
                params: default_params(),
                total_shares: shares,
                total_assets,
            };
            let mut b = a.clone();
            prop_assert_eq!(a.deposit(assets), b.deposit(assets));
            prop_assert_eq!(a, b);
        }
    }
}
