//! Pure share/asset conversion math (Stage 12).
//!
//! Three building blocks, all stateless:
//!   - [`assets_to_shares`] — given a deposit and the vault's current
//!     state, compute how many shares to mint.
//!   - [`shares_to_assets`] — given a withdrawal and the vault's
//!     current state, compute how many assets to release.
//!   - [`share_price_bps`] — the vault's NAV per share, in basis
//!     points relative to the inception 1:1 ratio.
//!
//! All math uses `i128` intermediates with saturating-to-i64/u64
//! conversions; no floats, no panics, deterministic across validators.
//! Floor division (round toward zero for positive numerators) is the
//! ERC-4626 convention — favors existing shareholders on deposit and
//! remaining shareholders on withdrawal.

use crate::types::SHARE_PRICE_BPS_SCALE;

/// Convert a deposit amount into the corresponding share allocation.
///
/// Returns `None` when:
///   - The vault is uninitialized (`total_shares == 0 && total_assets <= 0`)
///     and the deposit is non-positive — the caller is responsible
///     for the inception case (mint 1:1).
///   - The vault has shares but non-positive assets (insolvent) —
///     share price is undefined.
///   - Either input is non-positive in a configuration where the
///     output would underflow.
///
/// Formula (post-initialization, `total_assets > 0`):
/// ```text
///   shares_minted = floor(assets × total_shares / total_assets)
/// ```
///
/// Uses i128 intermediate to absorb the `assets × total_shares` product.
#[must_use]
pub fn assets_to_shares(assets: i64, total_shares: u64, total_assets: i64) -> Option<u64> {
    if assets <= 0 {
        return None;
    }
    // Inception: no shares yet → 1:1 mint.
    if total_shares == 0 {
        // Cap at u64::MAX; assets is positive so the cast is safe.
        return Some(u64::try_from(assets).unwrap_or(u64::MAX));
    }
    // Insolvent: shares exist but assets ≤ 0. Share price undefined.
    if total_assets <= 0 {
        return None;
    }
    // shares_minted = assets × total_shares / total_assets
    let numerator =
        i128::from(assets).saturating_mul(i128::from(total_shares));
    let denominator = i128::from(total_assets);
    let raw = numerator / denominator;
    if raw < 0 {
        return None;
    }
    Some(u64::try_from(raw).unwrap_or(u64::MAX))
}

/// Convert a share count into the corresponding asset release.
///
/// Returns `None` when:
///   - The vault is uninitialized (`total_shares == 0`) — there's
///     nothing to redeem against.
///   - `total_assets <= 0` (insolvent vault).
///   - `shares == 0` — zero-share withdrawals are caller bugs and
///     the layer above should have rejected.
///
/// Formula:
/// ```text
///   assets_returned = floor(shares × total_assets / total_shares)
/// ```
#[must_use]
pub fn shares_to_assets(shares: u64, total_shares: u64, total_assets: i64) -> Option<i64> {
    if shares == 0 || total_shares == 0 || total_assets <= 0 {
        return None;
    }
    // assets = shares × total_assets / total_shares
    let numerator =
        i128::from(shares).saturating_mul(i128::from(total_assets));
    let denominator = i128::from(total_shares);
    let raw = numerator / denominator;
    i64::try_from(raw).ok()
}

