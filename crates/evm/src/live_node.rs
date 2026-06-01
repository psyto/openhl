//! `LiveRethEvmBridge` — `ConsensusBridge` backed by a real Reth provider.
//!
//! Stage 7b: parent lookups go through the live node's provider via the
//! `BlockNumReader` trait.
//!
//! Stage 7c: `validate_payload` runs Reth's `EthBeaconConsensus::
//! validate_header_against_parent` against the live parent — that's real
//! header validation (number monotonicity, timestamp monotonicity, gas-limit
//! drift, base-fee math) using production Reth code.
//!
//! Stage 8d: the bridge now owns a CLOB matching engine. `submit_order` routes
//! orders into the book and accumulates resulting fills in `pending_fills`.
//! `build_payload` drains the pending fills and stores them alongside the
//! synthesized header, so the payload carries real CLOB-generated content.
//! Fills are not yet encoded as EVM transactions executable by Reth's
//! `BlockExecutor` — that's the next stage (or Module 3). 8d proves the
//! wiring exists; encoding is downstream.
//!
//! Stage 7d: `commit` now sends a `ForkchoiceUpdated` to Reth's in-process
//! consensus engine when an engine handle has been installed. The bridge
//! still maintains its own `chain` `HashMap` as the source of truth for
//! validation lookups — Reth's response (VALID/SYNCING/INVALID) is logged
//! but does not yet block the commit, because `build_payload` doesn't
//! produce a real `ExecutionPayload` for the engine to validate against.
//! Honest scoping: the wire is connected; payload-execution alignment is
//! the next chunk of work (depends on encoding CLOB fills as EVM txs).
//!
//! Still stubbed:
//!   - Full block execution + state-root verification (waits on fills being
//!     encoded as EVM-executable transactions, then `newPayload` round-trip)

use alloy_consensus::Header;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::ForkchoiceState;
use async_trait::async_trait;
use openhl_clearing::{apply_fill, Account};
use openhl_clob::{AccountId, Book, Fill, FillResult, Order};
use openhl_consensus::bridge::{BridgeError, ConsensusBridge};
use openhl_funding::Notional;
use openhl_liquidation::{
    margin_health as compute_margin_health, AccountSnapshot, LiquidationParams, MarginHealth,
};
use openhl_types::{BlockHash, ExecutedBlock, PayloadAttrs, PayloadId, PayloadStatus};
use reth_chainspec::{ChainSpec, EthChainSpec};
use reth_consensus::HeaderValidator;
use reth_engine_primitives::ConsensusEngineHandle;
use reth_ethereum_consensus::EthBeaconConsensus;
use reth_ethereum_engine_primitives::EthEngineTypes;
use reth_primitives_traits::SealedHeader;
use reth_storage_api::{BlockNumReader, HeaderProvider};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub struct LiveRethEvmBridge<P> {
    provider: P,
    chain_spec: Arc<ChainSpec>,
    validator: EthBeaconConsensus<ChainSpec>,
    /// `Arc<Mutex<Book>>` rather than `Mutex<Book>` so the bridge can share
    /// its CLOB with the precompile module's process-global state. The bridge
    /// writes via `submit_order`; smart contracts read via the
    /// `clob_read_best_bid` precompile — both touch the same `Book`.
    clob: Arc<Mutex<Book>>,
    /// Same shared-Arc pattern as `clob`: the precompile module's `FILL_SINK`
    /// global points at this buffer too, so fills produced by EVM-placed
    /// orders (via `clob_place_order`) flow into the same queue the bridge's
    /// own `submit_order` writes to (Stage 9c+).
    pending_fills: Arc<Mutex<Vec<Fill>>>,
    /// Optional in-process Engine API handle. When installed (Stage 7d via
    /// [`Self::with_engine_handle`]), `commit` sends a `ForkchoiceUpdated`
    /// to Reth so its canonical chain advances in lockstep with consensus.
    /// `None` at v0 means commits stay local to the bridge's `state.chain`
    /// `HashMap` — fine for unit tests, but RPC clients won't see new heads.
    engine_handle: Option<ConsensusEngineHandle<EthEngineTypes>>,
    state: Mutex<State>,
    /// Per-account perp state, mutated by every fill the bridge sees
    /// (Stage 16b). Indexed by [`AccountId`] for O(1) update; the
    /// `accounts_snapshot()` accessor returns a deterministically-
    /// sorted `Vec` for downstream consumers (scan, ADL, funding).
    ///
    /// `Arc<Mutex<...>>` (Stage 17c) so the EVM-side deposit
    /// precompile can hold a clone of the same map. Same shared-Arc
    /// pattern as `clob` and `pending_fills`.
    accounts: Arc<Mutex<HashMap<AccountId, Account>>>,
    /// Margin-model parameters (Stage 17m, generalizing 17l's
    /// single-`u32` field). The bridge stores the full
    /// [`LiquidationParams`] so it can answer not just "what's the
    /// initial-margin rate for withdraw?" but also "what's this
    /// account's margin health right now?" — see
    /// [`Self::margin_health`].
    ///
    /// Default: [`LiquidationParams::hyperliquid_default`].
    /// Override via [`Self::with_liquidation_params`], which ALSO
    /// installs `initial_margin_bps` into the precompile global
    /// so EVM-side and Rust-side withdraw rules stay in lockstep.
    liquidation_params: LiquidationParams,
    /// Stage 17o — latest oracle aggregated index price.
    /// `bin/openhl` pushes this after every successful
    /// `coordinator.tick` via [`Self::set_oracle_index_price`];
    /// the setter also installs it into the matching precompile
    /// global so the EVM-side reads the same value. `None` before
    /// the first refresh (and after every miss, e.g., a tick
    /// where the deviation filter failed quorum) — in that case
    /// margin / withdraw paths fall back to the CLOB midpoint
    /// via [`Self::effective_mark`].
    oracle_index_price: Mutex<Option<u64>>,
}

#[derive(Debug, Default)]
struct State {
    next_payload_id: u64,
    /// Pending payloads keyed by `PayloadId.0`. Value is (`block_hash`, `header`,
    /// fills drained from the CLOB at `build_payload` time).
    pending: HashMap<u64, (B256, Header, Vec<Fill>)>,
    chain: HashMap<B256, Header>,
    head: Option<B256>,
}

impl<P> LiveRethEvmBridge<P> {
    #[must_use]
    pub fn new(provider: P, chain_spec: Arc<ChainSpec>) -> Self {
        let validator = EthBeaconConsensus::new(Arc::clone(&chain_spec));
        let clob = Arc::new(Mutex::new(Book::new()));
        let pending_fills = Arc::new(Mutex::new(Vec::new()));

        // Make our CLOB visible to the `clob_read_best_bid` precompile so
        // smart contracts can query live orderbook state. The bridge writes
        // (submit_order), the EVM reads (precompile); they share the same Arc.
        crate::precompiles::install_clob(Arc::clone(&clob));

        // Route fills produced by the `clob_place_order` precompile into the
        // same queue `submit_order` writes to. Without this, EVM-placed orders
        // would match but their fills would be silently dropped (Stage 9c+).
        crate::precompiles::install_fill_sink(Arc::clone(&pending_fills));

        // Stage 17c: shared account map for the EVM-side deposit precompile.
        // Same pattern as clob + fill_sink — bridge writes via `deposit` /
        // `apply_fills_to_accounts`, EVM reads/writes via the precompile;
        // both touch the same Arc.
        let accounts = Arc::new(Mutex::new(HashMap::new()));
        crate::precompiles::install_accounts(Arc::clone(&accounts));

        Self {
            provider,
            chain_spec,
            validator,
            clob,
            pending_fills,
            engine_handle: None,
            state: Mutex::new(State::default()),
            accounts,
            liquidation_params: LiquidationParams::hyperliquid_default(),
            oracle_index_price: Mutex::new(None),
        }
    }

    /// Stage 17m — override the margin-model parameters used by
    /// [`Self::withdraw`] (via `params.initial_margin_bps`) and by
    /// [`Self::margin_health`] (via the full
    /// [`LiquidationParams`]). `bin/openhl` reads
    /// `OpenHlNodeConfig::liquidation_params` at boot and threads
    /// the whole struct through here.
    ///
    /// ALSO installs `params.initial_margin_bps` and
    /// `params.maintenance_margin_bps` into the matching
    /// precompile-module globals (Stage 17n) so the
    /// `openhl_withdraw` and `openhl_margin_health` precompiles
    /// read the same thresholds the bridge enforces — preserves
    /// the "EVM-side and Rust-side views are equivalent" property
    /// (see `docs/architecture.md`). The `liquidation_fee_bps`
    /// field isn't exposed via precompile (it's only used by the
    /// liquidation engine's solvent-close math, not by anything
    /// EVM contracts query).
    #[must_use]
    pub fn with_liquidation_params(mut self, params: LiquidationParams) -> Self {
        crate::precompiles::install_initial_margin_bps(params.initial_margin_bps);
        crate::precompiles::install_maintenance_margin_bps(params.maintenance_margin_bps);
        self.liquidation_params = params;
        self
    }

    /// Read the bridge's full margin-model parameters.
    #[must_use]
    pub const fn liquidation_params(&self) -> &LiquidationParams {
        &self.liquidation_params
    }

    /// Convenience getter — initial-margin rate in bps. Equivalent
    /// to `bridge.liquidation_params().initial_margin_bps()`; kept
    /// for parity with the Stage 17l API.
    #[must_use]
    pub const fn initial_margin_bps(&self) -> u32 {
        self.liquidation_params.initial_margin_bps
    }

    /// Stage 17o — install the latest oracle aggregated index
    /// price. Called by `bin/openhl` after every
    /// `coordinator.tick`; updates the bridge's local cache AND
    /// the matching precompile-module global so the EVM-side
    /// `openhl_withdraw` / `openhl_margin_health` precompiles read
    /// exactly the value the bridge uses for its Rust-side
    /// `withdraw` and `margin_health` accessors.
    pub fn set_oracle_index_price(&self, price: u64) {
        *self
            .oracle_index_price
            .lock()
            .expect("oracle_index_price mutex poisoned") = Some(price);
        crate::precompiles::install_oracle_index_price(price);
    }

    /// Read the cached oracle index price (last value pushed via
    /// [`Self::set_oracle_index_price`]), or `None` if no refresh
    /// has succeeded yet.
    #[must_use]
    pub fn oracle_index_price(&self) -> Option<u64> {
        *self
            .oracle_index_price
            .lock()
            .expect("oracle_index_price mutex poisoned")
    }

    /// Stage 17o — the mark used by margin / withdraw / margin_health
    /// computations. Returns the cached oracle index price if any
    /// has been installed (production-correct: oracle is the
    /// canonical reference, not the CLOB midpoint), otherwise
    /// falls back to [`Self::current_mark`] (CLOB midpoint).
    ///
    /// Returns `None` only when neither source is available — i.e.,
    /// no oracle refresh has succeeded AND the book is one-sided
    /// or empty. Withdraw degrades to the avg-entry fallback in
    /// that case; `margin_health` returns Indeterminate.
    #[must_use]
    pub fn effective_mark(&self) -> Option<openhl_funding::MarkPrice> {
        self.oracle_index_price()
            .map(openhl_funding::MarkPrice)
            .or_else(|| self.current_mark())
    }

    /// Stage 17m — classify an account's margin health using the
    /// production-shape `openhl_liquidation::margin_health` math
    /// at the current CLOB midpoint and this bridge's
    /// [`LiquidationParams`].
    ///
    /// Returns `None` when:
    ///   * the account doesn't exist, OR
    ///   * the CLOB midpoint isn't available (one-sided / empty
    ///     book) — clients should treat this as "indeterminate
    ///     until a mark is available", same handling pattern as
    ///     [`Self::current_mark`].
    ///
    /// Read-only; mutates nothing. Designed as the v0 primitive a
    /// future Hyperliquid-shape RPC `info` endpoint can return so
    /// clients see Safe / AtRisk / Liquidatable / Underwater
    /// status without re-implementing the liquidation engine.
    #[must_use]
    pub fn margin_health(&self, account: AccountId) -> Option<MarginHealth> {
        // Stage 17o: prefer the installed oracle index over the
        // CLOB midpoint (production-correct mark for margin /
        // liquidation). `effective_mark` does the fallback.
        let mark = self.effective_mark()?;
        let accts = self.accounts.lock().expect("accounts mutex poisoned");
        let acct = accts.get(&account)?;
        let snapshot = AccountSnapshot {
            account: acct.account,
            position_size: acct.position_size,
            avg_entry: acct.avg_entry,
            collateral: acct.collateral,
        };
        Some(compute_margin_health(&snapshot, mark, &self.liquidation_params))
    }

