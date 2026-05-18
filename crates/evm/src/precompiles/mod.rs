//! Custom REVM precompiles that expose CLOB state to EVM execution.
//!
//! Stage 9a — scout commit. One read precompile is defined and registered;
//! it returns a hardcoded best-bid response so smart contracts can prove
//! the precompile is reachable. Stages 9b+ wire the precompile to live
//! CLOB state and add write paths (`place_order` from EVM context).
//!
//! Precompile address conventions:
//!   - openhl reserves the range `0x0000...0c1b` upwards (mnemonic: "CLB")
//!   - addresses 1-9 are Ethereum's standard precompiles (ECDSA recover etc.)
//!   - we stay well above those to avoid collisions

use alloy_evm::revm::precompile::{
    Precompile, PrecompileId, PrecompileOutput, PrecompileResult, Precompiles,
};
use alloy_primitives::{address, Address, Bytes};

/// Address of the "read best bid" precompile.
///
/// Solidity call shape: `staticcall(gas, 0x...0c1b, calldata=empty, ...) → (price: u256, qty: u256)`
pub const CLOB_READ_BEST_BID: Address = address!("0x0000000000000000000000000000000000000c1b");

/// The minimum gas charge for invoking a CLOB precompile. Tuned later.
const CLOB_BASE_GAS_COST: u64 = 500;

/// Stage 9a stub: returns a hardcoded best bid so the precompile is callable
/// without requiring live CLOB state injection. Stage 9b replaces this with
/// an `Arc<Mutex<Book>>`-aware closure captured into the precompile.
///
/// `PrecompileFn` signature is `fn(&[u8], u64, u64) -> PrecompileResult`;
/// the third arg is a `reservoir` value (extra gas budget) that we ignore
/// at v0.
///
/// Encoding: 64 bytes total
///   bytes  0..32  big-endian u256 price (hardcoded 100)
///   bytes 32..64  big-endian u256 qty   (hardcoded 10)
// `PrecompileFn` signature mandates the `PrecompileResult` (i.e. `Result`)
// return type. Our v0 stub never errors — gas accounting is the EVM's
// responsibility — but the wrapper is structurally required.
#[allow(clippy::unnecessary_wraps)]
fn read_best_bid(_input: &[u8], _gas_limit: u64, _reservoir: u64) -> PrecompileResult {
    let mut out = vec![0u8; 64];
    // price = 100 (big-endian u256, rightmost byte holds the value)
    out[31] = 100;
    // qty = 10
    out[63] = 10;

    Ok(PrecompileOutput::new(CLOB_BASE_GAS_COST, Bytes::from(out), 0))
}

/// Build a `Precompiles` set that extends Reth's standard precompiles with
/// openhl's CLOB-reading additions. The base set is parameterized over the
/// hardfork's spec id so we inherit Ethereum's evolution (e.g., the
/// BLS-12-381 precompiles activated in Prague).
#[must_use]
pub fn openhl_precompiles(base: &Precompiles) -> Precompiles {
    let mut precompiles = base.clone();
    precompiles.extend([Precompile::new(
        PrecompileId::custom("clob_read_best_bid"),
        CLOB_READ_BEST_BID,
        read_best_bid,
    )]);
    precompiles
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    /// Direct unit test of the precompile function: invoked with empty input,
    /// it returns the hardcoded (price=100, qty=10) as 64 big-endian u256 bytes.
    #[test]
    fn read_best_bid_returns_hardcoded_price_and_qty() {
        let result = read_best_bid(&[], 100_000, 0).expect("precompile must not error");
        assert_eq!(result.bytes.len(), 64);
        let price = U256::from_be_slice(&result.bytes[0..32]);
        let qty = U256::from_be_slice(&result.bytes[32..64]);
        assert_eq!(price, U256::from(100u64));
        assert_eq!(qty, U256::from(10u64));
        assert_eq!(result.gas_used, CLOB_BASE_GAS_COST);
    }

    /// Registry test: `openhl_precompiles()` extends a base precompile set
    /// with our CLOB precompile at the well-known address. This is what the
    /// Stage 9a `EvmFactory` plugs into every EVM instance Reth constructs.
    #[test]
    fn openhl_precompiles_registers_clob_address() {
        let base = Precompiles::cancun();
        let extended = openhl_precompiles(base);

        // The CLOB address must be in the extended set.
        assert!(
            extended.contains(&CLOB_READ_BEST_BID),
            "openhl_precompiles must register the CLOB_READ_BEST_BID address"
        );

        // The base Ethereum precompiles (e.g. ECDSA recover at 0x...01) must
        // still be present — we EXTEND, not replace.
        let ecrecover: Address = alloy_primitives::address!("0x0000000000000000000000000000000000000001");
        assert!(
            extended.contains(&ecrecover),
            "extended set must retain base Ethereum precompiles"
        );
    }

    /// Invoke the registered precompile end-to-end through the registry
    /// (rather than calling `read_best_bid` directly). This proves the
    /// registration is wired such that an EVM dispatch to the address hits
    /// our function — the same path Reth's EVM uses on `staticcall` to
    /// `CLOB_READ_BEST_BID`.
    #[test]
    fn registered_precompile_is_invokable_via_registry() {
        let extended = openhl_precompiles(Precompiles::cancun());
        let precompile = extended
            .get(&CLOB_READ_BEST_BID)
            .expect("CLOB precompile must be registered");

        // Precompile::execute is the public dispatch method — same as what
        // the EVM calls internally when a contract STATICCALLs the address.
        let result = precompile
            .execute(&[], 100_000, 0)
            .expect("call must not error");
        assert_eq!(result.bytes.len(), 64);
        let price = U256::from_be_slice(&result.bytes[0..32]);
        let qty = U256::from_be_slice(&result.bytes[32..64]);
        assert_eq!(price, U256::from(100u64));
        assert_eq!(qty, U256::from(10u64));
    }
}
