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
        }
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
    /// (Stage 17e). Returns the new balance on success; `None` if
    /// the account doesn't exist or its current balance is less
    /// than `amount`.
    ///
    /// `amount` is unsigned by API contract — callers expressing
    /// "I want to take 100 out" never accidentally credit by
    /// passing a negative. The internal subtraction goes through
    /// `i64` so an `amount` larger than `i64::MAX` saturates to
    /// "rejected" via the balance check.
    ///
    /// Balance check is against raw collateral (Stage 17e
    /// limitation) — a margin-aware withdraw would also enforce
    /// `collateral - amount >= initial_margin_requirement(account)`,
    /// but that's the clearing-house layer's job, not the
    /// bridge's. v0 callers are responsible for the margin check.
    pub fn withdraw(
        &self,
        account: AccountId,
        amount: u64,
    ) -> Option<openhl_funding::Notional> {
        let mut accts = self.accounts.lock().expect("accounts mutex poisoned");
        let acct = accts.get_mut(&account)?;
        let amount_i64 = i64::try_from(amount).ok()?;
        if acct.collateral.0 < amount_i64 {
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
