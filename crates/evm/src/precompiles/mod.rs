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
use openhl_clearing::Account;
use openhl_clob::{AccountId, Book, Fill, Order, OrderId, OrderType, Price, Qty, Side};
use openhl_funding::Notional;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU32, AtomicU64, Ordering},
    Arc, Mutex, RwLock,
};

mod revert_guard;
pub use revert_guard::OpenHlRevertGuard;

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

/// Address of the "deposit collateral" precompile (Stage 17c).
///
/// Solidity call shape (ABI-aligned 64-byte input):
/// `call(gas, 0x...0c1d, calldata=(uint64 account, int64 amount), ...) → uint256 new_balance`
///
/// `amount` is signed (encoded as a 32-byte two's-complement big-endian
/// integer); positive credits the account's collateral, negative debits.
/// The returned `new_balance` is the post-deposit collateral as a
/// signed 256-bit integer. A return of 0 means "rejected" — currently
/// only triggered when no account map is installed; in production it
/// would also fire on malformed input or unauthorized accounts.
///
/// Same caveat as `clob_place_order` (and a known v0 limitation): the
/// mutation lands in the bridge's account map regardless of whether
/// the calling EVM transaction reverts. Tying mutations to the
/// transaction's success is a future hardening item.
pub const OPENHL_DEPOSIT: Address = address!("0x0000000000000000000000000000000000000c1d");

/// Address of the "withdraw collateral" precompile (Stage 17e).
///
/// Solidity call shape (ABI-aligned 64-byte input):
/// `call(gas, 0x...0c1e, calldata=(uint64 account, uint64 amount), ...) → uint256 new_balance_or_zero`
///
/// Returns the new collateral balance as a 32-byte sign-extended
/// int. Returns `1` packed in the rightmost byte (i.e., a 32-byte
/// value equal to `1`) is technically achievable for a real balance
/// of 1, so callers should distinguish success from rejection by
/// comparing against the pre-call balance rather than reading the
/// return alone.
///
/// **The zero-return is overloaded with success:** if the post-
/// withdraw balance happens to be exactly 0 (the caller drained
/// the account), the return is also 0. Future hardening should
/// use a richer return shape — for v0 the simplicity wins.
///
/// Rejections (no account map installed, account doesn't exist,
/// insufficient balance, input shorter than 64 bytes) return all
/// zeros. Same caveat as the other write precompiles: the
/// withdrawal lands regardless of whether the calling EVM
/// transaction reverts.
pub const OPENHL_WITHDRAW: Address = address!("0x0000000000000000000000000000000000000c1e");

/// The minimum gas charge for invoking a CLOB precompile. Tuned later.
const CLOB_BASE_GAS_COST: u64 = 500;

/// Base gas for the deposit precompile (Stage 17c). Same magnitude as
/// the CLOB precompiles; tuned later.
const DEPOSIT_BASE_GAS_COST: u64 = 500;

/// Base gas for the withdraw precompile (Stage 17e). Same magnitude.
const WITHDRAW_BASE_GAS_COST: u64 = 500;

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

/// Process-global handle to the buffer where the precompile pushes fills.
///
/// Same lifecycle rules as `CLOB_STATE`: installed by `LiveRethEvmBridge::new`,
/// none until set. When set, `place_order` extends this buffer with any fills
/// produced by the matched order, so production-shape EVM-placed orders flow
/// into the next `build_payload`'s drained fills exactly like bridge-side
/// `submit_order` does.
static FILL_SINK: RwLock<Option<Arc<Mutex<Vec<Fill>>>>> = RwLock::new(None);

/// Install the `pending_fills` buffer the precompile should write to.
/// Companion to `install_clob`. Calling this replaces any previously-installed
/// sink.
pub fn install_fill_sink(sink: Arc<Mutex<Vec<Fill>>>) {
    *FILL_SINK.write().expect("FILL_SINK rwlock poisoned") = Some(sink);
}

