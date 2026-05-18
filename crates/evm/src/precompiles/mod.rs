//! Custom REVM precompiles that expose CLOB state to EVM execution.
//!
//! Stage 9b — live CLOB state. The precompile reads from a process-global
//! `Arc<Mutex<Book>>` that the bridge installs at construction. Hardcoded
//! values from 9a are gone; smart contracts now see real best-bid data.
//!
//! ### Why a process-global, not a closure-captured reference
//!
//! REVM's `PrecompileFn = fn(&[u8], u64, u64) -> PrecompileResult` is a
//! **function pointer**, not an `Fn` closure. Function pointers can't capture
//! environment, so the only way to get per-instance state into the precompile
//! is via global storage. The trade-off: only one CLOB can be installed
//! per process. For single-validator openhl deployments that's fine. Future
//! REVM versions may expand the precompile signature; until then, the global
//! is load-bearing infrastructure.
//!
//! Precompile address conventions:
//!   - openhl reserves the range `0x0000...0c1b` upwards (mnemonic: "CLB")
//!   - addresses 1-9 are Ethereum's standard precompiles (ECDSA recover etc.)
//!   - we stay well above those to avoid collisions

use alloy_evm::revm::precompile::{
    Precompile, PrecompileId, PrecompileOutput, PrecompileResult, Precompiles,
};
use alloy_primitives::{address, Address, Bytes};
use openhl_clob::{AccountId, Book, Order, OrderId, OrderType, Price, Qty, Side};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, RwLock,
};

/// Address of the "read best bid" precompile.
///
/// Solidity call shape: `staticcall(gas, 0x...0c1b, calldata=empty, ...) → (price: u256, qty: u256)`
pub const CLOB_READ_BEST_BID: Address = address!("0x0000000000000000000000000000000000000c1b");

/// Address of the "place order" precompile (write path — Stage 9c).
///
/// Solidity call shape (ABI-aligned 128-byte input):
/// `call(gas, 0x...0c1c, calldata=(uint64 account, uint8 side, uint64 price, uint64 qty), ...) → uint256 order_id`
///
/// `side` encoding: 0 = Buy, 1 = Sell. Any other value → call returns 0
/// (rejected, no state change). Order type is hardcoded to Limit at v0.
///
/// Return: 32 bytes; the last 8 are a big-endian u64 `order_id`. A return
/// of 0 means the order was rejected (no CLOB installed, malformed input,
/// or invalid side byte) — distinguishable from "placed" because allocated
/// IDs start at 1.
pub const CLOB_PLACE_ORDER: Address = address!("0x0000000000000000000000000000000000000c1c");

/// The minimum gas charge for invoking a CLOB precompile. Tuned later.
const CLOB_BASE_GAS_COST: u64 = 500;

/// Monotonic order-ID counter for orders placed via the EVM. Starts at 1
/// so the sentinel value 0 (returned on rejection) is distinguishable from
/// a successfully placed order.
///
/// **Single-validator caveat:** This is a process-global counter. For
/// multi-validator deployments, order IDs must come from consensus —
/// each validator's precompile must allocate the same ID for the same
/// EVM-side call, which means the counter has to be either deterministic
/// from input or read from a shared block-scoped state. Out of scope at v0.
static NEXT_ORDER_ID: AtomicU64 = AtomicU64::new(1);

/// Process-global handle to the CLOB the precompile reads from.
///
/// `None` until [`install_clob`] is called (typically by `LiveRethEvmBridge::new`).
/// While `None`, `read_best_bid` returns zero-encoded output rather than
/// erroring — this keeps existing tests deterministic and matches what an
/// uninitialised perp market would return on mainnet.
static CLOB_STATE: RwLock<Option<Arc<Mutex<Book>>>> = RwLock::new(None);

/// Install the CLOB instance the precompile should read from. The bridge
/// shares its `Arc<Mutex<Book>>` with the global so every EVM-side
/// `staticcall` to `CLOB_READ_BEST_BID` sees the same book the application
/// writes to via `submit_order`.
///
/// Calling this replaces any previously-installed CLOB. Production deployments
/// should call it exactly once at bridge construction.
pub fn install_clob(clob: Arc<Mutex<Book>>) {
    *CLOB_STATE.write().expect("CLOB_STATE rwlock poisoned") = Some(clob);
}

