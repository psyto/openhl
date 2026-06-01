// Stage 19a: bin-crate `unreachable_pub` triggers on every public item
// in this module since `openhl` has no library surface. The macro-
// generated `*Server` trait + RpcModule helpers need to stay public
// inside the crate, so silence the lint module-wide rather than
// scattering `pub(crate)` everywhere.
#![allow(unreachable_pub)]

//! `openhl_*` JSON-RPC namespace (Stage 19a).
//!
//! Reth's RPC server hosts the standard `eth_*` namespace; this
//! module bolts a small openhl-specific namespace alongside it via
//! Reth's `.extend_rpc_modules(...)` builder hook. The methods
//! expose what the bridge already computes — current CLOB mark,
//! per-account snapshots, margin health classifications, configured
//! `LiquidationParams` — so a frontend or trading client can talk
//! to the chain without re-implementing the bridge's accessors.
//!
//! ### Lifecycle quirk
//!
//! Reth's `extend_rpc_modules` hook runs BEFORE `.launch()` returns
//! the node handle, but the bridge can only be constructed AFTER
//! we have a `node.provider`. So the server holds an
//! `Arc<RwLock<Option<Arc<Bridge>>>>` cell that's filled by
//! `bin/openhl` right after bridge construction. Clients that hit
//! an `openhl_*` method during the tiny window between Reth
//! launching and the bridge being installed get a `BridgeNotReady`
//! error rather than a wrong answer.

use std::sync::{Arc, RwLock};

use jsonrpsee::{
    core::{RpcResult, SubscriptionResult},
    proc_macros::rpc,
    types::{error::ErrorCode, ErrorObject, ErrorObjectOwned},
    PendingSubscriptionSink, SubscriptionMessage,
};
use std::time::Duration;
use openhl_clob::AccountId;
use openhl_evm::LiveRethEvmBridge;
use openhl_liquidation::{LiquidationParams, MarginHealth};
use serde::{Deserialize, Serialize};

/// Process-shared cell holding the bridge once `bin/openhl` has
/// constructed it. `None` only during the narrow window between
/// Reth's `.launch()` returning and `bin/openhl` calling
/// [`install_bridge`].
pub type BridgeCell<P> = Arc<RwLock<Option<Arc<LiveRethEvmBridge<P>>>>>;

/// Construct an empty cell. Call before passing to `extend_rpc_modules`.
#[must_use]
pub fn new_bridge_cell<P>() -> BridgeCell<P> {
    Arc::new(RwLock::new(None))
}

/// Fill the cell once the bridge exists. Subsequent RPC calls
/// will resolve through it.
pub fn install_bridge<P>(cell: &BridgeCell<P>, bridge: Arc<LiveRethEvmBridge<P>>) {
    *cell.write().expect("bridge cell rwlock poisoned") = Some(bridge);
}

/// JSON shape for a single account, returned by
/// `openhl_accountSnapshot`. All fields are quote-currency units
/// or u64 IDs — no fixed-point scaling on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSnapshotRpc {
    /// Account ID.
    pub account: u64,
    /// Signed position size; positive = long, negative = short.
    pub position_size: i64,
    /// Volume-weighted average entry price. Undefined for flat
    /// accounts; carries the prior value as telemetry.
    pub avg_entry: u64,
    /// Quote-currency collateral balance. Signed because liquidation
    /// can drive it to deficits the insurance fund absorbs.
    pub collateral: i64,
}

/// JSON shape for `openhl_liquidationParams`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiquidationParamsRpc {
    /// Initial-margin rate in basis points.
    pub initial_margin_bps: u32,
    /// Maintenance-margin rate in basis points.
    pub maintenance_margin_bps: u32,
    /// Liquidation fee rate in basis points (used by the close-out
    /// math; not consumed by the withdraw rule).
    pub liquidation_fee_bps: u32,
}

impl From<&LiquidationParams> for LiquidationParamsRpc {
    fn from(p: &LiquidationParams) -> Self {
        Self {
            initial_margin_bps: p.initial_margin_bps,
            maintenance_margin_bps: p.maintenance_margin_bps,
            liquidation_fee_bps: p.liquidation_fee_bps,
        }
    }
}

/// Hyperliquid-shape info RPC. Registered at namespace `openhl`.
///
/// Method naming follows jsonrpsee's camelCase convention; the
/// wire methods come out as `openhl_currentMark`, `openhl_accounts`,
/// `openhl_accountSnapshot`, `openhl_marginHealth`,
/// `openhl_liquidationParams`.
#[rpc(server, namespace = "openhl")]
pub trait OpenHlInfoApi {
    /// Current CLOB midpoint, or `null` if either side of the book
    /// is empty.
    #[method(name = "currentMark")]
    fn current_mark(&self) -> RpcResult<Option<u64>>;

