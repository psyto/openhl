// Stage 19d: bin-crate `unreachable_pub` triggers on every public item
// since openhl has no library surface — silence module-wide, same
// pattern as `rpc.rs` / `seed_fixture.rs`.
#![allow(unreachable_pub)]

//! Pre-deployed read-only contracts that wrap openhl precompiles
//! (Stage 19d).
//!
//! ### Why
//!
//! Stage 17n shipped `openhl_margin_health` as an EVM precompile at
//! `0x...0c1f`. Stage 17f / 17k let smart contracts use it via the
//! `EvmFactory` plumbing during block execution. But standard
//! Ethereum clients (curl, ethers, viem) don't talk to precompile
//! addresses directly — they call **deployed contracts**. Stage 19d
//! closes the gap by injecting a tiny wrapper contract at a fixed
//! address via the dev chain's `Genesis.alloc`. The wrapper's
//! bytecode is the same 26-byte CALL-forwarder Stage 17f's tests
//! use, just permanently installed at a known address.
//!
//! From an external client's perspective the reader contract looks
//! like any other Solidity contract:
//!
//! ```text
//! interface IMarginHealthReader {
//!     // Returns 0=Indeterminate / 1=Safe / 2=AtRisk
//!     //       / 3=Liquidatable / 4=Underwater
//!     fallback(uint64 account) external view returns (uint256 tag);
//! }
//! ```
//!
//! Reachable today via `eth_call`. `eth_sendRawTransaction` would
//! ALSO route through here, but submitting a real signed tx
//! requires `build_payload` to integrate Reth's `PayloadBuilder`
//! and that's a separate stage (the "Solidity full-tx path"
//! still-synthetic bullet).
//!
//! ### Why margin_health and not deposit/withdraw
//!
//! `openhl_margin_health` is read-only — it doesn't mutate the
//! bridge's account map. That makes it a clean `eth_call` target
//! (eth_call is supposed to be read-only). The deposit / withdraw
//! precompiles DO mutate process-global state from any frame,
//! including the eth_call frame; until those mutations are
//! revert-aware in the eth_call sense (the Stage 17k revert guard
//! only restores on `REVERT`, not on the simulator unwinding the
//! whole call), wrapping them as deployed contracts callable via
//! eth_call would be a quiet correctness foot-gun.

use alloy_genesis::GenesisAccount;
use alloy_primitives::{address, Address, Bytes, U256};
use std::collections::BTreeMap;

/// Fixed address of the margin-health reader contract on the dev
/// chain. Stable across boots; documented for client integrations.
pub const MARGIN_HEALTH_READER: Address =
    address!("0x0000000000000000000000000000000000011101");

/// 26-byte wrapper bytecode (matches the Stage 17f helper, retargeted
/// to `OPENHL_MARGIN_HEALTH = 0x...0c1f`). Calldata layout: 32 bytes,
/// big-endian u64 account id in the last 8. Returns 32 bytes whose
/// last byte is the `MarginHealth` tag the precompile produces.
const MARGIN_HEALTH_READER_CODE: &[u8] = &[
    // Copy all calldata into memory[0..calldatasize].
    0x36, // CALLDATASIZE
    0x60, 0x00, // PUSH1 0
    0x60, 0x00, // PUSH1 0
    0x37, // CALLDATACOPY
    // CALL(gas, addr, value=0, in_off=0, in_size=calldatasize,
    //      out_off=0, out_size=32). Args pushed in reverse so
    //      `gas` lands on top.
    0x60, 0x20, // PUSH1 32    out_size
    0x60, 0x00, // PUSH1 0     out_off
    0x36, // CALLDATASIZE       in_size
    0x60, 0x00, // PUSH1 0     in_off
    0x60, 0x00, // PUSH1 0     value
    0x61, 0x0c, 0x1f, // PUSH2 0x0c1f (OPENHL_MARGIN_HEALTH)
    0x5a, // GAS
    0xf1, // CALL
    0x50, // POP (success flag)
    // Return memory[0..32].
    0x60, 0x20, // PUSH1 32
    0x60, 0x00, // PUSH1 0
    0xf3, // RETURN
];

/// Genesis-allocation entries for the reader contracts. Caller
/// merges these into `Genesis.alloc` before constructing the
/// `ChainSpec`.
pub fn genesis_alloc() -> BTreeMap<Address, GenesisAccount> {
    let mut alloc = BTreeMap::new();
    alloc.insert(
        MARGIN_HEALTH_READER,
        GenesisAccount {
            nonce: Some(1),
            balance: U256::ZERO,
            code: Some(Bytes::copy_from_slice(MARGIN_HEALTH_READER_CODE)),
            storage: None,
            private_key: None,
        },
    );
    alloc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn margin_health_reader_alloc_carries_bytecode_at_known_address() {
        let alloc = genesis_alloc();
        let entry = alloc
            .get(&MARGIN_HEALTH_READER)
            .expect("reader contract present");
        let code = entry.code.as_ref().expect("code installed");
        assert_eq!(code.len(), 26, "wrapper is 26 bytes");
        assert_eq!(
            code.as_ref(),
            MARGIN_HEALTH_READER_CODE,
            "alloc carries the exact wrapper bytecode",
        );
        // Sanity: the PUSH2 in the middle targets the margin_health
        // precompile (0x0c1f), not some other address.
        assert_eq!(&code[15..18], &[0x61, 0x0c, 0x1f]);
    }
}
