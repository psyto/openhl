# openhl architecture

## Subsystems

openhl is a single Rust binary composed of two cleanly-separated halves:

- **Consensus layer (CL)** — Malachite BFT, wired through `crates/consensus`. Owns leader election, voting, view changes, finality.
- **Execution layer (EL)** — Reth as a library, wired through `crates/evm`. Owns state, EVM execution, payload building, mempool.

Plus four pure state-machine subsystems that the EL composes:

- **CLOB** (`crates/clob`) — orderbook matching engine. Pure, deterministic, replayable.
- **Settlement** (`crates/funding`, `crates/oracle`, `crates/liquidation`) — funding rates, mark prices, liquidations. `funding` (Stage 8b), `liquidation` (10a margin math → 10b insurance fund → 10c multi-account scanner → 10d ADL), and `oracle` (11 aggregation → 11b signed observations) are all complete; each runs deterministically per block via the integration coordinator (Stages 14a–15e).
- **Vault** (`crates/vault`) — protocol-native vault primitive for strategy products. Shipped at Stage 12 (share-based collateral pooling); marked-to-market per block (Stage 14a).
- **Clearing** (`crates/clearing`) — per-account position bookkeeping. `apply_fill(account, price, qty, side)` updates `(position_size, avg_entry)` and returns realized PnL across the open/increase/partial-close/flip cases (Stage 16a). The bridge owns the `HashMap<AccountId, Account>` and routes every CLOB fill through `apply_fill` (Stage 16b); accounts are produced by real fills (Stage 17a) and persisted in the bridge snapshot. Free-collateral math (`free_collateral(acct, mark, im_bps)` = `(collateral + uPnL) − |size| × mark × im_bps / 10⁴`) is mark-aware as of Stage 17j; both the bridge's `withdraw` and the `openhl_withdraw` precompile route through the same helper.
- **Integration coordinator** (`crates/node` — `OpenHlNode::tick`) — composes the pure subsystems above into one deterministic per-block routine: oracle refresh → liquidation scan → ADL absorption → vault mark-to-market → funding settlement. Driven from `LiveRethEvmBridge`'s commit path in `bin/openhl reth-devnet` (Stages 14a–15e); produces a `TickReport` whose fields the bridge applies back to per-account state.

### Collateral flow

Collateral enters and leaves accounts through `deposit`/`withdraw`, exposed two ways (Stages 17b–17e):

- **Bridge methods** — `LiveRethEvmBridge::deposit(account, amount: i64)` (signed, no balance check) and `withdraw(account, amount: u64) -> Option<Notional>` (margin-aware). Used by `bin/openhl` to seed demo collateral and by RPC clients via the bridge cell.
- **EVM precompiles** — `openhl_deposit` at `0x…0c1d` and `openhl_withdraw` at `0x…0c1e`, alongside the two CLOB precompiles (`clob_read_best_bid` at `0x…0c1b`, `clob_place_order` at `0x…0c1c`). They mutate the same `Arc<Mutex<HashMap<AccountId, Account>>>` the bridge owns, shared via the precompile module's install globals — so an EVM-side deposit and a Rust-side bridge deposit are the same state change.

#### Mark-aware free collateral (Stage 17j)

Both withdraw paths share one helper. When the CLOB has both a bid and an ask, the midpoint serves as the mark and the production-shape rule applies:

```
free = (collateral + unrealized_pnl) − |size| × mark × im_bps / 10⁴
```

With a one-sided book the rule falls back to IM at `avg_entry` (conservative). Flat accounts collapse to the raw-collateral check.

#### Tunable margin params (Stages 17l → 17m)

`LiveRethEvmBridge::with_liquidation_params(params)` stores the full `LiquidationParams` (initial / maintenance / fee bps) on the bridge AND installs `params.initial_margin_bps` + `params.maintenance_margin_bps` into precompile-module globals so the EVM-side reads exactly what the bridge enforces. `bin/openhl` plumbs it from `OpenHlNodeConfig::liquidation_params` at boot; tests use `LiquidationParams::hyperliquid_default()` (1000 bps initial, 200 bps maintenance, 150 bps fee).

#### Oracle index as mark (Stages 17o + 17p)

Margin / withdraw / margin_health all consult `bridge.effective_mark()` (Rust) / `precompiles::effective_mark()` (EVM) which prefers an installed oracle index over the CLOB midpoint. `bin/openhl` pushes `coordinator.oracle().current_price()` into the bridge after every `coordinator.tick`; the bridge's setter installs the same value into the precompile global, so on-chain and off-chain reads stay in lockstep. Before the first successful oracle refresh — or after any tick where the deviation filter failed quorum — there's no installed index and consumers fall back to the CLOB midpoint.