    /// Latest aggregated oracle index price (Stage 17o), or `null`
    /// before the first refresh has succeeded. This is the
    /// production-correct mark for margin / withdraw — the bridge
    /// prefers it over `currentMark` (the CLOB midpoint) when
    /// computing free collateral.
    #[method(name = "oracleIndexPrice")]
    fn oracle_index_price(&self) -> RpcResult<Option<u64>>;

    /// Mark actually consulted by `marginHealth` and the
    /// withdraw rule right now: `oracleIndexPrice` if any has
    /// been installed, otherwise `currentMark`. Returns `null`
    /// when neither source is available (no oracle refresh yet
    /// AND a one-sided/empty book).
    #[method(name = "effectiveMark")]
    fn effective_mark(&self) -> RpcResult<Option<u64>>;

    /// Account IDs the bridge has seen at least one fill or
    /// deposit for, sorted ascending.
    #[method(name = "accounts")]
    fn accounts(&self) -> RpcResult<Vec<u64>>;

    /// Snapshot of one account's perp state, or `null` if unknown.
    #[method(name = "accountSnapshot")]
    fn account_snapshot(&self, account: u64) -> RpcResult<Option<AccountSnapshotRpc>>;

    /// Margin health classification at the current mark, or `null`
    /// if indeterminate (unknown account, no CLOB midpoint). Values:
    /// `"Safe"`, `"AtRisk"`, `"Liquidatable"`, `"Underwater"`.
    #[method(name = "marginHealth")]
    fn margin_health(&self, account: u64) -> RpcResult<Option<String>>;

    /// Configured margin-model parameters (initial / maintenance /
    /// liquidation-fee bps) the bridge enforces today.
    #[method(name = "liquidationParams")]
    fn liquidation_params(&self) -> RpcResult<LiquidationParamsRpc>;

    /// Stage 19c — WebSocket subscription: emit the CLOB midpoint
    /// initially and on every change thereafter. Polled at 1s
    /// intervals on the server side; no event emitted when the
    /// value is unchanged from the prior tick (cheap to keep open
    /// long-term).
    #[subscription(name = "subscribeCurrentMark", item = Option<u64>)]
    fn subscribe_current_mark(&self) -> SubscriptionResult;

    /// WebSocket subscription: emit the effective mark (oracle
    /// index if installed, else CLOB midpoint) initially and on
    /// change. Use this when you want to know what the bridge is
    /// ACTUALLY using for margin / withdraw right now.
    #[subscription(name = "subscribeEffectiveMark", item = Option<u64>)]
    fn subscribe_effective_mark(&self) -> SubscriptionResult;

    /// WebSocket subscription: emit an account's margin health
    /// classification on every change. `null` until the bridge
    /// has enough information to classify (no mark / unknown
    /// account); a subscriber that joins before the first oracle
    /// refresh and the account exists sees null first, then a
    /// concrete classification once the mark is available.
    #[subscription(name = "subscribeMarginHealth", item = Option<String>)]
    fn subscribe_margin_health(&self, account: u64) -> SubscriptionResult;
}

/// Implementation backed by an [`BridgeCell`]. Constructed once
/// per node boot in `bin/openhl::run_reth_devnet` and merged into
/// Reth's RPC modules via `extend_rpc_modules`.
pub struct OpenHlInfoServer<P> {
    bridge: BridgeCell<P>,
}

impl<P> OpenHlInfoServer<P> {
    #[must_use]
    pub const fn new(bridge: BridgeCell<P>) -> Self {
        Self { bridge }
    }

    /// Resolve the cell into the live bridge. Returns a "bridge
    /// not ready yet" RPC error if [`install_bridge`] hasn't fired
    /// — the only path that ever yields `None`.
    fn bridge_or_err(&self) -> RpcResult<Arc<LiveRethEvmBridge<P>>> {
        let guard = self
            .bridge
            .read()
            .map_err(|_| internal_error("openhl rpc bridge lock poisoned"))?;
        guard.as_ref().cloned().ok_or_else(|| {
            ErrorObject::owned::<()>(
                ErrorCode::ServerError(-32_010).code(),
                "openhl bridge not yet installed (try again in a moment)",
                None,
            )
        })
    }
}

fn internal_error(msg: &'static str) -> ErrorObjectOwned {
    ErrorObject::owned::<()>(ErrorCode::InternalError.code(), msg, None)
}

fn margin_health_str(h: MarginHealth) -> &'static str {
    match h {
        MarginHealth::Safe => "Safe",
        MarginHealth::AtRisk => "AtRisk",
        MarginHealth::Liquidatable => "Liquidatable",
        MarginHealth::Underwater => "Underwater",
    }
}