/// NAV per share in basis points relative to the inception 1:1 ratio.
///
/// At inception (`total_shares = 0`) the share price is conventionally
/// reported as `SHARE_PRICE_BPS_SCALE` (`10_000` bps = 1.0×).
///
/// At any other state with `total_assets > 0` and `total_shares > 0`,
/// the share price in bps is:
///
/// ```text
///   share_price_bps = floor(total_assets × SHARE_PRICE_BPS_SCALE / total_shares)
/// ```
///
/// A share price of `12_000` bps means each share is worth 1.2× its
/// inception value. Returns `None` for an insolvent vault
/// (`total_assets ≤ 0` with `total_shares > 0`); the caller should
/// surface this as "vault wound down" / "fund is impaired".
#[must_use]
pub fn share_price_bps(total_shares: u64, total_assets: i64) -> Option<i64> {
    if total_shares == 0 {
        return Some(SHARE_PRICE_BPS_SCALE);
    }
    if total_assets <= 0 {
        return None;
    }
    let numerator =
        i128::from(total_assets).saturating_mul(i128::from(SHARE_PRICE_BPS_SCALE));
    let denominator = i128::from(total_shares);
    let raw = numerator / denominator;
    i64::try_from(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ─── assets_to_shares ────────────────────────────────────────

    #[test]
    fn assets_to_shares_inception_is_1_to_1() {
        assert_eq!(assets_to_shares(100, 0, 0), Some(100));
    }

    #[test]
    fn assets_to_shares_at_par_share_price() {
        // 100 shares back 100 assets → adding 50 assets should mint 50 shares.
        assert_eq!(assets_to_shares(50, 100, 100), Some(50));
    }

    #[test]
    fn assets_to_shares_after_markup() {
        // Vault grew: 100 shares, 200 assets (share price = 2×).
        // 50 new assets → 50 × 100 / 200 = 25 shares.
        assert_eq!(assets_to_shares(50, 100, 200), Some(25));
    }

    #[test]
    fn assets_to_shares_after_markdown() {
        // Vault shrunk: 100 shares, 50 assets (share price = 0.5×).
        // 50 new assets → 50 × 100 / 50 = 100 shares.
        assert_eq!(assets_to_shares(50, 100, 50), Some(100));
    }

    #[test]
    fn assets_to_shares_zero_assets_returns_none() {
        assert_eq!(assets_to_shares(0, 100, 100), None);
    }

    #[test]
    fn assets_to_shares_negative_assets_returns_none() {
        assert_eq!(assets_to_shares(-10, 100, 100), None);
    }

    #[test]
    fn assets_to_shares_insolvent_vault_returns_none() {
        assert_eq!(assets_to_shares(50, 100, -1), None);
        assert_eq!(assets_to_shares(50, 100, 0), None);
    }

    #[test]
    fn assets_to_shares_rounds_down() {
        // 100 shares, 300 assets. Deposit 1 → 1 × 100 / 300 = 0 (floor).
        assert_eq!(assets_to_shares(1, 100, 300), Some(0));
    }

    // ─── shares_to_assets ────────────────────────────────────────

    #[test]
    fn shares_to_assets_at_par_share_price() {
        // 100 shares back 100 assets → burning 25 shares releases 25 assets.
        assert_eq!(shares_to_assets(25, 100, 100), Some(25));
    }

    #[test]
    fn shares_to_assets_after_markup() {
        // 100 shares, 200 assets → 50 shares × 200 / 100 = 100 assets.
        assert_eq!(shares_to_assets(50, 100, 200), Some(100));
    }

    #[test]
    fn shares_to_assets_after_markdown() {
        // 100 shares, 50 assets → 50 shares × 50 / 100 = 25 assets.
        assert_eq!(shares_to_assets(50, 100, 50), Some(25));
    }

    #[test]
    fn shares_to_assets_zero_shares_returns_none() {
        assert_eq!(shares_to_assets(0, 100, 100), None);
    }

    #[test]
    fn shares_to_assets_empty_vault_returns_none() {
        assert_eq!(shares_to_assets(50, 0, 100), None);
    }

    #[test]
    fn shares_to_assets_insolvent_returns_none() {
        assert_eq!(shares_to_assets(50, 100, -1), None);
        assert_eq!(shares_to_assets(50, 100, 0), None);
    }

    // ─── share_price_bps ─────────────────────────────────────────

    #[test]
    fn share_price_inception_is_par() {
        assert_eq!(share_price_bps(0, 0), Some(SHARE_PRICE_BPS_SCALE));
        // Even with positive assets, an empty vault has no NAV-per-share
        // distinct from "par"; the caller treats the empty case
        // separately. We report par for uniformity.
        assert_eq!(share_price_bps(0, 1_000), Some(SHARE_PRICE_BPS_SCALE));
    }

    #[test]
    fn share_price_at_par() {
        // 100 shares, 100 assets → 10_000 bps = 1.0×
        assert_eq!(share_price_bps(100, 100), Some(10_000));
    }

    #[test]
    fn share_price_2x_after_markup() {
        // 100 shares, 200 assets → 20_000 bps = 2.0×
        assert_eq!(share_price_bps(100, 200), Some(20_000));
    }

    #[test]
    fn share_price_half_after_markdown() {
        // 100 shares, 50 assets → 5_000 bps = 0.5×
        assert_eq!(share_price_bps(100, 50), Some(5_000));
    }

    #[test]
    fn share_price_insolvent_returns_none() {
        assert_eq!(share_price_bps(100, -1), None);
        assert_eq!(share_price_bps(100, 0), None);
    }

    // ─── proptest: invariants ────────────────────────────────────

    proptest! {
        /// Round-trip: depositing assets and immediately withdrawing
        /// the resulting shares never returns *more* than was deposited
        /// (rounding can leave residual dust in the vault).
        #[test]
        fn deposit_then_withdraw_no_inflation(
            assets in 1_i64..1_000_000_000,
            total_shares in 1_u64..1_000_000_000,
            total_assets in 1_i64..1_000_000_000,
        ) {
            let shares = assets_to_shares(assets, total_shares, total_assets).unwrap();
            // After deposit, vault state advances:
            let new_total_shares = total_shares.saturating_add(shares);
            let new_total_assets = total_assets.saturating_add(assets);
            // Withdraw those shares immediately:
            if let Some(returned) = shares_to_assets(shares, new_total_shares, new_total_assets) {
                prop_assert!(returned <= assets, "withdraw inflated: deposited {}, got back {}", assets, returned);
            }
        }

        /// Determinism: same inputs → same outputs.
        #[test]
        fn assets_to_shares_is_deterministic(
            assets in 1_i64..1_000_000_000,
            total_shares in 0_u64..1_000_000_000,
            total_assets in 1_i64..1_000_000_000,
        ) {
            let a = assets_to_shares(assets, total_shares, total_assets);
            let b = assets_to_shares(assets, total_shares, total_assets);
            prop_assert_eq!(a, b);
        }

        /// Monotonicity: depositing more assets at the same share price
        /// never mints fewer shares.
        #[test]
        fn assets_to_shares_monotonic_in_assets(
            assets_a in 1_i64..500_000,
            delta in 1_i64..500_000,
            total_shares in 1_u64..1_000_000,
            total_assets in 1_i64..1_000_000,
        ) {
            let shares_a = assets_to_shares(assets_a, total_shares, total_assets).unwrap();
            let shares_b = assets_to_shares(assets_a + delta, total_shares, total_assets).unwrap();
            prop_assert!(shares_b >= shares_a);
        }
    }
}