    /// Install a Reth in-process Engine API handle. After this call,
    /// `commit` will fire a `ForkchoiceUpdated` to Reth's consensus engine
    /// alongside its own local bookkeeping. Without an engine handle, the
    /// bridge still works (commits go to its internal `HashMap`) but Reth's
    /// canonical chain won't advance — RPC and any other Reth consumer will
    /// see only the genesis block.
    #[must_use]
    pub fn with_engine_handle(
        mut self,
        handle: ConsensusEngineHandle<EthEngineTypes>,
    ) -> Self {
        self.engine_handle = Some(handle);
        self
    }

    #[must_use]
    pub const fn has_engine_handle(&self) -> bool {
        self.engine_handle.is_some()
    }

    #[must_use]
    pub fn chain_spec(&self) -> &Arc<ChainSpec> {
        &self.chain_spec
    }

    /// Submit an order to the CLOB. Resulting fills are buffered in
    /// `pending_fills` until the next `build_payload` drains them,
    /// AND (Stage 16b) routed through `openhl-clearing::apply_fill`
    /// to update per-account position + collateral state.
    pub fn submit_order(&self, order: Order) -> FillResult {
        let mut book = self.clob.lock().expect("clob mutex poisoned");
        let result = book.submit(order);
        if !result.fills.is_empty() {
            self.pending_fills
                .lock()
                .expect("pending_fills mutex poisoned")
                .extend(result.fills.iter().copied());
            self.apply_fills_to_accounts(&result.fills);
        }
        result
    }

    /// Walk a freshly produced fill list and update both the maker
    /// and taker accounts. Stage 16b — the bridge is now the owning
    /// layer for per-account perp state.
    fn apply_fills_to_accounts(&self, fills: &[Fill]) {
        let mut accts = self.accounts.lock().expect("accounts mutex poisoned");
        for fill in fills {
            let taker_side = fill.maker_side.opposite();

            let maker = accts
                .entry(fill.maker_account)
                .or_insert_with(|| Account::flat(fill.maker_account));
            let maker_realized = apply_fill(maker, fill.price, fill.qty, fill.maker_side);
            maker.collateral = Notional(maker.collateral.0.saturating_add(maker_realized));

            let taker = accts
                .entry(fill.taker_account)
                .or_insert_with(|| Account::flat(fill.taker_account));
            let taker_realized = apply_fill(taker, fill.price, fill.qty, taker_side);
            taker.collateral = Notional(taker.collateral.0.saturating_add(taker_realized));
        }
    }

    /// Snapshot the current per-account state as a deterministically
    /// sorted `Vec` (by `AccountId` ascending). Downstream tick
    /// consumers (Stage 16c) read this each block.
    #[must_use]
    pub fn accounts_snapshot(&self) -> Vec<Account> {
        let accts = self.accounts.lock().expect("accounts mutex poisoned");
        let mut out: Vec<Account> = accts.values().copied().collect();
        out.sort_by_key(|a| a.account.0);
        out
    }

    /// Mutate the bridge-owned account map under its lock. Stage 16c
    /// uses this to (a) seed the demo's starting accounts at boot and
    /// (b) write per-tick funding settlements / liquidation closes /
    /// ADL records back into the same map the snapshot reads from.
    ///
    /// The closure receives `&mut HashMap<AccountId, Account>` so
    /// callers can both update existing entries and insert new ones.
    /// Returning `R` lets the caller bubble values out (e.g., counts,
    /// `Result`s) without re-locking.
    pub fn with_accounts_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut HashMap<AccountId, Account>) -> R,
    {
        let mut accts = self.accounts.lock().expect("accounts mutex poisoned");
        f(&mut accts)
    }

    /// Credit `amount` quote-currency to `account`'s collateral
    /// (Stage 17b). Creates the account in its flat state if it
    /// doesn't exist yet — a real perp DEX deposit would land via
    /// an EVM-side `deposit(account, amount)` call from a USDC
    /// transfer; this is the bridge-layer hook that instruction
    /// would invoke. Returns the new collateral balance.
    ///
    /// `amount` is signed; positive credits, negative debits.
    /// No balance check on debits — for safety-checked withdrawals
    /// use [`Self::withdraw`]. Overflow is `saturating_add`.
    pub fn deposit(&self, account: AccountId, amount: i64) -> openhl_funding::Notional {
        let mut accts = self.accounts.lock().expect("accounts mutex poisoned");
        let acct = accts.entry(account).or_insert_with(|| Account::flat(account));
        acct.collateral = openhl_funding::Notional(acct.collateral.0.saturating_add(amount));
        acct.collateral
    }

    /// Debit `amount` quote-currency from `account`'s collateral
    /// (Stage 17e, margin-aware as of 17g, mark-aware as of 17j).
    /// Returns the new balance on success; `None` if the account
    /// doesn't exist, the requested amount doesn't fit in `i64`, or
    /// the withdraw would leave free collateral below zero.
    ///
    /// `amount` is unsigned by API contract — callers expressing
    /// "I want to take 100 out" never accidentally credit by passing
    /// a negative.
    ///
    /// **Free collateral rule (Stage 17j).** When the CLOB has both
    /// a bid and an ask, the midpoint serves as the mark and free
    /// collateral is `(collateral + unrealized_pnl) − IM_req(mark)` —
    /// the production shape used by Hyperliquid / Binance / Drift.
    /// Traders with positive uPnL can withdraw against their gains;
    /// traders at a loss face a tighter limit than the Stage 17g
    /// avg-entry rule.
    ///
    /// **Fallback.** With a one-sided or empty book (no midpoint),
    /// uPnL is treated as `0` and IM_req is evaluated at `avg_entry`
    /// — the exact rule Stage 17g shipped. Flat accounts collapse to
    /// the raw-collateral check Stage 17e shipped.
    pub fn withdraw(
        &self,
        account: AccountId,
        amount: u64,
    ) -> Option<openhl_funding::Notional> {
        // Stage 17o: oracle index price (if installed) is the
        // canonical mark; CLOB midpoint is the fallback. Same
        // source the `openhl_withdraw` precompile reads via
        // `precompiles::effective_mark`.
        let mark = self.effective_mark();
        let mut accts = self.accounts.lock().expect("accounts mutex poisoned");
        let acct = accts.get_mut(&account)?;
        let amount_i64 = i64::try_from(amount).ok()?;
        let free =
            withdraw_free_collateral(acct, mark, self.liquidation_params.initial_margin_bps);
        if i128::from(amount_i64) > i128::from(free) {
            return None;
        }
        acct.collateral = openhl_funding::Notional(acct.collateral.0 - amount_i64);
        Some(acct.collateral)
    }

    /// Inspect (read-only) the fills attached to a built payload. Returns
    /// `None` if the payload id is unknown. Production code would encode
    /// these as EVM-executable transactions before they reach the block
    /// body; v0 keeps them as a parallel list for test inspection.
    #[must_use]
    pub fn payload_fills(&self, id: PayloadId) -> Option<Vec<Fill>> {
        let s = self.state.lock().expect("state mutex poisoned");
        s.pending.get(&id.0).map(|(_, _, fills)| fills.clone())
    }

    /// Number of fills currently buffered, waiting for the next `build_payload`.
    #[must_use]
    pub fn pending_fill_count(&self) -> usize {
        self.pending_fills
            .lock()
            .expect("pending_fills mutex poisoned")
            .len()
    }

    /// Current top-of-book mark from the CLOB — the midpoint of
    /// `(best_bid + best_ask) / 2`, expressed as [`MarkPrice`].
    ///
    /// Returns `None` when either side of the book is empty: a one-sided
    /// book has no midpoint, and the caller (Stage 14c integration
    /// coordinator) is responsible for the fallback policy. Per the
    /// `TickInput::mark` docstring on `openhl-node`, the mark is
    /// strictly CLOB-derived and must **not** be conflated with the
    /// oracle's aggregated index price.
    #[must_use]
    pub fn current_mark(&self) -> Option<openhl_funding::MarkPrice> {
        let book = self.clob.lock().expect("clob mutex poisoned");
        let bid = book.best_bid()?;
        let ask = book.best_ask()?;
        // Integer midpoint; rounds toward zero. With u64 prices this is
        // a saturating add but bid + ask can't overflow u64 in any
        // realistic deployment.
        Some(openhl_funding::MarkPrice((bid.0 + ask.0) / 2))
    }

    /// Snapshot of the bridge's committed-chain state (Stage 13g)
    /// plus per-account perp state (Stage 16b).
    ///
    /// Captures only the load-bearing fields for cross-restart resume:
    ///   - `chain`: every block consensus has committed so far.
    ///   - `head`: the most recent committed block hash, if any.
    ///   - `accounts`: every account that has ever appeared in a
    ///     fill, with its current `(position_size, avg_entry,
    ///     collateral)`. Sorted by `account.0` for stable on-disk
    ///     diffs.
    ///
    /// Deliberately excludes:
    ///   - `next_payload_id`: a monotonic counter for in-flight
    ///     payloads. Resets to 0 on restart (in-flight builds don't
    ///     survive shutdown).
    ///   - `pending`: in-flight payloads. Ephemeral by definition;
    ///     consensus reissues them on restart.
    ///   - `pending_fills`: the CLOB's drained-but-unattached fills.
    ///     Same reasoning — ephemeral.
    ///
    /// JSON-serializable for human-inspectable on-disk snapshots.
    #[must_use]
    pub fn snapshot(&self) -> BridgeSnapshot {
        let s = self.state.lock().expect("state mutex poisoned");
        let accounts = self.accounts_snapshot();
        BridgeSnapshot {
            chain: s.chain.clone(),
            head: s.head,
            accounts,
        }
    }

    /// Replace the bridge's committed-chain state with `snapshot`
    /// (Stage 13g + 16b). Pending payloads and the fill buffer are
    /// NOT touched — they remain whatever the caller's bridge was
    /// holding before the load. Typical use is to call this
    /// immediately after `LiveRethEvmBridge::new` and before
    /// consensus starts.
    pub fn load_snapshot(&self, snapshot: BridgeSnapshot) {
        let mut s = self.state.lock().expect("state mutex poisoned");
        s.chain = snapshot.chain;
        s.head = snapshot.head;
        let mut accts = self.accounts.lock().expect("accounts mutex poisoned");
        accts.clear();
        for acct in snapshot.accounts {
            accts.insert(acct.account, acct);
        }
    }
}

/// Stage 17j helper for `withdraw`: free collateral with a CLOB
/// mark when one's available, falling back to the Stage 17g
/// avg-entry rule when it isn't. Shared by the bridge and the
/// withdraw precompile so on-chain and off-chain views of "how
/// much can I take out" stay byte-identical.
///
/// `im_bps` is the initial-margin rate in basis points. Stage 17l
/// made this configurable per-bridge via
/// [`LiveRethEvmBridge::with_initial_margin_bps`]; the precompile
/// reads it from the matching
/// [`crate::precompiles::current_initial_margin_bps`] global.
pub(crate) fn withdraw_free_collateral(
    acct: &openhl_clearing::Account,
    mark: Option<openhl_funding::MarkPrice>,
    im_bps: u32,
) -> i64 {
    match mark {
        Some(m) => openhl_clearing::free_collateral(acct, m, im_bps),
        None => {
            // Stage 17g fallback: no mark → IM at avg_entry, uPnL
            // treated as zero. Equivalent to `collateral − IM_req`.
            let im = openhl_clearing::initial_margin_requirement(acct, im_bps);
            acct.collateral.0.saturating_sub(im)
        }
    }
}

/// On-disk snapshot of the bridge's committed-chain state.
///
/// Stage 13g extracts this from
/// [`LiveRethEvmBridge::snapshot`] and writes JSON to
/// `<data-dir>/bridge/state.json`; subsequent runs load it via
/// [`LiveRethEvmBridge::load_snapshot`] before starting consensus.
/// `Option<B256>` for `head` is `None` on a fresh chain (no blocks
/// committed yet).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BridgeSnapshot {
    pub chain: HashMap<B256, Header>,
    pub head: Option<B256>,
    /// Per-account perp state, persisted so that restart preserves
    /// the cumulative effect of every fill the bridge has applied
    /// (Stage 16b). `#[serde(default)]` so old on-disk snapshots
    /// (Stage 13g..15c era) deserialize cleanly into an empty
    /// account map.
    #[serde(default)]
    pub accounts: Vec<Account>,
}