impl<P> OpenHlInfoApiServer for OpenHlInfoServer<P>
where
    P: Send + Sync + 'static,
{
    fn current_mark(&self) -> RpcResult<Option<u64>> {
        Ok(self.bridge_or_err()?.current_mark().map(|m| m.0))
    }

    fn oracle_index_price(&self) -> RpcResult<Option<u64>> {
        Ok(self.bridge_or_err()?.oracle_index_price())
    }

    fn effective_mark(&self) -> RpcResult<Option<u64>> {
        Ok(self.bridge_or_err()?.effective_mark().map(|m| m.0))
    }

    fn accounts(&self) -> RpcResult<Vec<u64>> {
        Ok(self
            .bridge_or_err()?
            .accounts_snapshot()
            .iter()
            .map(|a| a.account.0)
            .collect())
    }

    fn account_snapshot(&self, account: u64) -> RpcResult<Option<AccountSnapshotRpc>> {
        Ok(self
            .bridge_or_err()?
            .accounts_snapshot()
            .into_iter()
            .find(|a| a.account.0 == account)
            .map(|a| AccountSnapshotRpc {
                account: a.account.0,
                position_size: a.position_size.0,
                avg_entry: a.avg_entry.0,
                collateral: a.collateral.0,
            }))
    }

    fn margin_health(&self, account: u64) -> RpcResult<Option<String>> {
        Ok(self
            .bridge_or_err()?
            .margin_health(AccountId(account))
            .map(|h| margin_health_str(h).to_string()))
    }

    fn liquidation_params(&self) -> RpcResult<LiquidationParamsRpc> {
        Ok(LiquidationParamsRpc::from(
            self.bridge_or_err()?.liquidation_params(),
        ))
    }

    fn subscribe_current_mark(&self, pending: PendingSubscriptionSink) -> SubscriptionResult {
        let bridge = self.bridge.clone();
        spawn_polling_subscription(pending, move || {
            read_bridge(&bridge).map(|b| b.current_mark().map(|m| m.0))
        });
        Ok(())
    }

    fn subscribe_effective_mark(&self, pending: PendingSubscriptionSink) -> SubscriptionResult {
        let bridge = self.bridge.clone();
        spawn_polling_subscription(pending, move || {
            read_bridge(&bridge).map(|b| b.effective_mark().map(|m| m.0))
        });
        Ok(())
    }

    fn subscribe_margin_health(
        &self,
        pending: PendingSubscriptionSink,
        account: u64,
    ) -> SubscriptionResult {
        let bridge = self.bridge.clone();
        spawn_polling_subscription(pending, move || {
            read_bridge(&bridge).map(|b| {
                b.margin_health(AccountId(account))
                    .map(|h| margin_health_str(h).to_string())
            })
        });
        Ok(())
    }
}

/// Poll the bridge cell; `None` if the bridge isn't installed yet
/// (the narrow window between Reth launch and bin/openhl filling
/// the cell). Subscriptions skip emission until it's filled.
fn read_bridge<P>(cell: &BridgeCell<P>) -> Option<Arc<LiveRethEvmBridge<P>>> {
    cell.read().ok()?.clone()
}

/// Stage 19c — generic polling subscription. Spawns a tokio task
/// that:
///   1. Accepts the pending subscription.
///   2. Polls `poll_fn` every 1s.
///   3. Emits the value when it differs from the last emitted
///      one (or on the first iteration). Unchanged values are
///      silently dropped — the subscription stays cheap.
///   4. Exits when the subscriber disconnects or the bridge cell
///      poll returns `None` (the bridge isn't installed yet — the
///      caller is expected to retry; we don't keep a zombie task).
fn spawn_polling_subscription<T, F>(pending: PendingSubscriptionSink, mut poll_fn: F)
where
    T: serde::Serialize + PartialEq + Clone + Send + 'static,
    F: FnMut() -> Option<T> + Send + 'static,
{
    tokio::spawn(async move {
        let sink = match pending.accept().await {
            Ok(s) => s,
            Err(_) => return,
        };
        let interval = Duration::from_secs(1);
        let mut last: Option<T> = None;
        let mut first = true;
        loop {
            // Skip if the bridge cell isn't filled — we don't have
            // an answer to emit yet. Try again next interval.
            if let Some(now) = poll_fn() {
                if first || last.as_ref() != Some(&now) {
                    let raw = match serde_json::value::to_raw_value(&now) {
                        Ok(r) => r,
                        Err(_) => return,
                    };
                    if sink.send(SubscriptionMessage::from(raw)).await.is_err() {
                        return;
                    }
                    last = Some(now);
                    first = false;
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}
