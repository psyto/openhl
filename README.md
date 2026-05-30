# openhl

An open-source reference implementation of a Hyperliquid-shape L1: BFT consensus + EVM execution + a CLOB matching engine, with first-class vault primitives.

**Status:** Modules 1–5 shipped at the state-machine level, the **live per-block integration cascade runs end-to-end** across two validators, and a **clearing layer** now drives per-account position state from real CLOB fills. A two-validator BFT devnet (Stages 13l–13n) commits matching block hashes over libp2p; on every committed block the integration coordinator drives oracle aggregation, liquidation scan, ADL absorption, vault mark-to-market, and funding settlement (Stages 14a–15e). Account positions are produced by actual fills routed through `openhl-clearing::apply_fill` and owned by the bridge (Stages 16a–17a); collateral moves through `deposit`/`withdraw` primitives exposed both as bridge methods and as EVM precompiles (Stages 17b–17e). Funding settlements adjust collateral, liquidation/ADL close out unhealthy positions, the safety net converges to a resolved state — all deterministic, all byte-identical between validators, all persistent across restart. See the build arc below.

## Why

Hyperliquid's protocol stack (HyperBFT consensus, HyperCore matching engine, HyperEVM execution) is closed source. `openhl` is the open reference implementation: a working Rust workspace anyone can read, fork, and extend. The goal is not to compete with HL — it's to give the ecosystem a public substrate that HL-shape apps can deploy onto, and a teachable codebase for engineers who want to understand how this class of L1 actually works.

## Architecture

Six subsystems, eleven library crates plus the node binary. The split is deliberately load-bearing: pure state machines (clob, funding, vault, clearing) are I/O-free and deterministic; the I/O boundary (evm, consensus, node) talks to the outside world and calls into the pure crates.

```
bin/openhl/                          thin binary, calls crates/node
crates/
├── types/         shared primitives (Asset, Price, Qty, AccountId)
├── codec/         canonical encoding
├── clob/          Module 2 — orderbook state machine
├── oracle/                   mark price aggregation
├── funding/       Module 4 — funding-rate calc + settlement
├── liquidation/              liquidation engine
├── vault/         Module 5 — protocol-native vault primitive
├── clearing/      Module 6 — per-account position bookkeeping (apply_fill)
├── evm/           Module 3 — Reth integration + core↔EVM precompiles
├── consensus/     Module 1 — Malachite BFT app-side wiring
└── node/                     assembles consensus + evm + clob into Node::run()
```

See [`docs/architecture.md`](docs/architecture.md) for the full design, and `docs/adr/` for individual decisions as they land.

## Build arc