/// Clear the installed fill sink. Test-only typical use; idempotent.
pub fn uninstall_fill_sink() {
    *FILL_SINK.write().expect("FILL_SINK rwlock poisoned") = None;
}

/// Process-global handle to the bridge's per-account state map (Stage
/// 17c). When installed, the deposit precompile mutates this same map
/// that `LiveRethEvmBridge::deposit` / `submit_order` write to, so an
/// EVM-side deposit and a Rust-side bridge deposit are equivalent
/// state changes.
static ACCOUNTS_STATE: RwLock<Option<Arc<Mutex<HashMap<AccountId, Account>>>>> =
    RwLock::new(None);

/// Install the account map the deposit precompile should mutate.
/// Companion to `install_clob` / `install_fill_sink`; same lifecycle.
pub fn install_accounts(accounts: Arc<Mutex<HashMap<AccountId, Account>>>) {
    *ACCOUNTS_STATE.write().expect("ACCOUNTS_STATE rwlock poisoned") = Some(accounts);
}

/// Clear the installed account map. Test-only typical use; idempotent.
pub fn uninstall_accounts() {
    *ACCOUNTS_STATE.write().expect("ACCOUNTS_STATE rwlock poisoned") = None;
}

/// Stage 17l — process-global initial-margin rate (bps) used by
/// the [`openhl_withdraw`](OPENHL_WITHDRAW) precompile when
/// computing free collateral. Default
/// [`openhl_clearing::DEFAULT_INITIAL_MARGIN_BPS`] (1000 bps,
/// matching `LiquidationParams::hyperliquid_default`).
///
/// Bridge constructor sets this via
/// [`LiveRethEvmBridge::with_initial_margin_bps`](crate::live_node::LiveRethEvmBridge::with_initial_margin_bps)
/// so the bridge's Rust-side `withdraw` and the EVM-side precompile
/// always read the same rate — the "EVM and Rust deposit/withdraw
/// are equivalent state changes" property in `docs/architecture.md`
/// depends on this lockstep.
static INITIAL_MARGIN_BPS: AtomicU32 =
    AtomicU32::new(openhl_clearing::DEFAULT_INITIAL_MARGIN_BPS);

/// Set the process-global initial-margin rate the withdraw
/// precompile uses. Returns the previous value (rarely useful;
/// helps tests restore between runs).
pub fn install_initial_margin_bps(bps: u32) -> u32 {
    INITIAL_MARGIN_BPS.swap(bps, Ordering::Relaxed)
}