/// Clear the installed CLOB. Used by tests that need a clean slate; rare in
/// production. Idempotent — uninstalling when nothing is installed is a no-op.
pub fn uninstall_clob() {
    *CLOB_STATE.write().expect("CLOB_STATE rwlock poisoned") = None;
}

/// Read the currently-installed CLOB's best bid. Returns `None` if no CLOB
/// is installed or if the book has no bids. Public so tests can verify
/// install/uninstall without going through the precompile dispatch.
#[must_use]
pub fn current_best_bid() -> Option<(openhl_clob::Price, openhl_clob::Qty)> {
    let state = CLOB_STATE.read().expect("CLOB_STATE rwlock poisoned");
    let clob = state.as_ref()?;
    let book = clob.lock().expect("clob mutex poisoned");
    book.best_bid_with_qty()
}

/// Reads the best bid (highest-priced buy order's price + total qty at that
/// level) from the currently-installed CLOB and returns it as two
/// big-endian u256s (64 bytes total).
///
/// Encoding:
///   bytes  0..32  big-endian u256 price (0 if no bid or no CLOB installed)
///   bytes 32..64  big-endian u256 qty   (0 if no bid or no CLOB installed)
///
/// `PrecompileFn` signature is `fn(&[u8], u64, u64) -> PrecompileResult`;
/// the third arg is a `reservoir` value (extra gas budget) that we ignore
/// at v0. The Result wrapper is required by the signature even though we
/// never error — gas accounting is the EVM's responsibility.
#[allow(clippy::unnecessary_wraps)]
fn read_best_bid(_input: &[u8], _gas_limit: u64, _reservoir: u64) -> PrecompileResult {
    let mut out = vec![0u8; 64];

    if let Some((price, qty)) = current_best_bid() {
        // Big-endian u256: rightmost bytes carry the value.
        out[24..32].copy_from_slice(&price.0.to_be_bytes());
        out[56..64].copy_from_slice(&qty.0.to_be_bytes());
    }
    // If no CLOB is installed or there are no bids, `out` stays all zeros —
    // matches what an uninitialised perp market would return on mainnet.

    Ok(PrecompileOutput::new(CLOB_BASE_GAS_COST, Bytes::from(out), 0))
}

/// Place a limit order on the installed CLOB. The write counterpart to
/// `read_best_bid` — completes the EVM ↔ CLOB bidirectional surface.
///
/// Calldata layout (ABI-aligned, 128 bytes):
/// ```text
///   [  0.. 32]  account_id  (u64 in last 8 bytes)
///   [ 32.. 64]  side        (u8 in last byte: 0 = Buy, 1 = Sell)
///   [ 64.. 96]  price       (u64 in last 8 bytes)
///   [ 96..128]  qty         (u64 in last 8 bytes)
/// ```
///
/// Returns 32 bytes: the allocated `order_id` in the last 8 bytes, or zero
/// on rejection (no CLOB installed, malformed input, invalid side byte).
/// Allocated IDs start at 1, so zero is unambiguously "rejected".
///
/// Side note: the fills returned by `Book::submit` are discarded here.
/// Production-shape integration would route them through the bridge's
/// `pending_fills` so they reach the next `build_payload`. At v0 the
/// precompile and the bridge are write-side independent.
#[allow(clippy::unnecessary_wraps)]
fn place_order(input: &[u8], _gas_limit: u64, _reservoir: u64) -> PrecompileResult {
    let mut out = vec![0u8; 32];

    // Need exactly 128 bytes of input (4 × ABI-padded fields).
    if input.len() < 128 {
        return Ok(PrecompileOutput::new(CLOB_BASE_GAS_COST, Bytes::from(out), 0));
    }

    let account_id = u64_from_be_chunk(&input[0..32]);
    let side_byte = input[63];
    let price_value = u64_from_be_chunk(&input[64..96]);
    let qty_value = u64_from_be_chunk(&input[96..128]);

    let side = match side_byte {
        0 => Side::Buy,
        1 => Side::Sell,
        _ => return Ok(PrecompileOutput::new(CLOB_BASE_GAS_COST, Bytes::from(out), 0)),
    };

    // Reject orders with zero quantity outright — the book accepts them
    // technically, but a zero-qty order is always a bug from the caller.
    if qty_value == 0 {
        return Ok(PrecompileOutput::new(CLOB_BASE_GAS_COST, Bytes::from(out), 0));
    }

    let state = CLOB_STATE.read().expect("CLOB_STATE rwlock poisoned");
    let Some(clob) = state.as_ref() else {
        // No CLOB installed → 0 sentinel.
        return Ok(PrecompileOutput::new(CLOB_BASE_GAS_COST, Bytes::from(out), 0));
    };

    let order_id_val = NEXT_ORDER_ID.fetch_add(1, Ordering::Relaxed);

    let mut book = clob.lock().expect("clob mutex poisoned");
    let _result = book.submit(Order {
        id: OrderId(order_id_val),
        account: AccountId(account_id),
        side,
        qty: Qty(qty_value),
        order_type: OrderType::Limit {
            price: Price(price_value),
        },
    });
    drop(book);

    out[24..32].copy_from_slice(&order_id_val.to_be_bytes());
    Ok(PrecompileOutput::new(CLOB_BASE_GAS_COST, Bytes::from(out), 0))
}