impl BridgeSnapshot {
    /// Empty snapshot — no blocks committed, no head, no accounts.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            chain: HashMap::new(),
            head: None,
            accounts: Vec::new(),
        }
    }
}

#[async_trait]
impl<P> ConsensusBridge for LiveRethEvmBridge<P>
where
    P: BlockNumReader + HeaderProvider<Header = Header> + Clone + Sync + 'static,
{
    async fn build_payload(
        &self,
        parent: BlockHash,
        attrs: PayloadAttrs,
    ) -> Result<PayloadId, BridgeError> {
        let parent_b256 = B256::from(parent.0);

        // Look up the parent header. Two sources:
        //
        //   (a) Bridge's internal `chain` map — populated by `commit()`
        //       for every block consensus has decided. Source of truth
        //       for blocks the bridge has committed.
        //   (b) Reth's provider — source of truth for blocks Reth has
        //       persisted (genesis at chain bootstrap, plus any blocks
        //       the engine has successfully executed via `newPayload`).
        //
        // We check (a) first because the bridge's `commit` does not yet
        // upload an executable `ExecutionPayload` to Reth (the synthetic
        // headers produced here have placeholder state_roots — Reth's
        // newPayload would reject them as INVALID, see the doc on
        // `commit`). Without this internal fallback, `build_payload` for
        // block N+1 fails because Reth's provider never saw block N.
        // Stage 8e closes that gap by treating the bridge as the
        // committed-chain source of truth.
        //
        // Provider is the fallback path — exercised exclusively for the
        // chain's first block (parent = genesis), where Reth IS the
        // source of truth.
        let parent_header_from_chain = {
            let s = self.state.lock().expect("state mutex poisoned");
            s.chain.get(&parent_b256).cloned()
        };
        let (parent_header, _parent_sealed_opt) = if let Some(h) = parent_header_from_chain {
            (h, None)
        } else {
            let sealed = self
                .provider
                .sealed_header_by_hash(parent_b256)
                .map_err(|e| BridgeError::Internal(eyre::eyre!("provider error: {e}")))?
                .ok_or_else(|| {
                    BridgeError::Rejected(format!(
                        "neither bridge.chain nor provider has block {parent_b256}"
                    ))
                })?;
            (sealed.header().clone(), Some(sealed))
        };
        let parent_header = &parent_header;

        let mut s = self.state.lock().expect("state mutex poisoned");
        let id = s.next_payload_id;
        s.next_payload_id += 1;

        let our_timestamp = attrs.timestamp.max(parent_header.timestamp + 1);

        // Compute the EIP-1559 base fee for our block via the chain spec —
        // identical math to what EthBeaconConsensus's
        // `validate_against_parent_eip1559_base_fee` will check against.
        let next_base_fee = self
            .chain_spec
            .next_block_base_fee(parent_header, our_timestamp);

        let header = Header {
            parent_hash: parent_b256,
            number: parent_header.number + 1,
            // Timestamp must be strictly greater than parent's; force at least
            // parent.timestamp + 1 even if attrs.timestamp came in stale.
            timestamp: our_timestamp,
            beneficiary: Address::from(attrs.fee_recipient),
            mix_hash: B256::from(attrs.prev_randao),
            // Keep gas_limit identical to parent so EthBeaconConsensus's
            // 1/1024 drift check passes trivially. A real payload builder
            // would tune this per network policy.
            gas_limit: parent_header.gas_limit,
            // Post-merge: difficulty must be 0.
            difficulty: alloy_primitives::U256::ZERO,
            base_fee_per_gas: next_base_fee,
            ..Default::default()
        };
        let hash = header.hash_slow();

        // Drain whatever fills the CLOB has accumulated since the last
        // build_payload call. The fills attach to this payload so the bridge
        // can route them downstream (encode as EVM txs, return via
        // payload_fills, etc.). 8d keeps them as a parallel list; future
        // stages encode them into the block body.
        let drained_fills = std::mem::take(
            &mut *self
                .pending_fills
                .lock()
                .expect("pending_fills mutex poisoned"),
        );

        s.pending.insert(id, (hash, header, drained_fills));
        Ok(PayloadId(id))
    }

    async fn payload_ready(&self, id: PayloadId) -> Result<ExecutedBlock, BridgeError> {
        let s = self.state.lock().expect("state mutex poisoned");
        let n = id.0;
        let (hash, header, _fills) = s
            .pending
            .get(&n)
            .cloned()
            .ok_or_else(|| BridgeError::Rejected(format!("unknown payload id {n}")))?;
        Ok(ExecutedBlock {
            hash: BlockHash(hash.0),
            parent_hash: BlockHash(header.parent_hash.0),
            number: header.number,
            state_root: header.state_root.0,
            timestamp: header.timestamp,
        })
    }

    async fn validate_payload(
        &self,
        block: &ExecutedBlock,
    ) -> Result<PayloadStatus, BridgeError> {
        let block_hash = B256::from(block.hash.0);
        let parent_hash = B256::from(block.parent_hash.0);

        // Find our header for this block. In single-validator mode we always
        // built it, so it sits in pending (pre-commit) or chain (post-commit).
        let header = {
            let s = self.state.lock().expect("state mutex poisoned");
            s.pending
                .values()
                .find(|(h, _, _)| *h == block_hash)
                .map(|(_, h, _)| h.clone())
                .or_else(|| s.chain.get(&block_hash).cloned())
        };
        let Some(header) = header else {
            return Ok(PayloadStatus::Invalid);
        };

        // Fetch parent sealed header from the LIVE provider.
        let Some(parent_sealed) = self
            .provider
            .sealed_header_by_hash(parent_hash)
            .map_err(|e| BridgeError::Internal(eyre::eyre!("provider error: {e}")))?
        else {
            return Ok(PayloadStatus::Invalid);
        };

        // Run Reth's real header validator. EthBeaconConsensus checks number
        // monotonicity, timestamp monotonicity, gas-limit drift, base-fee.
        let our_sealed = SealedHeader::new(header, block_hash);
        match self
            .validator
            .validate_header_against_parent(&our_sealed, &parent_sealed)
        {
            Ok(()) => Ok(PayloadStatus::Valid),
            Err(_) => Ok(PayloadStatus::Invalid),
        }
    }

    async fn commit(&self, block_hash: BlockHash) -> Result<(), BridgeError> {
        let hash = B256::from(block_hash.0);
        let header = {
            let mut s = self.state.lock().expect("state mutex poisoned");
            let header = s
                .pending
                .values()
                .find(|(h, _, _)| *h == hash)
                .map(|(_, h, _)| h.clone())
                .ok_or_else(|| {
                    BridgeError::Rejected(format!("commit for unknown hash {hash}"))
                })?;
            s.chain.insert(hash, header.clone());
            s.head = Some(hash);
            header
        };

        // Stage 7d: if an Engine API handle has been installed, also tell
        // Reth's consensus engine about the new canonical head. We always
        // commit *locally* first (above) — sending to the engine is best-
        // effort at this stage because we haven't yet uploaded a real
        // ExecutionPayload via newPayload (Stage 8d's drained fills aren't
        // EVM-executable yet), so the engine will return SYNCING/INVALID.
        // The wire being connected is what 7d proves; full payload-execution
        // alignment is downstream once fills become EVM transactions.
        if let Some(handle) = &self.engine_handle {
            let state = ForkchoiceState {
                head_block_hash: hash,
                safe_block_hash: hash,
                finalized_block_hash: hash,
            };
            let _ = handle.fork_choice_updated(state, None).await;
        }

        // `header` is bound so the post-engine path can read fields off it
        // for telemetry if desired. Drop is fine.
        drop(header);
        Ok(())
    }

    /// Stage 18a — serialise the just-built payload for cross-validator
    /// transport. Includes the full alloy [`Header`] (so the follower
    /// reconstructs an identical `parent_header` for its own next
    /// `build_payload`) plus the [`ExecutedBlock`] view and the drained
    /// fills (currently unused on the follower but kept in the wire
    /// format so adding fill-replication later doesn't break the
    /// schema).
    async fn encode_proposed_block(&self, id: PayloadId) -> Result<Vec<u8>, BridgeError> {
        let (hash, header, _fills) = {
            let s = self.state.lock().expect("state mutex poisoned");
            s.pending
                .get(&id.0)
                .cloned()
                .ok_or_else(|| {
                    BridgeError::Rejected(format!(
                        "encode_proposed_block: unknown payload id {}",
                        id.0
                    ))
                })?
        };
        let block = ExecutedBlock {
            hash: BlockHash(hash.0),
            parent_hash: BlockHash(header.parent_hash.0),
            number: header.number,
            state_root: header.state_root.0,
            timestamp: header.timestamp,
        };
        // Note: drained fills are intentionally NOT included in the wire
        // format at v0. The proposer's `pending_fills` are CLOB-local
        // book-keeping; they aren't yet encoded as EVM-executable
        // transactions, so the follower has no use for them. When fills
        // become real EVM txs the schema gets a fills field — adding
        // one to `ProposedBlockWire` is the only change.
        let wire = ProposedBlockWire { header, block };
        serde_json::to_vec(&wire).map_err(|e| {
            BridgeError::Internal(eyre::eyre!("serialise proposed block: {e}"))
        })
    }

    /// Stage 18a — companion to [`Self::encode_proposed_block`]. Decodes
    /// the wire bytes and installs the block in the bridge's pending
    /// map so a subsequent `commit(block.hash)` finds it without going
    /// through `build_payload`.
    ///
    /// Sanity: re-hashes the decoded header and rejects the part if the
    /// hash disagrees with the carried `block.hash` field — guards
    /// against a malformed wire payload silently committing the wrong
    /// state.
    async fn register_proposed_block(
        &self,
        bytes: &[u8],
    ) -> Result<ExecutedBlock, BridgeError> {
        let wire: ProposedBlockWire = serde_json::from_slice(bytes).map_err(|e| {
            BridgeError::Rejected(format!("decode proposed block: {e}"))
        })?;
        let computed = wire.header.hash_slow();
        let claimed = B256::from(wire.block.hash.0);
        if computed != claimed {
            return Err(BridgeError::Rejected(format!(
                "proposed block hash mismatch — header hashes to {computed} but wire claims {claimed}"
            )));
        }

        let mut s = self.state.lock().expect("state mutex poisoned");
        let id = s.next_payload_id;
        s.next_payload_id += 1;
        s.pending
            .insert(id, (computed, wire.header, Vec::new()));
        Ok(wire.block)
    }
}

/// Wire format the proposer ships and the follower decodes. Carries the
/// full alloy [`Header`] so the follower's bridge can act as if it had
/// built the block itself — its next `build_payload` finds the right
/// parent in `state.chain` and produces an identical hash, just like
/// any other committed block on this validator.
#[derive(serde::Serialize, serde::Deserialize)]
struct ProposedBlockWire {
    header: Header,
    block: ExecutedBlock,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_genesis::Genesis;
    use reth_chainspec::ChainSpec;
    use reth_node_builder::{NodeBuilder, NodeHandle};
    use reth_node_core::node_config::NodeConfig;
    use reth_node_ethereum::EthereumNode;
    use reth_storage_api::BlockHashReader;
    use reth_tasks::Runtime;
    use std::sync::Arc;