/// Read the process-global initial-margin rate. Mirrors
/// [`install_initial_margin_bps`]; intended for the bridge's
/// withdraw helper to consult so it doesn't diverge from the
/// precompile path.
#[must_use]
pub fn current_initial_margin_bps() -> u32 {
    INITIAL_MARGIN_BPS.load(Ordering::Relaxed)
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

/// Stage 17j — read the currently-installed CLOB's midpoint as a
/// [`openhl_funding::MarkPrice`]. Returns `None` when no CLOB is
/// installed or either side of the book is empty; that's the signal
/// the withdraw precompile uses to fall back to the avg-entry IM
/// rule. Mirror of [`crate::live_node::LiveRethEvmBridge::current_mark`]
/// so the EVM-side check and the bridge's Rust-side check stay in
/// lockstep.
#[must_use]
pub fn current_mark() -> Option<openhl_funding::MarkPrice> {
    let state = CLOB_STATE.read().expect("CLOB_STATE rwlock poisoned");
    let clob = state.as_ref()?;
    let book = clob.lock().expect("clob mutex poisoned");
    let bid = book.best_bid()?;
    let ask = book.best_ask()?;
    Some(openhl_funding::MarkPrice((bid.0 + ask.0) / 2))
}

/// Stage 17i: in-memory snapshot of every mutating bridge global —
/// `{accounts, book, pending_fills}`. Used by [`revert_guard`] to
/// roll back precompile mutations when the calling EVM frame
/// reverts.
///
/// All three fields are `Option` so a snapshot is meaningful even
/// before any of the globals are installed (e.g., the read-only
/// CLOB precompile under tests that haven't wired in the account
/// map).
#[derive(Debug, Default)]
pub(crate) struct BridgeStateSnapshot {
    accounts: Option<HashMap<AccountId, Account>>,
    book: Option<Book>,
    fills: Option<Vec<Fill>>,
}

/// Clone the contents of every currently-installed mutating global.
/// Cheap for v0 (5-account dev state); a production rewrite would
/// move to per-mutation journal entries instead of whole-state
/// clones, mirroring REVM's storage journal.
#[must_use]
pub(crate) fn snapshot_bridge_state() -> BridgeStateSnapshot {
    let accounts = {
        let state = ACCOUNTS_STATE
            .read()
            .expect("ACCOUNTS_STATE rwlock poisoned");
        state
            .as_ref()
            .map(|a| a.lock().expect("accounts mutex poisoned").clone())
    };
    let book = {
        let state = CLOB_STATE.read().expect("CLOB_STATE rwlock poisoned");
        state
            .as_ref()
            .map(|c| c.lock().expect("clob mutex poisoned").clone())
    };
    let fills = {
        let state = FILL_SINK.read().expect("FILL_SINK rwlock poisoned");
        state
            .as_ref()
            .map(|f| f.lock().expect("fill_sink mutex poisoned").clone())
    };
    BridgeStateSnapshot {
        accounts,
        book,
        fills,
    }
}

/// Overwrite the contents of every installed mutating global with
/// the values captured by [`snapshot_bridge_state`]. Preserves the
/// `Arc` identity (so consumers holding a clone of the Arc see the
/// restored state); only the data behind the lock is replaced.
///
/// `None` fields are skipped — if a global wasn't installed at
/// snapshot time, this leaves whatever's currently installed in
/// place.
pub(crate) fn restore_bridge_state(snap: BridgeStateSnapshot) {
    if let Some(snap_accounts) = snap.accounts {
        let state = ACCOUNTS_STATE
            .read()
            .expect("ACCOUNTS_STATE rwlock poisoned");
        if let Some(arc) = state.as_ref() {
            *arc.lock().expect("accounts mutex poisoned") = snap_accounts;
        }
    }
    if let Some(snap_book) = snap.book {
        let state = CLOB_STATE.read().expect("CLOB_STATE rwlock poisoned");
        if let Some(arc) = state.as_ref() {
            *arc.lock().expect("clob mutex poisoned") = snap_book;
        }
    }
    if let Some(snap_fills) = snap.fills {
        let state = FILL_SINK.read().expect("FILL_SINK rwlock poisoned");
        if let Some(arc) = state.as_ref() {
            *arc.lock().expect("fill_sink mutex poisoned") = snap_fills;
        }
    }
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
/// Stage 9c+ (this commit): any fills produced by the submit are pushed into
/// the `FILL_SINK` global if installed. This is what makes EVM-placed orders
/// flow into the bridge's `pending_fills` and out via `build_payload`,
/// matching the bridge-side `submit_order` semantics. If no sink is
/// installed the fills are still produced (visible via subsequent
/// `read_best_bid`) but won't reach a payload.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn place_order(input: &[u8], _gas_limit: u64, _reservoir: u64) -> PrecompileResult {
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
    let submit_result = book.submit(Order {
        id: OrderId(order_id_val),
        account: AccountId(account_id),
        side,
        qty: Qty(qty_value),
        order_type: OrderType::Limit {
            price: Price(price_value),
        },
    });
    drop(book);

    // Stage 9c+: route any fills produced by this order through the bridge's
    // pending_fills buffer so they reach the next `build_payload`. Drops
    // silently if no sink is installed (consistent with no-CLOB → return 0).
    if !submit_result.fills.is_empty() {
        let sink_state = FILL_SINK.read().expect("FILL_SINK rwlock poisoned");
        if let Some(sink) = sink_state.as_ref() {
            sink.lock()
                .expect("fill_sink mutex poisoned")
                .extend(submit_result.fills.iter().copied());
        }
    }

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

/// Read a big-endian signed i64 from a 32-byte ABI chunk. Solidity's
/// `int256` encoding is sign-extended to 32 bytes; we take the
/// upper 24 bytes as the sign-extension and the last 8 bytes as the
/// magnitude. Values outside `i64` range saturate.
fn i64_from_be_chunk(chunk: &[u8]) -> i64 {
    debug_assert!(chunk.len() == 32);
    // Sign-extension check: bytes 0..24 must all match the sign bit
    // of byte 24 for the value to fit in i64. If they don't, we
    // saturate to i64::MIN or i64::MAX.
    let sign_byte = chunk[24];
    let sign_ext = if sign_byte & 0x80 != 0 { 0xff } else { 0x00 };
    if chunk[..24].iter().any(|&b| b != sign_ext) {
        return if sign_byte & 0x80 != 0 { i64::MIN } else { i64::MAX };
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&chunk[24..32]);
    i64::from_be_bytes(buf)
}

/// Deposit collateral on behalf of an account (Stage 17c).
///
/// Calldata (64 bytes):
///   bytes  0..32  account_id (last 8 bytes are the u64; upper bytes ignored)
///   bytes 32..64  amount     (full 32-byte sign-extended int256)
///
/// Returns 32 bytes: the post-deposit collateral as a big-endian
/// `int256` (sign-extended). Returns all zeros when rejected (no
/// account map installed, or input shorter than 64 bytes).
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn deposit(input: &[u8], _gas_limit: u64, _reservoir: u64) -> PrecompileResult {
    let mut out = vec![0u8; 32];

    if input.len() < 64 {
        return Ok(PrecompileOutput::new(
            DEPOSIT_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    }

    let account_id = u64_from_be_chunk(&input[0..32]);
    let amount = i64_from_be_chunk(&input[32..64]);

    let state = ACCOUNTS_STATE
        .read()
        .expect("ACCOUNTS_STATE rwlock poisoned");
    let Some(accounts) = state.as_ref() else {
        return Ok(PrecompileOutput::new(
            DEPOSIT_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    };

    let mut map = accounts.lock().expect("accounts mutex poisoned");
    let acct = map
        .entry(AccountId(account_id))
        .or_insert_with(|| Account::flat(AccountId(account_id)));
    acct.collateral = Notional(acct.collateral.0.saturating_add(amount));
    let new_balance = acct.collateral.0;
    drop(map);
    drop(state);

    // Encode i64 → 32-byte sign-extended big-endian.
    let sign_ext: u8 = if new_balance < 0 { 0xff } else { 0x00 };
    for b in &mut out[..24] {
        *b = sign_ext;
    }
    out[24..32].copy_from_slice(&new_balance.to_be_bytes());
    Ok(PrecompileOutput::new(
        DEPOSIT_BASE_GAS_COST,
        Bytes::from(out),
        0,
    ))
}

/// Withdraw collateral from an account (Stage 17e). Companion to
/// [`deposit`].
///
/// Calldata (64 bytes):
///   bytes  0..32  account_id (last 8 bytes are the u64)
///   bytes 32..64  amount     (last 8 bytes are the u64; upper bytes ignored)
///
/// Returns 32 bytes: the post-withdraw collateral as a big-endian
/// sign-extended int256 on success. Returns all zeros when
/// rejected (no map installed, account doesn't exist, insufficient
/// balance, input shorter than 64 bytes). Note: a successful
/// withdraw that drains to exactly 0 also returns 0 — callers
/// distinguishing success from rejection should read the
/// pre-call balance separately.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn withdraw(input: &[u8], _gas_limit: u64, _reservoir: u64) -> PrecompileResult {
    let mut out = vec![0u8; 32];

    if input.len() < 64 {
        return Ok(PrecompileOutput::new(
            WITHDRAW_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    }

    let account_id = u64_from_be_chunk(&input[0..32]);
    let amount = u64_from_be_chunk(&input[32..64]);

    let state = ACCOUNTS_STATE
        .read()
        .expect("ACCOUNTS_STATE rwlock poisoned");
    let Some(accounts) = state.as_ref() else {
        return Ok(PrecompileOutput::new(
            WITHDRAW_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    };

    let mut map = accounts.lock().expect("accounts mutex poisoned");
    let Some(acct) = map.get_mut(&AccountId(account_id)) else {
        return Ok(PrecompileOutput::new(
            WITHDRAW_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    };
    let Ok(amount_i64) = i64::try_from(amount) else {
        return Ok(PrecompileOutput::new(
            WITHDRAW_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    };
    // Stage 17j: mark-aware free collateral. With a two-sided CLOB
    // book, use the midpoint as mark and the production-shape
    // `(equity − IM_req_at_mark)` rule. Without a book midpoint, the
    // helper falls back to the Stage 17g avg-entry rule. For a flat
    // position both reduce to a raw-collateral check.
    //
    // Stage 17l: the IM rate is read from the process-global
    // `INITIAL_MARGIN_BPS` (installed by
    // `LiveRethEvmBridge::with_initial_margin_bps`), so this stays
    // in lockstep with the bridge's Rust-side withdraw rule.
    let free = crate::live_node::withdraw_free_collateral(
        acct,
        current_mark(),
        current_initial_margin_bps(),
    );
    if i128::from(amount_i64) > i128::from(free) {
        return Ok(PrecompileOutput::new(
            WITHDRAW_BASE_GAS_COST,
            Bytes::from(out),
            0,
        ));
    }
    acct.collateral = Notional(acct.collateral.0 - amount_i64);
    let new_balance = acct.collateral.0;
    drop(map);
    drop(state);

    let sign_ext: u8 = if new_balance < 0 { 0xff } else { 0x00 };
    for b in &mut out[..24] {
        *b = sign_ext;
    }
    out[24..32].copy_from_slice(&new_balance.to_be_bytes());
    Ok(PrecompileOutput::new(
        WITHDRAW_BASE_GAS_COST,
        Bytes::from(out),
        0,
    ))
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
        Precompile::new(
            PrecompileId::custom("openhl_deposit"),
            OPENHL_DEPOSIT,
            deposit,
        ),
        Precompile::new(
            PrecompileId::custom("openhl_withdraw"),
            OPENHL_WITHDRAW,
            withdraw,
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

    /// **Stage 9c+**: when a `FILL_SINK` is installed alongside the CLOB,
    /// fills produced by a `place_order` call flow into the sink. This is the
    /// hook the bridge relies on to surface EVM-placed fills in the next
    /// `build_payload`. With no sink installed, fills are still produced but
    /// Stage 17c: build 64-byte deposit calldata `(uint64 account,
    /// int64 amount)` ABI-aligned to 32-byte chunks.
    fn deposit_calldata(account: u64, amount: i64) -> Vec<u8> {
        let mut input = vec![0u8; 64];
        input[24..32].copy_from_slice(&account.to_be_bytes());
        // Sign-extend amount into bytes 32..64.
        let sign_byte = if amount < 0 { 0xff } else { 0x00 };
        for b in &mut input[32..56] {
            *b = sign_byte;
        }
        input[56..64].copy_from_slice(&amount.to_be_bytes());
        input
    }

    /// Stage 17c: without an installed account map, deposit returns
    /// the zero sentinel — same shape as `place_order` / `read_best_bid`.
    #[test]
    fn deposit_returns_zero_when_no_accounts_installed() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_accounts();

        let calldata = deposit_calldata(42, 500);
        let r = deposit(&calldata, 100_000, 0).expect("precompile must not error");
        assert_eq!(r.bytes.len(), 32);
        assert_eq!(U256::from_be_slice(&r.bytes[..32]), U256::ZERO);
        assert_eq!(r.gas_used, DEPOSIT_BASE_GAS_COST);
    }

    /// Stage 17c: a first-time deposit creates the flat account
    /// and credits collateral. Returns the new balance encoded as
    /// a 32-byte sign-extended int.
    #[test]
    fn deposit_creates_account_and_credits_balance() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        let calldata = deposit_calldata(42, 750);
        let r = deposit(&calldata, 100_000, 0).unwrap();
        // 32-byte big-endian decoding of a positive i64 = 750.
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&r.bytes[24..32]);
        assert_eq!(i64::from_be_bytes(buf), 750);

        let map = accounts.lock().unwrap();
        let acct = map.get(&AccountId(42)).expect("account created on deposit");
        assert_eq!(acct.collateral, Notional(750));

        uninstall_accounts();
    }

    /// Stage 17e: build 64-byte withdraw calldata `(uint64 account,
    /// uint64 amount)` ABI-aligned to 32-byte chunks.
    fn withdraw_calldata(account: u64, amount: u64) -> Vec<u8> {
        let mut input = vec![0u8; 64];
        input[24..32].copy_from_slice(&account.to_be_bytes());
        input[56..64].copy_from_slice(&amount.to_be_bytes());
        input
    }

    /// Stage 17e: with no map installed, withdraw returns zero
    /// like its companion precompiles.
    #[test]
    fn withdraw_returns_zero_when_no_accounts_installed() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_accounts();

        let r = withdraw(&withdraw_calldata(42, 100), 100_000, 0).unwrap();
        assert_eq!(r.bytes.len(), 32);
        assert_eq!(U256::from_be_slice(&r.bytes[..32]), U256::ZERO);
        assert_eq!(r.gas_used, WITHDRAW_BASE_GAS_COST);
    }

    /// Stage 17e: withdraw rejects an unknown account.
    #[test]
    fn withdraw_rejects_unknown_account() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        let r = withdraw(&withdraw_calldata(42, 100), 100_000, 0).unwrap();
        assert_eq!(U256::from_be_slice(&r.bytes[..32]), U256::ZERO);
        assert!(accounts.lock().unwrap().is_empty(), "no account materialized");

        uninstall_accounts();
    }

    /// Stage 17e: withdraw rejects when balance is insufficient.
    #[test]
    fn withdraw_rejects_insufficient_balance() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        let _ = deposit(&deposit_calldata(7, 100), 100_000, 0).unwrap();
        // Try to take more than is there.
        let r = withdraw(&withdraw_calldata(7, 250), 100_000, 0).unwrap();
        assert_eq!(U256::from_be_slice(&r.bytes[..32]), U256::ZERO);
        // Balance untouched.
        assert_eq!(
            accounts.lock().unwrap().get(&AccountId(7)).unwrap().collateral,
            Notional(100)
        );

        uninstall_accounts();
    }

    /// Stage 17e: happy path — deposit, then withdraw, balance
    /// reflected in the return AND in the map.
    #[test]
    fn withdraw_debits_balance_on_success() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        let _ = deposit(&deposit_calldata(7, 1000), 100_000, 0).unwrap();
        let r = withdraw(&withdraw_calldata(7, 300), 100_000, 0).unwrap();

        let mut buf = [0u8; 8];
        buf.copy_from_slice(&r.bytes[24..32]);
        assert_eq!(i64::from_be_bytes(buf), 700);

        assert_eq!(
            accounts.lock().unwrap().get(&AccountId(7)).unwrap().collateral,
            Notional(700)
        );

        uninstall_accounts();
    }

    /// Stage 17j: when a CLOB with a two-sided book is installed,
    /// the precompile uses mark-aware free collateral — a long at
    /// a gain can withdraw against unrealized profits, mirroring
    /// the bridge's Rust-side rule. Sanity-check that the EVM-side
    /// withdraw stays byte-identical with `bridge.withdraw`.
    #[test]
    fn withdraw_precompile_uses_mark_aware_free_collateral_at_gain() {
        use openhl_funding::{MarkPrice, PositionSize};

        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Defensive: prior tests may have left a CLOB installed
        // whose midpoint isn't what this test wants.
        uninstall_clob();
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        // Install a CLOB with bid 119 / ask 121 → midpoint 120.
        let book = Arc::new(Mutex::new(Book::new()));
        book.lock().unwrap().submit(Order {
            id: OrderId(101),
            account: AccountId(99),
            side: Side::Buy,
            qty: Qty(1),
            order_type: OrderType::Limit { price: Price(119) },
        });
        book.lock().unwrap().submit(Order {
            id: OrderId(102),
            account: AccountId(98),
            side: Side::Sell,
            qty: Qty(1),
            order_type: OrderType::Limit { price: Price(121) },
        });
        install_clob(Arc::clone(&book));
        assert_eq!(current_mark(), Some(MarkPrice(120)));

        // Long 10 @ 100, collateral 500. At mark 120: uPnL=+200,
        // equity=700, IM=120, free=580.
        accounts.lock().unwrap().insert(
            AccountId(42),
            Account {
                account: AccountId(42),
                position_size: PositionSize(10),
                avg_entry: MarkPrice(100),
                collateral: Notional(500),
            },
        );

        // One above free → reject; at free → succeeds with balance
        // = 500 - 580 = -80 (deficit absorbed by the gain).
        let r = withdraw(&withdraw_calldata(42, 581), 100_000, 0).unwrap();
        assert!(r.bytes.iter().all(|&b| b == 0), "above free → reject");
        let r = withdraw(&withdraw_calldata(42, 580), 100_000, 0).unwrap();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&r.bytes[24..32]);
        assert_eq!(i64::from_be_bytes(buf), -80);
        assert_eq!(
            accounts.lock().unwrap().get(&AccountId(42)).unwrap().collateral,
            Notional(-80),
        );

        uninstall_accounts();
        uninstall_clob();
    }

    /// Stage 17g: with an open position installed, the precompile
    /// rejects a withdraw that would breach the initial-margin
    /// requirement — same rule the bridge enforces. A boundary
    /// withdraw (post-balance == IM_req) is allowed; one more quote
    /// is not.
    ///
    /// Stage 17j note: this test installs no CLOB, so the
    /// `current_mark()` fallback puts us on the Stage 17g
    /// avg-entry rule — exactly what this test expects.
    #[test]
    fn withdraw_precompile_respects_initial_margin() {
        use openhl_clearing::DEFAULT_INITIAL_MARGIN_BPS;
        use openhl_funding::{MarkPrice, PositionSize};

        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        // Stage 17j: defensive clear of any leftover CLOB so the
        // fallback (no mark → avg-entry rule) actually fires.
        uninstall_clob();
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        // Open-position account: size=10, avg_entry=100, collateral=500.
        // IM_req at default 1000 bps = 10*100*1000/10000 = 100, so
        // free collateral = 400.
        accounts.lock().unwrap().insert(
            AccountId(42),
            Account {
                account: AccountId(42),
                position_size: PositionSize(10),
                avg_entry: MarkPrice(100),
                collateral: Notional(500),
            },
        );
        // Sanity-check the IM math against the helper itself so this
        // test is robust to a future DEFAULT bps tweak.
        let acct_snapshot = *accounts.lock().unwrap().get(&AccountId(42)).unwrap();
        let im_req = openhl_clearing::initial_margin_requirement(
            &acct_snapshot,
            DEFAULT_INITIAL_MARGIN_BPS,
        );
        assert_eq!(im_req, 100, "IM_req sanity check");

        // 1 wei past the free-collateral line → reject (sentinel zero).
        let r = withdraw(&withdraw_calldata(42, 401), 100_000, 0).unwrap();
        assert!(r.bytes.iter().all(|&b| b == 0), "above-IM withdraw rejects");
        assert_eq!(
            accounts.lock().unwrap().get(&AccountId(42)).unwrap().collateral,
            Notional(500),
            "balance untouched on reject",
        );

        // Exactly to the IM line → succeeds. Post balance = IM_req = 100.
        let r = withdraw(&withdraw_calldata(42, 400), 100_000, 0).unwrap();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&r.bytes[24..32]);
        assert_eq!(i64::from_be_bytes(buf), 100);

        // One more → reject.
        let r = withdraw(&withdraw_calldata(42, 1), 100_000, 0).unwrap();
        assert!(r.bytes.iter().all(|&b| b == 0), "below-IM withdraw rejects");

        uninstall_accounts();
    }

    /// Stage 17l: `install_initial_margin_bps` flows into the
    /// `openhl_withdraw` precompile's free-collateral computation —
    /// same rate the bridge enforces, so on-chain and off-chain
    /// withdraw rules don't drift.
    #[test]
    fn withdraw_precompile_picks_up_installed_initial_margin_bps() {
        use openhl_clearing::DEFAULT_INITIAL_MARGIN_BPS;
        use openhl_funding::{MarkPrice, PositionSize};

        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        uninstall_clob();
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        // 500 bps (5%) instead of the default 1000 (10%).
        let prev = install_initial_margin_bps(500);

        // Open-position account: size=10, avg_entry=100, collateral=500.
        // IM at 500 bps = 10*100*500/10000 = 50. Free = 450.
        // (Stage 17g test with 1000 bps had free = 400.)
        accounts.lock().unwrap().insert(
            AccountId(42),
            Account {
                account: AccountId(42),
                position_size: PositionSize(10),
                avg_entry: MarkPrice(100),
                collateral: Notional(500),
            },
        );

        let r = withdraw(&withdraw_calldata(42, 451), 100_000, 0).unwrap();
        assert!(r.bytes.iter().all(|&b| b == 0), "above-IM withdraw rejects");

        let r = withdraw(&withdraw_calldata(42, 450), 100_000, 0).unwrap();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&r.bytes[24..32]);
        assert_eq!(i64::from_be_bytes(buf), 50);

        // Restore the default and verify the swap actually toggled.
        let after_test = install_initial_margin_bps(DEFAULT_INITIAL_MARGIN_BPS);
        assert_eq!(after_test, 500);
        assert_eq!(prev, DEFAULT_INITIAL_MARGIN_BPS);

        uninstall_accounts();
    }

    /// Stage 17c: a negative amount debits the balance.
    #[test]
    fn deposit_accepts_negative_amount_as_debit() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        install_accounts(Arc::clone(&accounts));

        let _ = deposit(&deposit_calldata(7, 1000), 100_000, 0).unwrap();
        let r = deposit(&deposit_calldata(7, -250), 100_000, 0).unwrap();

        let mut buf = [0u8; 8];
        buf.copy_from_slice(&r.bytes[24..32]);
        assert_eq!(i64::from_be_bytes(buf), 750);

        uninstall_accounts();
    }

    /// silently dropped — verified by the round-trip test above (which never
    /// installs a sink yet still observes book state changes).
    #[test]
    fn place_order_routes_fills_to_installed_sink() {
        let _g = TEST_SERIALIZER.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let book = Arc::new(Mutex::new(Book::new()));
        let sink: Arc<Mutex<Vec<Fill>>> = Arc::new(Mutex::new(Vec::new()));
        install_clob(book);
        install_fill_sink(Arc::clone(&sink));

        // Maker: Buy @ 100, qty 10. Rests, no fill.
        let maker = place_order_calldata(1, 0, 100, 10);
        let r = place_order(&maker, 100_000, 0).unwrap();
        assert!(U256::from_be_slice(&r.bytes[0..32]) > U256::ZERO);
        assert!(sink.lock().unwrap().is_empty(), "no fills after resting maker");

        // Taker: Sell @ 100, qty 10. Crosses the maker → exactly one fill.
        let taker = place_order_calldata(2, 1, 100, 10);
        let r = place_order(&taker, 100_000, 0).unwrap();
        assert!(U256::from_be_slice(&r.bytes[0..32]) > U256::ZERO);

        let fills = sink.lock().unwrap().clone();
        assert_eq!(fills.len(), 1, "exactly one fill from the crossing taker");
        assert_eq!(fills[0].price, Price(100));
        assert_eq!(fills[0].qty, Qty(10));

        uninstall_fill_sink();
        uninstall_clob();
    }
}