/// Read a big-endian u64 from the last 8 bytes of a 32-byte ABI chunk.
fn u64_from_be_chunk(chunk: &[u8]) -> u64 {
    debug_assert!(chunk.len() == 32);
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&chunk[24..32]);
    u64::from_be_bytes(buf)
}

/// Build a `Precompiles` set that extends Reth's standard precompiles with
/// openhl's CLOB-reading + CLOB-writing additions. The base set is parameterized
/// over the hardfork's spec id so we inherit Ethereum's evolution (e.g., the
/// BLS-12-381 precompiles activated in Prague).
#[must_use]
pub fn openhl_precompiles(base: &Precompiles) -> Precompiles {
    let mut precompiles = base.clone();
    precompiles.extend([
        Precompile::new(
            PrecompileId::custom("clob_read_best_bid"),
            CLOB_READ_BEST_BID,
            read_best_bid,
        ),
        Precompile::new(
            PrecompileId::custom("clob_place_order"),
            CLOB_PLACE_ORDER,
            place_order,
        ),
    ]);
    precompiles
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use openhl_clob::{AccountId, Order, OrderId, OrderType, Price, Qty, Side};

    /// Tests in this module touch process-global `CLOB_STATE`. This mutex
    /// serializes them so parallel test execution can't observe a torn state.
    static TEST_SERIALIZER: Mutex<()> = Mutex::new(());

    /// With no CLOB installed, the precompile returns 64 zero bytes —
    /// matching what an uninitialised perp market would report on mainnet.
    #[test]
    fn read_best_bid_returns_zero_when_no_clob_installed() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_clob();

        let result = read_best_bid(&[], 100_000, 0).expect("precompile must not error");
        assert_eq!(result.bytes.len(), 64);
        let price = U256::from_be_slice(&result.bytes[0..32]);
        let qty = U256::from_be_slice(&result.bytes[32..64]);
        assert_eq!(price, U256::ZERO);
        assert_eq!(qty, U256::ZERO);
        assert_eq!(result.gas_used, CLOB_BASE_GAS_COST);
    }

    /// **Stage 9b end-to-end**: install a CLOB with a known bid, call the
    /// precompile, observe the live data flow through to the EVM-visible
    /// response. This is the moment custom EVM execution reads real
    /// orderbook state.
    #[test]
    fn read_best_bid_returns_live_state_when_clob_installed() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

        let book = Arc::new(Mutex::new(Book::new()));
        // Rest a buy @ 250 with qty 7
        book.lock().unwrap().submit(Order {
            id: OrderId(1),
            account: AccountId(42),
            side: Side::Buy,
            qty: Qty(7),
            order_type: OrderType::Limit { price: Price(250) },
        });
        // Rest another buy @ 240 (lower; shouldn't be picked as best bid)
        book.lock().unwrap().submit(Order {
            id: OrderId(2),
            account: AccountId(43),
            side: Side::Buy,
            qty: Qty(99),
            order_type: OrderType::Limit { price: Price(240) },
        });

        install_clob(book);

        let result = read_best_bid(&[], 100_000, 0).expect("precompile must not error");
        let price = U256::from_be_slice(&result.bytes[0..32]);
        let qty = U256::from_be_slice(&result.bytes[32..64]);
        assert_eq!(price, U256::from(250u64), "best bid is the 250 order, not 240");
        assert_eq!(qty, U256::from(7u64), "qty at the best level is 7");

        uninstall_clob();
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
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_clob();

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
        // No CLOB → zero output, matching read_best_bid_returns_zero_when_no_clob_installed.
        let price = U256::from_be_slice(&result.bytes[0..32]);
        assert_eq!(price, U256::ZERO);
    }

    /// Helper: build a 128-byte ABI-aligned `place_order` calldata buffer.
    fn place_order_calldata(account: u64, side: u8, price: u64, qty: u64) -> Vec<u8> {
        let mut buf = vec![0u8; 128];
        buf[24..32].copy_from_slice(&account.to_be_bytes());
        buf[63] = side;
        buf[88..96].copy_from_slice(&price.to_be_bytes());
        buf[120..128].copy_from_slice(&qty.to_be_bytes());
        buf
    }

    /// With no CLOB installed, `place_order` rejects (returns sentinel 0).
    #[test]
    fn place_order_returns_zero_when_no_clob_installed() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_clob();

        let calldata = place_order_calldata(42, 0, 100, 5);
        let result = place_order(&calldata, 100_000, 0).expect("precompile must not error");
        let order_id = U256::from_be_slice(&result.bytes[0..32]);
        assert_eq!(order_id, U256::ZERO);
    }

    /// `place_order` with bad input (too short, invalid side byte, zero qty)
    /// rejects without mutating state.
    #[test]
    fn place_order_rejects_malformed_input() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let book = Arc::new(Mutex::new(Book::new()));
        install_clob(book.clone());

        // Too short.
        let r = place_order(&[0u8; 64], 100_000, 0).unwrap();
        assert_eq!(U256::from_be_slice(&r.bytes[0..32]), U256::ZERO);
        assert_eq!(book.lock().unwrap().depth_bid(), 0, "no order on book after short input");

        // Invalid side byte.
        let bad_side = place_order_calldata(42, 7, 100, 5);
        let r = place_order(&bad_side, 100_000, 0).unwrap();
        assert_eq!(U256::from_be_slice(&r.bytes[0..32]), U256::ZERO);
        assert_eq!(book.lock().unwrap().depth_bid(), 0, "no order on book after bad side");

        // Zero qty.
        let zero_qty = place_order_calldata(42, 0, 100, 0);
        let r = place_order(&zero_qty, 100_000, 0).unwrap();
        assert_eq!(U256::from_be_slice(&r.bytes[0..32]), U256::ZERO);
        assert_eq!(book.lock().unwrap().depth_bid(), 0, "no order on book after zero qty");

        uninstall_clob();
    }

    /// **Stage 9c end-to-end (write side)**: place a Buy via the precompile,
    /// then read the best bid via the read precompile. The two-precompile
    /// round-trip is the moment the EVM ↔ CLOB surface becomes bidirectional.
    #[test]
    fn place_order_then_read_best_bid_round_trips() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let book = Arc::new(Mutex::new(Book::new()));
        install_clob(book);

        // EVM call: place Buy @ 175 with qty 12, account 0xABCD.
        let calldata = place_order_calldata(0xABCD, 0, 175, 12);
        let result = place_order(&calldata, 100_000, 0).expect("precompile must not error");
        let returned_id = U256::from_be_slice(&result.bytes[0..32]);
        assert!(
            returned_id > U256::ZERO,
            "place_order must return a non-zero order id on success"
        );

        // Now read the best bid via the read precompile. Should see our order.
        let read_result = read_best_bid(&[], 100_000, 0).expect("precompile must not error");
        let price = U256::from_be_slice(&read_result.bytes[0..32]);
        let qty = U256::from_be_slice(&read_result.bytes[32..64]);
        assert_eq!(price, U256::from(175u64), "best bid is the placed order's price");
        assert_eq!(qty, U256::from(12u64), "qty at best level matches placed qty");

        uninstall_clob();
    }
}
