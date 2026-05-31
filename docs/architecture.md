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

- **Bridge methods** — `LiveRethEvmBridge::deposit(account, amount: i64)` (signed, no balance check) and `withdraw(account, amount: u64) -> Option<Notional>` (margin-aware). Used by `bin/openhl` to seed demo collateral.
- **EVM precompiles** — `openhl_deposit` at `0x…0c1d` and `openhl_withdraw` at `0x…0c1e`, alongside the two CLOB precompiles (`clob_read_best_bid` at `0x…0c1b`, `clob_place_order` at `0x…0c1c`). They mutate the same `Arc<Mutex<HashMap<AccountId, Account>>>` the bridge owns, shared via the precompile module's install globals — so an EVM-side deposit and a Rust-side bridge deposit are the same state change.

The withdraw rule (bridge + precompile, identical math) is **mark-aware free collateral** as of Stage 17j: when the CLOB has both a bid and an ask, `free = (collateral + uPnL) − |size| × mark × im_bps / 10⁴`; with a one-sided book it falls back to IM at `avg_entry`. Stage 17l made `im_bps` runtime-tunable: `LiveRethEvmBridge::with_initial_margin_bps(bps)` stores the rate on the bridge AND installs it into a precompile-module global so the EVM-side withdraw reads the same value. `bin/openhl` plumbs it from `OpenHlNodeConfig::liquidation_params::initial_margin_bps` at boot; tests use `DEFAULT_INITIAL_MARGIN_BPS = 1000` (matches `LiquidationParams::hyperliquid_default`).

The precompiles are **revert-aware** as of Stage 17k: `OpenHlEvmFactory::create_evm` returns an `OpenHlEvm<DB, I, P>` wrapper that internally composes the user-facing inspector with `OpenHlRevertGuard` via REVM's `Inspector for (L, R)` tuple impl. The guard snapshots `{accounts, CLOB book, pending_fills}` on every call-frame entry and restores on revert — a contract that calls `openhl_deposit` and then `REVERT`s no longer mints collateral. The wrapper presents `Inspector = I` to satisfy `EvmFactory`'s GAT bound; users can still pass any inspector via `create_evm_with_inspector` and it runs alongside the guard.

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

Significant design decisions are recorded as ADRs under `docs/adr/`. Each ADR is dated, stable, and never edited after acceptance — supersede with a new ADR instead.