`openhl` is built incrementally as the worked example for the [rethlab](https://rethlab.com) L1 Architect tier. Each module ships working code here and matching lessons there:

| # | Module | Crates touched | Status |
| - | --- | --- | --- |
| 1 | Consensus substrate (Malachite + Reth) | `consensus`, `evm`, `node` | ✅ Stage 6 → 7d (single-validator); Stages 13l–13n add two-validator BFT |
| 2 | CLOB matching engine | `clob`, `types`, `codec` | ✅ Stage 8a + 8d |
| 3 | Core ↔ EVM precompiles | `evm`, `clob` | ✅ Stage 9a–9e + 9c+ + 9d |
| 4 | Funding, oracle, liquidations | `funding`, `oracle`, `liquidation` | ✅ Stage 8b (funding) + 10a–10d (liquidation margin, insurance fund, scanner, ADL) + 11–11b (oracle aggregation + signed observations); driven per-block via Stages 14a–15d |
| 5 | Protocol-native vault primitive | `vault` | ✅ Stage 12 (share-based collateral pooling); marked-to-market per block via Stage 14a |
| 6 | Clearing layer (positions + collateral) | `clearing`, `evm` | ✅ Stage 16a–16d (apply_fill + bridge-owned accounts) + 17a (real fills create accounts) + 17b–17e (deposit/withdraw primitives + EVM precompiles) |

v0 milestone: single-validator devnet produces blocks end-to-end. **Achieved** at the end of Module 1 / Stage 7d.

Two-validator BFT milestone: two `openhl reth-devnet` processes reach consensus over libp2p and commit matching block hashes with identical bridge state. **Achieved** at Stage 13n. See [`docs/testing.md`](docs/testing.md) for the manual bring-up procedure (including restart resilience).

v1 milestone: per-block integration cascade runs across both validators — oracle aggregation → liquidation scan → ADL → vault mark-to-market → funding settlement → record application back to positions. **Achieved** at Stage 15d. Both validators arrive at byte-identical post-tick account state; the full safety net cascades from underwater positions to a resolved zero-position chain state in a single block on the synthetic seed. Coordinator state (insurance fund, vault NAV, oracle refresh marker, funding clock) and account state both persist across restart.

Clearing-layer milestone: per-account positions are produced by real CLOB fills (not direct injection), owned by the bridge, persisted across restart, and collateral moves through `deposit`/`withdraw` primitives callable both from Rust and from EVM smart contracts via precompiles. **Achieved** at Stage 17e.

What's still synthetic / next:
- **Boot scenario is fixed but realistic-shaped.** Stage 17h retired the MM (account 999) and replaced it with five accounts trading at the same fair price (100): Alice/Bob/Carol go long against makers Dave/Eve, then the mark book sets (95/97 → mid 96) and a 4-point uPnL drift drives the cascade. Bob lands Liquidatable, Carol Underwater, Dave + Eve become ADL counterparties — same disjoint-target invariant the cascade needs, no more absurd off-market orders. A persisted-trade-history seed (so the boot scenario evolves block-by-block rather than springing fully-formed) is a separate follow-up.
- **Solidity-side test is bytecode-only.** Stage 17f deploys a hand-rolled 26-byte wrapper at a contract address in an in-memory revm `CacheDB`, executes a transaction against it via `OpenHlEvmFactory`, and asserts that the EVM `CALL` into `openhl_deposit`/`openhl_withdraw` mutates the bridge's account map. Two tests, marked `#[ignore]` due to a process-global precompile-state race with parallel bridge tests — see [`docs/testing.md`](docs/testing.md). What remains: a full signed-transaction-through-mempool-to-mined-block path requires EVM-encoded fills (deferred until follower-side payload replication lands).
- **Margin model is initial-margin-at-entry, no maintenance/oracle inputs yet.** Stage 17g enforces `collateral - amount >= |size| × avg_entry × im_bps / 10⁴` on every withdraw (bridge + precompile), at a constant 1000 bps. Free collateral against the *current* mark or oracle index, maintenance-margin checks, and `LiquidationParams` plumbing into the bridge are follow-ups.
- **Revert-aware precompiles: mechanism shipped, production wiring deferred.** Stage 17i added `OpenHlRevertGuard`, a REVM `Inspector` that snapshots `{accounts, CLOB book, pending_fills}` on every call-frame entry and restores on revert. Verified end-to-end (deposit-then-REVERT keeps the bridge map empty; deposit-then-RETURN commits). Not yet wired into `OpenHlEvmFactory::create_evm`'s default `NoOpInspector` slot — that touches every block execution in Reth and needs its own integration pass. Run the demonstrations via `cargo test -p openhl-evm via_evm_bytecode -- --ignored --test-threads=1`.
- **Follower-side bridge replication** uses Stage 13n's deterministic-recompute shortcut, which expires once `build_payload` starts pulling mempool transactions.

## Build

```bash
cargo check
cargo test
```

Requires Rust 1.85+ (pinned via `rust-toolchain.toml`).

For environment-sensitive diagnostics and manual integration checks, see
[`docs/testing.md`](docs/testing.md).
CI runs stable consensus tests by default (`cargo test -p openhl-consensus`);
ignored diagnostics are reserved for manual non-sandbox runs
(`cargo test -p openhl-consensus -- --ignored --nocapture`).

## License

Dual-licensed under [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE), at your option.