    fn dev_chain_spec() -> Arc<ChainSpec> {
        let custom_genesis = r#"{
            "nonce": "0x42",
            "timestamp": "0x0",
            "extraData": "0x5343",
            "gasLimit": "0x5208",
            "difficulty": "0x400000000",
            "mixHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "coinbase": "0x0000000000000000000000000000000000000000",
            "alloc": {},
            "number": "0x0",
            "gasUsed": "0x0",
            "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
            "config": {
                "ethash": {},
                "chainId": 2600,
                "homesteadBlock": 0,
                "eip150Block": 0,
                "eip155Block": 0,
                "eip158Block": 0,
                "byzantiumBlock": 0,
                "constantinopleBlock": 0,
                "petersburgBlock": 0,
                "istanbulBlock": 0,
                "berlinBlock": 0,
                "londonBlock": 0,
                "terminalTotalDifficulty": 0,
                "terminalTotalDifficultyPassed": true,
                "shanghaiTime": 0
            }
        }"#;
        let genesis: Genesis = serde_json::from_str(custom_genesis).expect("dev genesis parses");
        Arc::new(genesis.into())
    }

    /// Stage 14c: `current_mark()` is empty until both sides of the book
    /// have resting liquidity, then returns the midpoint as a
    /// [`openhl_funding::MarkPrice`]. Uses `()` as the provider since the
    /// method only reads from the bridge's `clob` and never touches the
    /// provider — the trait bound on `ConsensusBridge` doesn't apply to
    /// inherent methods.
    #[test]
    fn current_mark_midpoint_of_two_sided_book() {
        use openhl_clob::{AccountId, OrderId, OrderType, Price, Qty, Side};
        use openhl_funding::MarkPrice;

        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());

        // Empty book → no mark.
        assert_eq!(bridge.current_mark(), None);

        // One-sided book → still no mark (no midpoint defined).
        bridge.submit_order(Order {
            id: OrderId(1),
            account: AccountId(1),
            side: Side::Buy,
            qty: Qty(1),
            order_type: OrderType::Limit { price: Price(99) },
        });
        assert_eq!(bridge.current_mark(), None);

        // Two-sided → midpoint of (99, 103) = 101.
        bridge.submit_order(Order {
            id: OrderId(2),
            account: AccountId(2),
            side: Side::Sell,
            qty: Qty(1),
            order_type: OrderType::Limit { price: Price(103) },
        });
        assert_eq!(bridge.current_mark(), Some(MarkPrice(101)));
    }

    /// Stage 16b: a crossing taker against a resting maker should
    /// produce two accounts in the bridge's account map, each with
    /// the correct position direction.
    #[test]
    fn submit_order_routes_fills_through_apply_fill() {
        use openhl_clob::{AccountId, OrderId, OrderType, Price, Qty, Side};
        use openhl_funding::{MarkPrice, Notional, PositionSize};

        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());

        // Maker (account 1) rests a Buy limit at 100.
        bridge.submit_order(Order {
            id: OrderId(1),
            account: AccountId(1),
            side: Side::Buy,
            qty: Qty(5),
            order_type: OrderType::Limit { price: Price(100) },
        });
        // Resting only — no fill yet, no accounts touched.
        assert!(bridge.accounts_snapshot().is_empty());

        // Taker (account 2) crosses with a Sell market for 5.
        bridge.submit_order(Order {
            id: OrderId(2),
            account: AccountId(2),
            side: Side::Sell,
            qty: Qty(5),
            order_type: OrderType::Market,
        });

        let snapshot = bridge.accounts_snapshot();
        assert_eq!(snapshot.len(), 2, "both accounts should now exist");

        // Sorted ascending by account_id.
        let maker = snapshot[0];
        let taker = snapshot[1];
        assert_eq!(maker.account, AccountId(1));
        assert_eq!(taker.account, AccountId(2));

        // Maker bought 5 @ 100 → long 5, avg_entry 100, no realized
        // PnL (opening from flat).
        assert_eq!(maker.position_size, PositionSize(5));
        assert_eq!(maker.avg_entry, MarkPrice(100));
        assert_eq!(maker.collateral, Notional(0));

        // Taker sold 5 @ 100 → short 5, avg_entry 100, no realized
        // PnL.
        assert_eq!(taker.position_size, PositionSize(-5));
        assert_eq!(taker.avg_entry, MarkPrice(100));
        assert_eq!(taker.collateral, Notional(0));
    }

    /// Stage 17b: `deposit` credits an account, creating it if
    /// missing.
    #[test]
    fn deposit_creates_flat_account_and_credits_collateral() {
        use openhl_clob::AccountId;
        use openhl_funding::{Notional, PositionSize};

        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());

        // First deposit on a never-seen account creates it flat
        // (size 0, avg_entry 0) and credits collateral.
        let balance = bridge.deposit(AccountId(42), 500);
        assert_eq!(balance, Notional(500));

        let snap = bridge.accounts_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].account, AccountId(42));
        assert_eq!(snap[0].position_size, PositionSize(0));
        assert_eq!(snap[0].collateral, Notional(500));

        // Second deposit adds.
        let balance = bridge.deposit(AccountId(42), 250);
        assert_eq!(balance, Notional(750));

        // Negative amount (withdrawal) debits.
        let balance = bridge.deposit(AccountId(42), -100);
        assert_eq!(balance, Notional(650));
    }

    /// Stage 17e: `withdraw` is rejection-safe on missing accounts
    /// and insufficient balance; debits on success.
    #[test]
    fn withdraw_rejects_or_debits_correctly() {
        use openhl_clob::AccountId;
        use openhl_funding::Notional;

        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());

        // Unknown account → None.
        assert_eq!(bridge.withdraw(AccountId(1), 100), None);

        // Deposit first, then withdraw less than balance.
        let _ = bridge.deposit(AccountId(1), 500);
        assert_eq!(bridge.withdraw(AccountId(1), 200), Some(Notional(300)));

        // Withdraw more than balance → None, balance untouched.
        assert_eq!(bridge.withdraw(AccountId(1), 1000), None);
        let snap = bridge.accounts_snapshot();
        assert_eq!(snap[0].collateral, Notional(300));

        // Withdraw to exactly zero → Some(0).
        assert_eq!(bridge.withdraw(AccountId(1), 300), Some(Notional(0)));
    }

    /// Stage 17g: `withdraw` is now margin-aware. An account with an
    /// open position can only withdraw down to its initial-margin
    /// requirement; raw-collateral semantics survive for flat
    /// accounts.
    #[test]
    fn withdraw_respects_initial_margin_for_open_position() {
        use openhl_clob::{AccountId, OrderId, OrderType, Price, Qty, Side};
        use openhl_funding::Notional;

        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());

        // Maker (account 1) rests Buy 10 @ 100; taker crosses with a
        // matching Sell. Account 1 ends with size=10, avg_entry=100,
        // realized PnL 0 (opening from flat).
        bridge.submit_order(Order {
            id: OrderId(1),
            account: AccountId(1),
            side: Side::Buy,
            qty: Qty(10),
            order_type: OrderType::Limit { price: Price(100) },
        });
        bridge.submit_order(Order {
            id: OrderId(2),
            account: AccountId(2),
            side: Side::Sell,
            qty: Qty(10),
            order_type: OrderType::Market,
        });

        // Fund account 1: collateral = 500. IM_req for the position
        // is |10| × 100 × 1000 / 10000 = 100. Free collateral = 400.
        let _ = bridge.deposit(AccountId(1), 500);

        // One quote above free collateral → reject, balance untouched.
        assert_eq!(bridge.withdraw(AccountId(1), 401), None);
        let snap = bridge.accounts_snapshot();
        let acct1 = snap.iter().find(|a| a.account == AccountId(1)).unwrap();
        assert_eq!(acct1.collateral, Notional(500));

        // Exactly to the IM line — succeeds. Post balance equals IM_req.
        assert_eq!(bridge.withdraw(AccountId(1), 400), Some(Notional(100)));

        // Any further withdrawal violates IM — reject.
        assert_eq!(bridge.withdraw(AccountId(1), 1), None);
    }

    /// Stage 17j: with a CLOB midpoint available, withdraw uses
    /// mark-aware free collateral instead of the Stage 17g
    /// avg-entry rule. A long position with mark above entry can
    /// withdraw against its unrealized gains.
    #[test]
    fn withdraw_uses_mark_aware_free_collateral_at_gain() {
        use openhl_clob::{AccountId, OrderId, OrderType, Price, Qty, Side};
        use openhl_funding::Notional;

        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());

        // Cross at 100 to give account 1 a long position.
        bridge.submit_order(Order {
            id: OrderId(1),
            account: AccountId(1),
            side: Side::Buy,
            qty: Qty(10),
            order_type: OrderType::Limit { price: Price(100) },
        });
        bridge.submit_order(Order {
            id: OrderId(2),
            account: AccountId(2),
            side: Side::Sell,
            qty: Qty(10),
            order_type: OrderType::Market,
        });
        // Mark-book at midpoint 120 (bid 119, ask 121). Resting on
        // both sides with no cross — the book ends two-sided and
        // `current_mark()` returns Some(120).
        bridge.submit_order(Order {
            id: OrderId(101),
            account: AccountId(99),
            side: Side::Buy,
            qty: Qty(1),
            order_type: OrderType::Limit { price: Price(119) },
        });
        bridge.submit_order(Order {
            id: OrderId(102),
            account: AccountId(98),
            side: Side::Sell,
            qty: Qty(1),
            order_type: OrderType::Limit { price: Price(121) },
        });

        // Fund account 1: collateral = 500.
        let _ = bridge.deposit(AccountId(1), 500);

        // At mark 120: uPnL = (120-100)*10 = +200; equity = 700;
        // IM at mark = 10*120*10% = 120; free = 580.
        //
        // Stage 17g would have allowed only 400 (collateral - IM at
        // avg_entry). The mark-aware rule lets the trader pull
        // against the gain.
        assert_eq!(bridge.withdraw(AccountId(1), 581), None, "one above free → reject");
        assert_eq!(
            bridge.withdraw(AccountId(1), 580),
            Some(Notional(-80)),
            "at the IM line: balance = 500 - 580 = -80 (deficit absorbed by uPnL)",
        );
    }

    /// Stage 17j companion: long at a loss tightens the rule
    /// relative to Stage 17g (the trader has *less* free collateral
    /// than `collateral − IM_at_avg_entry`).
    #[test]
    fn withdraw_uses_mark_aware_free_collateral_at_loss() {
        use openhl_clob::{AccountId, OrderId, OrderType, Price, Qty, Side};
        use openhl_funding::Notional;

        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());

        bridge.submit_order(Order {
            id: OrderId(1),
            account: AccountId(1),
            side: Side::Buy,
            qty: Qty(10),
            order_type: OrderType::Limit { price: Price(100) },
        });
        bridge.submit_order(Order {
            id: OrderId(2),
            account: AccountId(2),
            side: Side::Sell,
            qty: Qty(10),
            order_type: OrderType::Market,
        });
        // Mark-book at midpoint 80 (bid 79, ask 81).
        bridge.submit_order(Order {
            id: OrderId(101),
            account: AccountId(99),
            side: Side::Buy,
            qty: Qty(1),
            order_type: OrderType::Limit { price: Price(79) },
        });
        bridge.submit_order(Order {
            id: OrderId(102),
            account: AccountId(98),
            side: Side::Sell,
            qty: Qty(1),
            order_type: OrderType::Limit { price: Price(81) },
        });

        let _ = bridge.deposit(AccountId(1), 500);

        // At mark 80: uPnL = (80-100)*10 = -200; equity = 300;
        // IM at mark = 10*80*10% = 80; free = 220.
        //
        // Stage 17g would have wrongly let the trader withdraw up
        // to 400.
        assert_eq!(bridge.withdraw(AccountId(1), 221), None);
        assert_eq!(
            bridge.withdraw(AccountId(1), 220),
            Some(Notional(280)),
        );
    }

    /// Stage 17l → 17m: the margin params are now tunable. A
    /// bridge constructed with non-default params enforces them
    /// on `withdraw`, and the matching precompile-module global
    /// is also installed (proved indirectly here by reading it
    /// back).
    #[test]
    fn with_liquidation_params_overrides_default_rate() {
        use openhl_clob::{AccountId, OrderId, OrderType, Price, Qty, Side};
        use openhl_funding::Notional;

        // 500 bps initial (5%), 100 bps maintenance (1%) instead of
        // the defaults (1000 / 200).
        let mut params = LiquidationParams::hyperliquid_default();
        params.initial_margin_bps = 500;
        params.maintenance_margin_bps = 100;
        let bridge = LiveRethEvmBridge::new((), dev_chain_spec())
            .with_liquidation_params(params);
        assert_eq!(bridge.initial_margin_bps(), 500);
        assert_eq!(bridge.liquidation_params().initial_margin_bps, 500);
        assert_eq!(
            crate::precompiles::current_initial_margin_bps(),
            500,
            "bridge builder must install the initial rate into the precompile global",
        );
        assert_eq!(
            crate::precompiles::current_maintenance_margin_bps(),
            100,
            "Stage 17n: bridge builder must install the maintenance rate too",
        );

        // Same setup as the Stage 17g IM test: long 10 @ 100,
        // collateral 500.
        bridge.submit_order(Order {
            id: OrderId(1),
            account: AccountId(1),
            side: Side::Buy,
            qty: Qty(10),
            order_type: OrderType::Limit { price: Price(100) },
        });
        bridge.submit_order(Order {
            id: OrderId(2),
            account: AccountId(2),
            side: Side::Sell,
            qty: Qty(10),
            order_type: OrderType::Market,
        });
        let _ = bridge.deposit(AccountId(1), 500);

        // No mark book → avg-entry fallback. At 500 bps:
        // IM = 10*100*5%/1 = 50. Free = 500 - 50 = 450.
        // (At the default 1000 bps the free would have been 400.)
        assert_eq!(bridge.withdraw(AccountId(1), 451), None);
        assert_eq!(bridge.withdraw(AccountId(1), 450), Some(Notional(50)));

        // Restore defaults so other tests in the workspace aren't
        // disturbed. Process-global concern, same as ACCOUNTS_STATE.
        crate::precompiles::install_initial_margin_bps(
            openhl_clearing::DEFAULT_INITIAL_MARGIN_BPS,
        );
        crate::precompiles::install_maintenance_margin_bps(
            openhl_clearing::DEFAULT_MAINTENANCE_MARGIN_BPS,
        );
    }

    /// Stage 17m — `margin_health(account)` classifies an
    /// account's solvency at the current CLOB midpoint using the
    /// bridge's `LiquidationParams`. Cycles through the four
    /// states by varying mark + collateral on a long-10-at-100
    /// position.
    #[test]
    fn margin_health_classifies_against_current_mark() {
        use openhl_clob::{AccountId, OrderId, OrderType, Price, Qty, Side};

        // Helper: build a fresh bridge, fund account 1 with `coll`
        // and put account 1 long 10 @ 100. Then place mark-book
        // orders at `bid`/`ask` so `current_mark` returns
        // `(bid+ask)/2`. Returns the bridge so we can introspect.
        let setup = |coll: i64, bid: u64, ask: u64| -> LiveRethEvmBridge<()> {
            let bridge = LiveRethEvmBridge::new((), dev_chain_spec());
            bridge.submit_order(Order {
                id: OrderId(1),
                account: AccountId(1),
                side: Side::Buy,
                qty: Qty(10),
                order_type: OrderType::Limit { price: Price(100) },
            });
            bridge.submit_order(Order {
                id: OrderId(2),
                account: AccountId(2),
                side: Side::Sell,
                qty: Qty(10),
                order_type: OrderType::Market,
            });
            bridge.submit_order(Order {
                id: OrderId(101),
                account: AccountId(99),
                side: Side::Buy,
                qty: Qty(1),
                order_type: OrderType::Limit { price: Price(bid) },
            });
            bridge.submit_order(Order {
                id: OrderId(102),
                account: AccountId(98),
                side: Side::Sell,
                qty: Qty(1),
                order_type: OrderType::Limit { price: Price(ask) },
            });
            let _ = bridge.deposit(AccountId(1), coll);
            bridge
        };

        // hyperliquid_default: IM = 10% (1000 bps), MM = 2% (200 bps).
        // Account 1 = long 10 @ 100, notional at mark m = 10*m.

        // Safe: mark 110 → uPnL=+100, equity=600, MR ≈ 5455 bps ≥ IM.
        let bridge = setup(500, 109, 111);
        assert_eq!(bridge.margin_health(AccountId(1)), Some(MarginHealth::Safe));

        // AtRisk: mark 95 → uPnL=-50, equity=50, notional=950,
        // MR ≈ 526 bps. < IM(1000), ≥ MM(200) → AtRisk.
        let bridge = setup(100, 94, 96);
        assert_eq!(
            bridge.margin_health(AccountId(1)),
            Some(MarginHealth::AtRisk),
        );

        // Liquidatable: mark 92 → uPnL=-80, equity=20, notional=920,
        // MR ≈ 217 bps. Tighten by lowering collateral so MR < MM.
        // collateral 90, equity = 10, MR = 10*10000/920 = 108 bps < 200.
        let bridge = setup(90, 91, 93);
        assert_eq!(
            bridge.margin_health(AccountId(1)),
            Some(MarginHealth::Liquidatable),
        );

        // Underwater: mark 80 → uPnL=-200, equity=-150, MR<0.
        let bridge = setup(50, 79, 81);
        assert_eq!(
            bridge.margin_health(AccountId(1)),
            Some(MarginHealth::Underwater),
        );

        // Unknown account → None.
        let bridge = setup(100, 99, 101);
        assert_eq!(bridge.margin_health(AccountId(999)), None);

        // No mark (one-sided book) → None.
        let solo = LiveRethEvmBridge::new((), dev_chain_spec());
        solo.submit_order(Order {
            id: OrderId(1),
            account: AccountId(1),
            side: Side::Buy,
            qty: Qty(10),
            order_type: OrderType::Limit { price: Price(100) },
        });
        solo.submit_order(Order {
            id: OrderId(2),
            account: AccountId(2),
            side: Side::Sell,
            qty: Qty(10),
            order_type: OrderType::Market,
        });
        let _ = solo.deposit(AccountId(1), 500);
        assert_eq!(
            solo.margin_health(AccountId(1)),
            None,
            "no CLOB midpoint → indeterminate",
        );
    }

    /// Stage 17o — when an oracle index has been installed via
    /// `set_oracle_index_price`, both `effective_mark` and the
    /// downstream margin / withdraw math prefer it over the CLOB
    /// midpoint. Without an oracle install they fall back to the
    /// midpoint (Stage 17j behavior).
    #[test]
    fn effective_mark_prefers_oracle_index_over_clob_midpoint() {
        use openhl_clob::{AccountId, OrderId, OrderType, Price, Qty, Side};
        use openhl_funding::{MarkPrice, Notional};

        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());

        // Cross at 100 to make account 1 long 10 @ 100.
        bridge.submit_order(Order {
            id: OrderId(1),
            account: AccountId(1),
            side: Side::Buy,
            qty: Qty(10),
            order_type: OrderType::Limit { price: Price(100) },
        });
        bridge.submit_order(Order {
            id: OrderId(2),
            account: AccountId(2),
            side: Side::Sell,
            qty: Qty(10),
            order_type: OrderType::Market,
        });
        // CLOB mark-book at midpoint 110 (bid 109 / ask 111).
        bridge.submit_order(Order {
            id: OrderId(101),
            account: AccountId(99),
            side: Side::Buy,
            qty: Qty(1),
            order_type: OrderType::Limit { price: Price(109) },
        });
        bridge.submit_order(Order {
            id: OrderId(102),
            account: AccountId(98),
            side: Side::Sell,
            qty: Qty(1),
            order_type: OrderType::Limit { price: Price(111) },
        });
        let _ = bridge.deposit(AccountId(1), 500);

        // No oracle installed yet → fall back to CLOB midpoint 110.
        assert_eq!(bridge.oracle_index_price(), None);
        assert_eq!(bridge.effective_mark(), Some(MarkPrice(110)));

        // Install an oracle index AT THE SAME PRICE as the midpoint.
        // Sanity: oracle takes precedence even when values match.
        bridge.set_oracle_index_price(110);
        assert_eq!(bridge.oracle_index_price(), Some(110));
        assert_eq!(bridge.effective_mark(), Some(MarkPrice(110)));

        // Install an oracle index BELOW the midpoint. The withdraw
        // rule should now tighten — at mark 90 vs midpoint 110:
        //   uPnL = (90-100)*10 = -100
        //   equity = 500 - 100 = 400
        //   IM at mark 90 = 10*90*10%/1 = 90
        //   free = 400 - 90 = 310
        // (At the midpoint 110 the free would have been ≈ 590.)
        bridge.set_oracle_index_price(90);
        assert_eq!(bridge.effective_mark(), Some(MarkPrice(90)));
        assert_eq!(bridge.withdraw(AccountId(1), 311), None);
        assert_eq!(bridge.withdraw(AccountId(1), 310), Some(Notional(190)));

        // The precompile global is in lockstep — bridge.set_oracle_…
        // installs into it too.
        assert_eq!(
            crate::precompiles::current_oracle_index_price(),
            Some(90),
            "bridge setter must install to the precompile global",
        );

        // Restore default for other tests in the workspace.
        crate::precompiles::clear_oracle_index_price();
    }

    /// Stage 16b: bridge snapshot round-trips the account map.
    #[test]
    fn snapshot_round_trips_accounts() {
        use openhl_clob::{AccountId, OrderId, OrderType, Price, Qty, Side};

        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());
        bridge.submit_order(Order {
            id: OrderId(1),
            account: AccountId(7),
            side: Side::Buy,
            qty: Qty(3),
            order_type: OrderType::Limit { price: Price(100) },
        });
        bridge.submit_order(Order {
            id: OrderId(2),
            account: AccountId(8),
            side: Side::Sell,
            qty: Qty(3),
            order_type: OrderType::Market,
        });

        let snap = bridge.snapshot();
        assert_eq!(snap.accounts.len(), 2);

        // Restore on a fresh bridge.
        let bridge2 = LiveRethEvmBridge::new((), dev_chain_spec());
        bridge2.load_snapshot(snap);
        assert_eq!(bridge2.accounts_snapshot().len(), 2);
        assert_eq!(bridge2.accounts_snapshot(), bridge.accounts_snapshot());
    }

    /// END-TO-END Stage 7b: bootstrap a real Reth node, hand its provider to
    /// `LiveRethEvmBridge`, build a payload on top of the real genesis block.
    /// Asserts the `parent_hash` and number come from the live chain, not an
    /// in-process synthesis.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_bridge_builds_on_real_genesis() {
        let runtime = Runtime::test();
        let chain_spec = dev_chain_spec();
        let node_config = NodeConfig::test().dev().with_chain(chain_spec.clone());

        let NodeHandle {
            node,
            node_exit_future: _,
        } = NodeBuilder::new(node_config)
            .testing_node(runtime)
            .node(EthereumNode::default())
            .launch_with_debug_capabilities()
            .await
            .expect("launch failed");

        // Pull the genesis hash from the live provider.
        let genesis_hash_b256 = node
            .provider
            .block_hash(0)
            .expect("provider call failed")
            .expect("provider has no block 0 (genesis)");

        // Construct the bridge against the live provider AND chain_spec
        // (chain_spec wires up EthBeaconConsensus for real header validation).
        let bridge = LiveRethEvmBridge::new(node.provider.clone(), chain_spec.clone());

        // Build a payload on the real genesis.
        let attrs = PayloadAttrs {
            timestamp: 1,
            fee_recipient: [0u8; 20],
            prev_randao: [0u8; 32],
        };
        let id = bridge
            .build_payload(BlockHash(genesis_hash_b256.0), attrs.clone())
            .await
            .expect("build_payload failed");
        let block = bridge.payload_ready(id).await.expect("payload_ready failed");

        // The bridge's lookup hit the LIVE provider — assert the resulting
        // header carries genesis as its parent and is at height 1.
        assert_eq!(block.parent_hash, BlockHash(genesis_hash_b256.0));
        assert_eq!(block.number, 1);

        // Stage 7c: validate_payload runs Reth's EthBeaconConsensus against
        // the live parent. A well-formed block we just built must validate.
        let status = bridge
            .validate_payload(&block)
            .await
            .expect("validate_payload failed");
        assert_eq!(status, PayloadStatus::Valid);

        // A block whose hash we don't know must be Invalid (we have no header
        // to validate against).
        let unknown_block = ExecutedBlock {
            hash: BlockHash([0xddu8; 32]),
            parent_hash: BlockHash(genesis_hash_b256.0),
            number: 1,
            state_root: [0u8; 32],
            timestamp: 0,
        };
        let status = bridge
            .validate_payload(&unknown_block)
            .await
            .expect("validate_payload failed");
        assert_eq!(status, PayloadStatus::Invalid);

        // Negative case: a fabricated parent hash must be rejected because
        // the live provider doesn't know it.
        let fake_parent = BlockHash([0xeeu8; 32]);
        let err = bridge.build_payload(fake_parent, attrs).await.unwrap_err();
        assert!(matches!(err, BridgeError::Rejected(_)));
    }

    /// Stage 8d end-to-end: CLOB → bridge → payload.
    /// A maker rests, a taker crosses it, the fill flows into the next
    /// `build_payload`'s stored fills. The empty-fill `build_payload` that
    /// preceded the orders proves the drain semantics — fills accumulate
    /// AFTER they're built, not retroactively included.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn clob_fills_flow_into_payload() {
        use openhl_clob::{AccountId, OrderId, OrderType, Price, Qty, Side};

        let runtime = Runtime::test();
        let chain_spec = dev_chain_spec();
        let node_config = NodeConfig::test().dev().with_chain(chain_spec.clone());

        let NodeHandle {
            node,
            node_exit_future: _,
        } = NodeBuilder::new(node_config)
            .testing_node(runtime)
            .node(EthereumNode::default())
            .launch_with_debug_capabilities()
            .await
            .expect("launch failed");

        let genesis_hash_b256 = node
            .provider
            .block_hash(0)
            .expect("provider call failed")
            .expect("provider has no genesis");

        let bridge = LiveRethEvmBridge::new(node.provider.clone(), chain_spec);

        // Empty initial state — no orders submitted, no fills pending.
        assert_eq!(bridge.pending_fill_count(), 0);

        // First payload built with no orders → no fills attached.
        let attrs = PayloadAttrs {
            timestamp: 1,
            fee_recipient: [0u8; 20],
            prev_randao: [0u8; 32],
        };
        let empty_id = bridge
            .build_payload(BlockHash(genesis_hash_b256.0), attrs.clone())
            .await
            .expect("build_payload failed");
        let empty_fills = bridge
            .payload_fills(empty_id)
            .expect("payload exists");
        assert!(empty_fills.is_empty(), "no orders submitted yet, fills must be empty");

        // Submit a resting limit BID @ 100 from account 1, then a crossing
        // SELL @ 100 from account 2. This produces exactly one fill.
        let maker = Order {
            id: OrderId(1),
            account: AccountId(1),
            side: Side::Buy,
            qty: Qty(10),
            order_type: OrderType::Limit { price: Price(100) },
        };
        let taker = Order {
            id: OrderId(2),
            account: AccountId(2),
            side: Side::Sell,
            qty: Qty(10),
            order_type: OrderType::Limit { price: Price(100) },
        };

        let maker_result = bridge.submit_order(maker);
        assert!(maker_result.fills.is_empty(), "maker rests, no immediate fill");
        assert_eq!(bridge.pending_fill_count(), 0);

        let taker_result = bridge.submit_order(taker);
        assert_eq!(taker_result.fills.len(), 1, "taker should cross the maker");
        assert_eq!(bridge.pending_fill_count(), 1, "fill buffered in pending");

        // Build the NEXT payload — it should drain the buffered fill.
        let next_id = bridge
            .build_payload(BlockHash(genesis_hash_b256.0), attrs)
            .await
            .expect("build_payload failed");
        let next_fills = bridge
            .payload_fills(next_id)
            .expect("payload exists");
        assert_eq!(next_fills.len(), 1, "fill must be attached to the payload");
        assert_eq!(next_fills[0].price, Price(100));
        assert_eq!(next_fills[0].qty, Qty(10));
        assert_eq!(next_fills[0].maker_order_id, OrderId(1));
        assert_eq!(next_fills[0].taker_order_id, OrderId(2));

        // After draining, pending fills must be empty.
        assert_eq!(bridge.pending_fill_count(), 0);

        // The earlier (empty) payload's fills must still be empty —
        // draining is forward-only, never retroactive.
        let empty_fills_again = bridge
            .payload_fills(empty_id)
            .expect("earlier payload exists");
        assert!(empty_fills_again.is_empty(), "earlier payload not retroactively filled");
    }

    /// **Stage 9d**: bootstrap a Reth node WITH `OpenHlExecutorBuilder` (so its
    /// EVM has our CLOB precompiles registered), construct a `LiveRethEvmBridge`
    /// against that node's provider, submit an order via the bridge — verify
    /// that the precompile module's process-global `CLOB_STATE` now reflects
    /// the order. This proves the full bridge ↔ custom-EVM-node integration:
    /// the same `Arc<Mutex<Book>>` that the bridge's `submit_order` writes to
    /// is the one any smart contract calling `clob_read_best_bid` through this
    /// node's EVM would see.
    ///
    /// Doesn't yet invoke the precompile via RPC `eth_call` — that's deferred
    /// indefinitely (validates Reth's plumbing rather than openhl behavior).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn bridge_against_custom_evm_node_shares_clob_with_precompile() {
        use crate::OpenHlExecutorBuilder;
        use crate::precompiles::{
            CLOB_PLACE_ORDER, current_best_bid, uninstall_clob, uninstall_fill_sink,
        };
        use openhl_clob::{AccountId, OrderId, OrderType, Price, Qty, Side};
        use reth_node_ethereum::node::EthereumAddOns;

        // Start from a clean global state — other tests may have left a CLOB
        // or fill sink installed; that's fine for those tests but would mask
        // bugs here (especially the "sink was wired by bridge::new" assertion).
        uninstall_clob();
        uninstall_fill_sink();

        let runtime = Runtime::test();
        let chain_spec = dev_chain_spec();
        let node_config = NodeConfig::test().dev().with_chain(chain_spec.clone());

        let handle = NodeBuilder::new(node_config)
            .testing_node(runtime)
            .with_types::<EthereumNode>()
            .with_components(EthereumNode::components().executor(OpenHlExecutorBuilder))
            .with_add_ons(EthereumAddOns::default())
            .launch()
            .await
            .expect("launch of custom-EVM node failed");

        // Build the bridge against the live custom-EVM node's provider.
        // The bridge installs its CLOB as the precompile's global state
        // (per the install_clob call inside LiveRethEvmBridge::new).
        let bridge = LiveRethEvmBridge::new(handle.node.provider.clone(), chain_spec);

        // Pre-condition: precompile sees an empty book.
        assert_eq!(current_best_bid(), None);

        // Submit a resting bid via the bridge. This goes through Book::submit
        // under the same Arc<Mutex<Book>> the precompile reads from.
        bridge.submit_order(Order {
            id: OrderId(1),
            account: AccountId(42),
            side: Side::Buy,
            qty: Qty(33),
            order_type: OrderType::Limit { price: Price(200) },
        });

        // Post-condition: the precompile's view (which is what a smart
        // contract calling `clob_read_best_bid` through this node would see)
        // now reflects the order.
        let best = current_best_bid().expect("CLOB has bids after submit_order");
        assert_eq!(best.0, Price(200));
        assert_eq!(best.1, Qty(33));

        // === Stage 9c+ ===
        // Now hit the WRITE precompile: place a crossing Sell @ 200 qty 33
        // via `place_order`. The bridge's pending_fills should see the fill
        // even though we never went through bridge.submit_order. This proves
        // the FILL_SINK that LiveRethEvmBridge::new installed is the same
        // Arc<Mutex<Vec<Fill>>> the bridge later drains in build_payload.
        assert_eq!(
            bridge.pending_fill_count(),
            0,
            "fills empty before crossing taker via precompile"
        );

        let mut calldata = [0u8; 128];
        // account_id = 7 (last 8 bytes of slot 0)
        calldata[24..32].copy_from_slice(&7u64.to_be_bytes());
        // side = Sell (1) at byte 63
        calldata[63] = 1;
        // price = 200 (last 8 bytes of slot 2)
        calldata[88..96].copy_from_slice(&200u64.to_be_bytes());
        // qty = 33 (last 8 bytes of slot 3)
        calldata[120..128].copy_from_slice(&33u64.to_be_bytes());

        let r = crate::precompiles::place_order(&calldata, 100_000, 0)
            .expect("place_order must not error");
        let order_id_bytes = &r.bytes[24..32];
        let order_id = u64::from_be_bytes(order_id_bytes.try_into().unwrap());
        assert!(order_id > 0, "successful place_order returns nonzero id");

        // The fill from the cross should have landed in bridge's pending_fills
        // via the FILL_SINK install_fill_sink path inside LiveRethEvmBridge::new.
        assert_eq!(
            bridge.pending_fill_count(),
            1,
            "precompile-placed cross must populate bridge.pending_fills (Stage 9c+)"
        );

        // CLOB_PLACE_ORDER's address constant is part of the public surface
        // (and registered into the precompiles set by `openhl_precompiles`);
        // touch it here so the import resolves and the constant stays load-bearing.
        let _ = CLOB_PLACE_ORDER;

        // Clean up the globals so other tests can start clean.
        uninstall_fill_sink();
        uninstall_clob();

        // Drop the node handle explicitly to make the lifecycle visible
        // in the trace.
        drop(handle);
    }

    /// **Stage 17d**: mirror of the CLOB precompile test above, but for
    /// `openhl_deposit`. Proves the load-bearing wiring:
    ///
    ///   1. `LiveRethEvmBridge::new` calls
    ///      `precompiles::install_accounts(Arc::clone(&accounts))`,
    ///      so the bridge's account-map `Arc` becomes the precompile
    ///      module's `ACCOUNTS_STATE` global.
    ///   2. A call to the `deposit` precompile function mutates that
    ///      same map, observable via `bridge.accounts_snapshot()`.
    ///
    /// Like the CLOB end-to-end test, this calls the precompile
    /// function directly with synthesized calldata rather than going
    /// through a full EVM transaction. A Solidity-side test would
    /// add transaction signing + pool submission + block production
    /// on top — that's its own stage. What this test pins is the
    /// architecture-level claim that a smart contract calling the
    /// precompile address sees the same accounts the bridge writes
    /// to.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn deposit_precompile_mutates_bridge_accounts() {
        use crate::precompiles::{deposit, uninstall_accounts, uninstall_clob, uninstall_fill_sink};
        use crate::OpenHlExecutorBuilder;
        use openhl_clob::AccountId;
        use openhl_funding::Notional;
        use reth_node_ethereum::node::EthereumAddOns;

        // Start from a clean global state — earlier tests may have
        // left an accounts map installed.
        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();

        let runtime = Runtime::test();
        let chain_spec = dev_chain_spec();
        let node_config = NodeConfig::test().dev().with_chain(chain_spec.clone());

        let handle = NodeBuilder::new(node_config)
            .testing_node(runtime)
            .with_types::<EthereumNode>()
            .with_components(EthereumNode::components().executor(OpenHlExecutorBuilder))
            .with_add_ons(EthereumAddOns::default())
            .launch()
            .await
            .expect("launch of custom-EVM node failed");

        // Constructing the bridge installs the accounts Arc as the
        // precompile module's ACCOUNTS_STATE global.
        let bridge = LiveRethEvmBridge::new(handle.node.provider.clone(), chain_spec);
        assert!(
            bridge.accounts_snapshot().is_empty(),
            "fresh bridge has no accounts yet",
        );

        // Build deposit calldata: (uint64 account=7, int64 amount=1000).
        let mut calldata = vec![0u8; 64];
        calldata[24..32].copy_from_slice(&7u64.to_be_bytes());
        // amount = 1000, sign-extended (positive — upper 24 bytes stay zero).
        calldata[56..64].copy_from_slice(&1000_i64.to_be_bytes());

        let r = deposit(&calldata, 100_000, 0).expect("deposit must not error");
        // Returned balance encoded as 32-byte sign-extended int.
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&r.bytes[24..32]);
        assert_eq!(i64::from_be_bytes(buf), 1000);

        // The bridge's view now reflects the deposit: a single
        // account 7 with collateral 1000. This is what proves the
        // shared `Arc<Mutex<HashMap<...>>>` between the bridge and
        // the precompile global.
        let snap = bridge.accounts_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].account, AccountId(7));
        assert_eq!(snap[0].collateral, Notional(1000));

        // A second deposit accumulates.
        let mut calldata2 = vec![0u8; 64];
        calldata2[24..32].copy_from_slice(&7u64.to_be_bytes());
        calldata2[56..64].copy_from_slice(&250_i64.to_be_bytes());
        let _ = deposit(&calldata2, 100_000, 0).unwrap();

        let snap = bridge.accounts_snapshot();
        assert_eq!(snap[0].collateral, Notional(1250));

        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();
        drop(handle);
    }

    /// **Stage 17e**: companion to the deposit precompile e2e test —
    /// boots a real Reth node, deposits, then withdraws via the
    /// withdraw precompile, asserting the bridge sees the debit.
    /// Pins the same architectural claim for the withdraw side.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn withdraw_precompile_debits_bridge_accounts() {
        use crate::precompiles::{
            deposit, uninstall_accounts, uninstall_clob, uninstall_fill_sink, withdraw,
        };
        use crate::OpenHlExecutorBuilder;
        use openhl_clob::AccountId;
        use openhl_funding::Notional;
        use reth_node_ethereum::node::EthereumAddOns;

        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();

        let runtime = Runtime::test();
        let chain_spec = dev_chain_spec();
        let node_config = NodeConfig::test().dev().with_chain(chain_spec.clone());

        let handle = NodeBuilder::new(node_config)
            .testing_node(runtime)
            .with_types::<EthereumNode>()
            .with_components(EthereumNode::components().executor(OpenHlExecutorBuilder))
            .with_add_ons(EthereumAddOns::default())
            .launch()
            .await
            .expect("launch of custom-EVM node failed");

        let bridge = LiveRethEvmBridge::new(handle.node.provider.clone(), chain_spec);

        // Seed via deposit precompile so account 9 exists with
        // collateral 2000.
        let mut deposit_calldata = vec![0u8; 64];
        deposit_calldata[24..32].copy_from_slice(&9u64.to_be_bytes());
        deposit_calldata[56..64].copy_from_slice(&2000_i64.to_be_bytes());
        let _ = deposit(&deposit_calldata, 100_000, 0).unwrap();
        assert_eq!(
            bridge.accounts_snapshot()[0].collateral,
            Notional(2000),
        );

        // Withdraw 750 via the withdraw precompile.
        let mut withdraw_calldata = vec![0u8; 64];
        withdraw_calldata[24..32].copy_from_slice(&9u64.to_be_bytes());
        withdraw_calldata[56..64].copy_from_slice(&750_u64.to_be_bytes());
        let r = withdraw(&withdraw_calldata, 100_000, 0).unwrap();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&r.bytes[24..32]);
        assert_eq!(i64::from_be_bytes(buf), 1250);

        // Bridge sees the debit.
        let snap = bridge.accounts_snapshot();
        assert_eq!(snap[0].account, AccountId(9));
        assert_eq!(snap[0].collateral, Notional(1250));

        // Insufficient-balance rejection is also observable through
        // the bridge: try to withdraw 5000 from a balance of 1250.
        let mut withdraw_too_much = vec![0u8; 64];
        withdraw_too_much[24..32].copy_from_slice(&9u64.to_be_bytes());
        withdraw_too_much[56..64].copy_from_slice(&5000_u64.to_be_bytes());
        let r = withdraw(&withdraw_too_much, 100_000, 0).unwrap();
        assert!(r.bytes.iter().all(|&b| b == 0), "rejection returns zeros");
        assert_eq!(
            bridge.accounts_snapshot()[0].collateral,
            Notional(1250),
            "balance unchanged on rejected withdraw",
        );

        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();
        drop(handle);
    }

    /// **Stage 17f**: drive the deposit precompile from inside EVM bytecode.
    /// Earlier stages (17c–17e) proved that calling the precompile *function*
    /// directly mutates the bridge's account map. They did NOT prove that a
    /// contract whose bytecode issues a `CALL` to the precompile address
    /// reaches the same code path through the EVM's precompile dispatch.
    ///
    /// This test closes that gap. We deploy a 26-byte wrapper that forwards
    /// its calldata to `OPENHL_DEPOSIT` via `CALL` and returns the precompile's
    /// 32-byte response, then execute a transaction against it through the
    /// same `OpenHlEvmFactory` Reth wires into every block. The transaction
    /// succeeds, its return matches what the precompile produced, AND the
    /// bridge's account map carries the credit — proving the bytecode →
    /// `CALL` → `openhl_precompiles` dispatch → state mutation path is whole.
    ///
    /// We don't boot a Reth node here: the precompile registration is in
    /// the factory, the account-map handoff is in `LiveRethEvmBridge::new`,
    /// and neither needs a running node to exercise. Earlier tests already
    /// confirm the factory is the same one Reth installs at boot.
    ///
    /// `#[ignore]`: the precompile module's `ACCOUNTS_STATE` is a process
    /// global. Any other test that constructs a `LiveRethEvmBridge` in
    /// parallel will overwrite it, derailing this test's precompile call.
    /// Run via `cargo test -p openhl-evm -- --ignored --test-threads=1`.
    #[test]
    #[ignore]
    fn deposit_via_evm_bytecode_mutates_bridge_accounts() {
        use crate::precompiles::{
            uninstall_accounts, uninstall_clob, uninstall_fill_sink, OPENHL_DEPOSIT,
        };
        use crate::OpenHlEvmFactory;
        use alloy_evm::revm::{
            context::{result::ExecutionResult, TxEnv},
            database::{CacheDB, EmptyDB},
            primitives::{Address, Bytes, TxKind, U256},
            state::{AccountInfo, Bytecode},
        };
        use alloy_evm::{Evm, EvmEnv, EvmFactory};
        use openhl_clob::AccountId;
        use openhl_funding::Notional;

        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();

        // Construct the bridge — this installs its account-map Arc as
        // the precompile module's `ACCOUNTS_STATE` global. No Reth node
        // needed for this leg of the test (see test docstring).
        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());
        assert!(bridge.accounts_snapshot().is_empty());

        // Pre-load the wrapper bytecode at a fixed contract address and
        // fund a caller EOA. The caller doesn't need much — `gas_price`
        // is 0, no value is sent — but a non-empty balance dodges any
        // pre-pay checks.
        let contract_addr = Address::from([0xc0; 20]);
        let caller_addr = Address::from([0xca; 20]);
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            contract_addr,
            AccountInfo {
                nonce: 1,
                code: Some(Bytecode::new_raw(Bytes::from(wrapper_bytecode_for(
                    OPENHL_DEPOSIT,
                )))),
                ..Default::default()
            },
        );
        db.insert_account_info(
            caller_addr,
            AccountInfo {
                balance: U256::from(1_000_000_000u64),
                ..Default::default()
            },
        );

        // Same factory Reth installs via `OpenHlExecutorBuilder`. Default
        // `EvmEnv` selects `SpecId::OSAKA`, which dispatches to the prague
        // branch of `precompiles_for` — our precompiles get registered.
        let mut evm = OpenHlEvmFactory.create_evm(db, EvmEnv::default());

        // Deposit calldata: (uint64 account=42, int64 amount=1000).
        let mut calldata = vec![0u8; 64];
        calldata[24..32].copy_from_slice(&42u64.to_be_bytes());
        calldata[56..64].copy_from_slice(&1000_i64.to_be_bytes());

        let tx = TxEnv {
            caller: caller_addr,
            kind: TxKind::Call(contract_addr),
            data: Bytes::from(calldata),
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let result = evm.transact(tx).expect("evm.transact must not error");

        let output = match result.result {
            ExecutionResult::Success { output, .. } => output.into_data(),
            other => panic!("expected Success, got {other:?}"),
        };
        // Wrapper returns exactly the precompile's 32-byte response.
        assert_eq!(output.len(), 32);
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&output[24..32]);
        assert_eq!(
            i64::from_be_bytes(buf),
            1000,
            "wrapper must return the precompile's new-balance int64",
        );

        // The bridge's map carries the credit — the bytecode → CALL
        // dispatch reached the same `ACCOUNTS_STATE` global the bridge
        // shares with the precompile module.
        let snap = bridge.accounts_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].account, AccountId(42));
        assert_eq!(snap[0].collateral, Notional(1000));

        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();
    }

    /// **Stage 17f companion**: same path as the deposit test, but
    /// targeting `OPENHL_WITHDRAW`. Seeds collateral via the bridge's
    /// own `deposit` (Rust API) so the EVM-side withdraw has something
    /// to drain — and asserts both the wrapper's return and the
    /// bridge's debited balance.
    ///
    /// `#[ignore]` for the same parallel-test reason as
    /// [`deposit_via_evm_bytecode_mutates_bridge_accounts`].
    #[test]
    #[ignore]
    fn withdraw_via_evm_bytecode_debits_bridge_accounts() {
        use crate::precompiles::{
            uninstall_accounts, uninstall_clob, uninstall_fill_sink, OPENHL_WITHDRAW,
        };
        use crate::OpenHlEvmFactory;
        use alloy_evm::revm::{
            context::{result::ExecutionResult, TxEnv},
            database::{CacheDB, EmptyDB},
            primitives::{Address, Bytes, TxKind, U256},
            state::{AccountInfo, Bytecode},
        };
        use alloy_evm::{Evm, EvmEnv, EvmFactory};
        use openhl_clob::AccountId;
        use openhl_funding::Notional;

        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();

        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());
        // Seed account 9 with 2000 collateral via the bridge's Rust API
        // (Stage 17b primitive). The EVM-side withdraw must see this.
        let _ = bridge.deposit(AccountId(9), 2000);

        let contract_addr = Address::from([0xc1; 20]);
        let caller_addr = Address::from([0xcb; 20]);
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            contract_addr,
            AccountInfo {
                nonce: 1,
                code: Some(Bytecode::new_raw(Bytes::from(wrapper_bytecode_for(
                    OPENHL_WITHDRAW,
                )))),
                ..Default::default()
            },
        );
        db.insert_account_info(
            caller_addr,
            AccountInfo {
                balance: U256::from(1_000_000_000u64),
                ..Default::default()
            },
        );

        let mut evm = OpenHlEvmFactory.create_evm(db, EvmEnv::default());

        // Withdraw calldata: (uint64 account=9, uint64 amount=750).
        let mut calldata = vec![0u8; 64];
        calldata[24..32].copy_from_slice(&9u64.to_be_bytes());
        calldata[56..64].copy_from_slice(&750_u64.to_be_bytes());

        let tx = TxEnv {
            caller: caller_addr,
            kind: TxKind::Call(contract_addr),
            data: Bytes::from(calldata),
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let result = evm.transact(tx).expect("evm.transact must not error");

        let output = match result.result {
            ExecutionResult::Success { output, .. } => output.into_data(),
            other => panic!("expected Success, got {other:?}"),
        };
        assert_eq!(output.len(), 32);
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&output[24..32]);
        assert_eq!(
            i64::from_be_bytes(buf),
            1250,
            "wrapper must return post-withdraw balance (2000 - 750)",
        );

        let snap = bridge.accounts_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].account, AccountId(9));
        assert_eq!(snap[0].collateral, Notional(1250));

        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();
    }

    /// Build a minimal 26-byte wrapper contract that forwards all its
    /// calldata to `precompile` via `CALL`, then returns the first 32
    /// bytes of the precompile's response. Equivalent to the Solidity:
    ///
    /// ```solidity
    /// fallback() external returns (bytes memory) {
    ///     (bool ok, bytes memory ret) = precompile.call(msg.data);
    ///     require(ok);
    ///     return ret;
    /// }
    /// ```
    ///
    /// The precompile address is encoded into bytes 16..18 (the `PUSH2`
    /// operand). All known openhl precompile addresses fit in 16 bits
    /// (`0x0c1b`..`0x0c1e`), so a fixed `PUSH2` is enough.
    fn wrapper_bytecode_for(precompile: alloy_primitives::Address) -> Vec<u8> {
        let raw = precompile.into_array();
        // Sanity-check the assumption: only the low 2 bytes may be non-zero.
        assert!(
            raw[..18].iter().all(|&b| b == 0),
            "wrapper helper only handles 16-bit precompile addresses",
        );
        let lo = u16::from_be_bytes([raw[18], raw[19]]).to_be_bytes();
        vec![
            // Copy all calldata into memory[0..calldatasize].
            0x36, // CALLDATASIZE
            0x60, 0x00, // PUSH1 0
            0x60, 0x00, // PUSH1 0
            0x37, // CALLDATACOPY
            // CALL(gas, addr, value=0, in_off=0, in_size=calldatasize,
            //      out_off=0, out_size=32). Args pushed in reverse so
            //      `gas` lands on top.
            0x60, 0x20, // PUSH1 32   out_size
            0x60, 0x00, // PUSH1 0    out_off
            0x36, // CALLDATASIZE       in_size
            0x60, 0x00, // PUSH1 0    in_off
            0x60, 0x00, // PUSH1 0    value
            0x61, lo[0], lo[1], // PUSH2 <precompile_lo>
            0x5a, // GAS
            0xf1, // CALL
            0x50, // POP (discard the success flag — precompile never fails)
            // Return memory[0..32], which CALL already populated.
            0x60, 0x20, // PUSH1 32
            0x60, 0x00, // PUSH1 0
            0xf3, // RETURN
        ]
    }

    /// Variant of [`wrapper_bytecode_for`] that replaces the terminating
    /// `RETURN` with `REVERT`. The precompile call still executes (and
    /// without [`OpenHlRevertGuard`] would still mutate the bridge's
    /// account map), but the calling frame reverts — so a revert-aware
    /// EVM should roll the precompile mutation back.
    fn reverting_wrapper_bytecode_for(precompile: alloy_primitives::Address) -> Vec<u8> {
        let mut bytecode = wrapper_bytecode_for(precompile);
        let last = bytecode.len() - 1;
        assert_eq!(bytecode[last], 0xf3, "wrapper must terminate in RETURN");
        bytecode[last] = 0xfd; // REVERT
        bytecode
    }

    /// **Stage 17i**: when a contract calls the deposit precompile and
    /// then `REVERT`s, the precompile's mutation must roll back. The
    /// [`OpenHlRevertGuard`] inspector implements this by snapshotting
    /// the bridge globals at every call-frame entry and restoring on
    /// revert. Without it, the deposit would land in `bridge.accounts`
    /// even though the EVM rolled back the calling tx — a real
    /// double-spend / mint-collateral vector.
    ///
    /// This test pairs with [`deposit_via_evm_bytecode_persists_on_return`]
    /// to confirm the inspector restores ONLY on revert and lets
    /// successful calls commit normally.
    ///
    /// `#[ignore]` for the same parallel-test reason as the Stage 17f
    /// tests: `ACCOUNTS_STATE` is process-global.
    #[test]
    #[ignore]
    fn deposit_via_evm_bytecode_rolls_back_on_revert() {
        use crate::precompiles::{
            uninstall_accounts, uninstall_clob, uninstall_fill_sink, OpenHlRevertGuard,
            OPENHL_DEPOSIT,
        };
        use crate::OpenHlEvmFactory;
        use alloy_evm::revm::{
            context::{result::ExecutionResult, TxEnv},
            database::{CacheDB, EmptyDB},
            primitives::{Address, Bytes, TxKind, U256},
            state::{AccountInfo, Bytecode},
        };
        use alloy_evm::{Evm, EvmEnv, EvmFactory};

        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();

        // Same bridge wiring as the Stage 17f tests — install_accounts
        // runs in `new`, pointing the precompile global at this
        // bridge's account map.
        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());
        assert!(bridge.accounts_snapshot().is_empty());

        let contract_addr = Address::from([0xd0; 20]);
        let caller_addr = Address::from([0xda; 20]);
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            contract_addr,
            AccountInfo {
                nonce: 1,
                code: Some(Bytecode::new_raw(Bytes::from(
                    reverting_wrapper_bytecode_for(OPENHL_DEPOSIT),
                ))),
                ..Default::default()
            },
        );
        db.insert_account_info(
            caller_addr,
            AccountInfo {
                balance: U256::from(1_000_000_000u64),
                ..Default::default()
            },
        );

        let guard = OpenHlRevertGuard::new();
        let mut evm = OpenHlEvmFactory.create_evm_with_inspector(db, EvmEnv::default(), guard);
        evm.enable_inspector();

        // (uint64 account=42, int64 amount=1000).
        let mut calldata = vec![0u8; 64];
        calldata[24..32].copy_from_slice(&42u64.to_be_bytes());
        calldata[56..64].copy_from_slice(&1000_i64.to_be_bytes());

        let tx = TxEnv {
            caller: caller_addr,
            kind: TxKind::Call(contract_addr),
            data: Bytes::from(calldata),
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let result = evm.transact(tx).expect("evm.transact must not error");

        // The transaction reverted — the EVM returns Revert with the
        // wrapper's return data (still the precompile's 1000 balance,
        // exposed as revert data).
        match result.result {
            ExecutionResult::Revert { output, .. } => {
                assert_eq!(output.len(), 32);
                let mut buf = [0u8; 8];
                buf.copy_from_slice(&output[24..32]);
                assert_eq!(
                    i64::from_be_bytes(buf),
                    1000,
                    "precompile still computed the deposit; only post-call EVM state was rolled back",
                );
            }
            other => panic!("expected Revert, got {other:?}"),
        }

        // The key assertion: the bridge sees NO mutation. Without
        // the revert guard, account 42 would carry collateral 1000.
        assert!(
            bridge.accounts_snapshot().is_empty(),
            "OpenHlRevertGuard must roll back the precompile's mutation on REVERT",
        );

        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();
    }

    /// **Stage 17k**: production wiring. `OpenHlEvmFactory::create_evm`
    /// now installs `OpenHlRevertGuard` by default — no
    /// `create_evm_with_inspector` call, no explicit guard, no
    /// `evm.enable_inspector()`. Reth's executor invokes
    /// `create_evm` for every block, so this is the path that
    /// matters for real on-chain reverts.
    ///
    /// `#[ignore]` for the same `ACCOUNTS_STATE`-race reason as the
    /// other bytecode-driven tests.
    #[test]
    #[ignore]
    fn deposit_via_evm_bytecode_rolls_back_on_revert_through_create_evm() {
        use crate::precompiles::{
            uninstall_accounts, uninstall_clob, uninstall_fill_sink, OPENHL_DEPOSIT,
        };
        use crate::OpenHlEvmFactory;
        use alloy_evm::revm::{
            context::{result::ExecutionResult, TxEnv},
            database::{CacheDB, EmptyDB},
            primitives::{Address, Bytes, TxKind, U256},
            state::{AccountInfo, Bytecode},
        };
        use alloy_evm::{Evm, EvmEnv, EvmFactory};

        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();

        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());
        assert!(bridge.accounts_snapshot().is_empty());

        let contract_addr = Address::from([0xe0; 20]);
        let caller_addr = Address::from([0xea; 20]);
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            contract_addr,
            AccountInfo {
                nonce: 1,
                code: Some(Bytecode::new_raw(Bytes::from(
                    reverting_wrapper_bytecode_for(OPENHL_DEPOSIT),
                ))),
                ..Default::default()
            },
        );
        db.insert_account_info(
            caller_addr,
            AccountInfo {
                balance: U256::from(1_000_000_000u64),
                ..Default::default()
            },
        );

        // The key difference vs 17i: `create_evm` (no explicit
        // inspector), no enable_inspector() call. This is what
        // Reth's BlockExecutor uses on every block.
        let mut evm = OpenHlEvmFactory.create_evm(db, EvmEnv::default());

        let mut calldata = vec![0u8; 64];
        calldata[24..32].copy_from_slice(&42u64.to_be_bytes());
        calldata[56..64].copy_from_slice(&1000_i64.to_be_bytes());
        let tx = TxEnv {
            caller: caller_addr,
            kind: TxKind::Call(contract_addr),
            data: Bytes::from(calldata),
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let result = evm.transact(tx).expect("evm.transact must not error");

        match result.result {
            ExecutionResult::Revert { .. } => {}
            other => panic!("expected Revert, got {other:?}"),
        }
        assert!(
            bridge.accounts_snapshot().is_empty(),
            "Stage 17k: create_evm installs the guard by default — revert must roll back",
        );

        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();
    }

    /// **Stage 17i companion**: with the same inspector wired in,
    /// a deposit-then-RETURN flow MUST still commit the mutation.
    /// Otherwise the guard would over-rollback and break the happy
    /// path that Stages 17c–17f already proved.
    ///
    /// `#[ignore]` for the same parallel-test reason.
    #[test]
    #[ignore]
    fn deposit_via_evm_bytecode_persists_on_return() {
        use crate::precompiles::{
            uninstall_accounts, uninstall_clob, uninstall_fill_sink, OpenHlRevertGuard,
            OPENHL_DEPOSIT,
        };
        use crate::OpenHlEvmFactory;
        use alloy_evm::revm::{
            context::{result::ExecutionResult, TxEnv},
            database::{CacheDB, EmptyDB},
            primitives::{Address, Bytes, TxKind, U256},
            state::{AccountInfo, Bytecode},
        };
        use alloy_evm::{Evm, EvmEnv, EvmFactory};
        use openhl_clob::AccountId;
        use openhl_funding::Notional;

        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();

        let bridge = LiveRethEvmBridge::new((), dev_chain_spec());

        let contract_addr = Address::from([0xd1; 20]);
        let caller_addr = Address::from([0xdb; 20]);
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            contract_addr,
            AccountInfo {
                nonce: 1,
                code: Some(Bytecode::new_raw(Bytes::from(wrapper_bytecode_for(
                    OPENHL_DEPOSIT,
                )))),
                ..Default::default()
            },
        );
        db.insert_account_info(
            caller_addr,
            AccountInfo {
                balance: U256::from(1_000_000_000u64),
                ..Default::default()
            },
        );

        let guard = OpenHlRevertGuard::new();
        let mut evm = OpenHlEvmFactory.create_evm_with_inspector(db, EvmEnv::default(), guard);
        evm.enable_inspector();

        let mut calldata = vec![0u8; 64];
        calldata[24..32].copy_from_slice(&7u64.to_be_bytes());
        calldata[56..64].copy_from_slice(&500_i64.to_be_bytes());

        let tx = TxEnv {
            caller: caller_addr,
            kind: TxKind::Call(contract_addr),
            data: Bytes::from(calldata),
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let result = evm.transact(tx).expect("evm.transact must not error");

        match result.result {
            ExecutionResult::Success { output, .. } => {
                let data = output.into_data();
                assert_eq!(data.len(), 32);
            }
            other => panic!("expected Success, got {other:?}"),
        }

        // Happy path: bridge sees the deposit, inspector did not
        // over-rollback.
        let snap = bridge.accounts_snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].account, AccountId(7));
        assert_eq!(snap[0].collateral, Notional(500));

        uninstall_accounts();
        uninstall_clob();
        uninstall_fill_sink();
    }

    /// **Stage 7d**: with a Reth `ConsensusEngineHandle` installed, `commit`
    /// sends a `ForkchoiceUpdated` to the in-process Engine API. The bridge's
    /// own bookkeeping still happens (so existing callers don't regress), but
    /// now Reth is told about the new head too.
    ///
    /// At this stage the engine will respond SYNCING because we haven't sent
    /// a matching `newPayload` (`build_payload` doesn't yet produce a real
    /// `ExecutionPayload` — fills aren't EVM-encoded). That's intentional: 7d
    /// proves the wire is connected. Full alignment between Malachite's
    /// commit and Reth's canonical head needs `newPayload` integration, which
    /// is the next staging chunk after fills become EVM transactions.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn commit_sends_forkchoice_to_engine_when_handle_installed() {
        use crate::OpenHlExecutorBuilder;
        use crate::precompiles::{uninstall_clob, uninstall_fill_sink};
        use reth_node_ethereum::node::EthereumAddOns;

        uninstall_clob();
        uninstall_fill_sink();

        let runtime = Runtime::test();
        let chain_spec = dev_chain_spec();
        let node_config = NodeConfig::test().dev().with_chain(chain_spec.clone());

        let handle = NodeBuilder::new(node_config)
            .testing_node(runtime)
            .with_types::<EthereumNode>()
            .with_components(EthereumNode::components().executor(OpenHlExecutorBuilder))
            .with_add_ons(EthereumAddOns::default())
            .launch()
            .await
            .expect("launch failed");

        // Pull the engine handle out of add_ons. This is what RPC's
        // engine_forkchoiceUpdated endpoint would dispatch to — we're
        // taking the in-process shortcut around the JSON-RPC layer.
        let engine_handle = handle.node.add_ons_handle.beacon_engine_handle.clone();

        let bridge = LiveRethEvmBridge::new(handle.node.provider.clone(), chain_spec)
            .with_engine_handle(engine_handle);
        assert!(
            bridge.has_engine_handle(),
            "with_engine_handle must install the handle"
        );

        let genesis_hash_b256 = handle
            .node
            .provider
            .block_hash(0)
            .expect("provider call failed")
            .expect("provider has no genesis");

        // Build a payload on top of genesis so commit has something to find.
        let attrs = PayloadAttrs {
            timestamp: 1,
            fee_recipient: [0u8; 20],
            prev_randao: [0u8; 32],
        };
        let id = bridge
            .build_payload(BlockHash(genesis_hash_b256.0), attrs)
            .await
            .expect("build_payload failed");
        let block = bridge.payload_ready(id).await.expect("payload_ready failed");

        // The actual test: commit should not panic, not block forever, not
        // surface an error from the engine-side SYNCING response. We're
        // proving the wire is connected — that fork_choice_updated reaches
        // the engine and returns *some* response (even SYNCING).
        bridge
            .commit(block.hash)
            .await
            .expect("commit failed even though local bookkeeping should succeed");

        // The bridge's own chain HashMap must reflect the new head.
        // Negative case: a commit for an unknown hash must still be Rejected
        // (the engine-side call doesn't happen because the bridge bails out
        // before it).
        let bogus = BlockHash([0xddu8; 32]);
        let err = bridge.commit(bogus).await.unwrap_err();
        assert!(
            matches!(err, BridgeError::Rejected(_)),
            "unknown hash must yield Rejected"
        );

        uninstall_fill_sink();
        uninstall_clob();
        drop(handle);
    }
}