Stage 17p extends the same rule to the integration coordinator: `OpenHlNode::tick` now reads `self.oracle.current_price()` as the mark for the liquidation scanner and ADL (with `input.mark` as the same fallback), so the on-chain ADL engine and the bridge's `margin_health` accessor agree on which accounts are at risk. The CLOB midpoint (`input.mark`) is still the input to the funding rate's `premium = mark − index` — using oracle for both sides would collapse the premium to zero.

Stage 17q adds a freshness check. `OracleParams::aggregate_max_age_secs` (default 60s) caps how old the cached aggregate is allowed to be relative to `input.block_time`; the new `OracleState::current_price_fresh_at` returns `None` past the window. Scan / ADL / funding all gate through this accessor, and `bin/openhl`'s tick hook installs OR clears the bridge's oracle cell based on the same check — so a stalled publisher set degrades cleanly to the CLOB midpoint everywhere rather than letting an aging aggregate delay liquidations or fix the funding premium.

#### Queryable margin health (Stages 17m–17n + 19a)

Three surfaces, all returning the same production-shape `Safe / AtRisk / Liquidatable / Underwater` classification computed by `openhl-liquidation::margin_health` at the current CLOB midpoint:

- **Rust** — `bridge.margin_health(account) -> Option<MarginHealth>` (Stage 17m).
- **EVM precompile** — `openhl_margin_health` at `0x…0c1f` (Stage 17n). Calldata: single `uint64` account id. Return: 32-byte word whose last byte is the discriminator (`0` Indeterminate / `1` Safe / `2` AtRisk / `3` Liquidatable / `4` Underwater).
- **JSON-RPC** — `openhl_marginHealth(account)` (Stage 19a). Plus `openhl_currentMark`, `openhl_accounts`, `openhl_accountSnapshot`, `openhl_liquidationParams` for the rest of the accessor surface. See the README's RPC section.

#### Revert-safe precompile mutations (Stages 17i + 17k)

`OpenHlEvmFactory::create_evm` returns an `OpenHlEvm<DB, I, P>` wrapper that internally composes the user-facing inspector with `OpenHlRevertGuard` via REVM's `Inspector for (L, R)` tuple impl. The guard snapshots `{accounts, CLOB book, pending_fills}` on every call-frame entry and restores on revert — a contract that calls `openhl_deposit` and then `REVERT`s no longer mints collateral. The wrapper presents `Inspector = I` to satisfy `EvmFactory`'s GAT bound; users can still pass any inspector via `create_evm_with_inspector` and it runs alongside the guard.

## The CL/EL contract

The boundary between consensus and execution is six messages, defined as the `ConsensusBridge` trait in `crates/consensus/src/bridge.rs`:

| Direction | Message | Promise |
| :--- | :--- | :--- |
| CL → EL | `build_payload(parent, attrs)` | "Build me a candidate block on top of `parent`." |
| EL → CL | `payload_ready(block)` | "Here is the assembled block." |
| CL → EL | `validate_payload(block)` | "Would this block execute cleanly?" |
| CL → EL | `commit(block_hash)` | "Finalize this block. Update fork-choice." |
| CL → EL | `encode_proposed_block(id)` | "Serialise the block I just built for cross-validator transport." (Stage 18a) |
| CL → EL | `register_proposed_block(bytes)` | "Decode + install a block another validator built, so my next `commit` finds it." (Stage 18a) |

The last two pair the consensus loop's `GetValue` (proposer) and `ReceivedProposalPart` (follower) handlers with the bridge so follower-side replication no longer relies on the Stage 13n deterministic-recompute trick. Every interaction between CL and EL flows through these six; anything else is a contract leak.

## The pure / I/O split

| Crate group | I/O? | Tested how |
| :--- | :--- | :--- |
| `types`, `codec`, `clob`, `funding`, `liquidation`, `vault`, `oracle`, `clearing` | No | Unit tests + proptest, microseconds per case |
| `evm`, `consensus`, `node` | Yes | Integration tests, devnet replay |

The pure crates do not depend on tokio, networking, disk, or system time. This is enforced by `unsafe_code = "forbid"` plus dependency-policy review.

## Determinism rules

State changes happen exclusively inside the pure crates. The I/O crates may only:

1. Receive an event from the network or disk.
2. Call into the pure crates with that event as input.
3. Persist or broadcast the result.

The pure crates never call `SystemTime::now`, `HashMap` iteration order, `rand`, or any operation whose output depends on host state. Determinism is the only reason multiple validators converge on the same state root; one violation forks the chain.

## ADRs

`docs/adr/` is reserved for Architecture Decision Records that need their own dated, stable artifact (think: chain reorg semantics, validator-set rotation, fee market). The directory is empty today — design notes through Stage 19a live in commit messages and this document, which has been adequate so far. Add an ADR when a decision is (a) load-bearing, (b) likely to be revisited, and (c) hard to reconstruct from the diff alone. ADRs are never edited after acceptance — supersede with a new one instead.
