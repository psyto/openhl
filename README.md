# openhl

An open-source reference implementation of a Hyperliquid-shape L1: BFT consensus + EVM execution + a CLOB matching engine, with first-class vault primitives.

**Status:** Modules 1–5 shipped at the state-machine level; live per-block integration in progress. Single-validator devnet produces blocks end-to-end (real Reth EVM + real Malachite BFT) and now extends to a **two-validator BFT devnet** where two `openhl reth-devnet` processes produce matching block hashes and identical bridge state over libp2p (Stages 13l–13n). CLOB matching engine wires fills through the bridge into committed payloads; custom EVM precompiles let smart contracts read CLOB state and place orders; funding, oracle (with signed observations), liquidation (with insurance fund + ADL), and vault all ship as pure deterministic state machines. Next: driving those state machines from the live per-block flow inside the production devnet. See the build arc below.

## Why

Hyperliquid's protocol stack (HyperBFT consensus, HyperCore matching engine, HyperEVM execution) is closed source. `openhl` is the open reference implementation: a working Rust workspace anyone can read, fork, and extend. The goal is not to compete with HL — it's to give the ecosystem a public substrate that HL-shape apps can deploy onto, and a teachable codebase for engineers who want to understand how this class of L1 actually works.

## Architecture

Five subsystems, ten library crates plus the node binary. The split is deliberately load-bearing: pure state machines (clob, funding, vault) are I/O-free and deterministic; the I/O boundary (evm, consensus, node) talks to the outside world and calls into the pure crates.

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
| 4 | Funding, oracle, liquidations | `funding`, `oracle`, `liquidation` | ✅ Stage 8b (funding) + 10a–10d (liquidation margin, insurance fund, scanner, ADL) + 11–11b (oracle aggregation + signed observations) |
| 5 | Protocol-native vault primitive | `vault` | ✅ Stage 12 (share-based collateral pooling) |

v0 milestone: single-validator devnet produces blocks end-to-end. **Achieved** at the end of Module 1 / Stage 7d.

Two-validator BFT milestone: two `openhl reth-devnet` processes reach consensus over libp2p and commit matching block hashes with identical bridge state. **Achieved** at Stage 13n. See [`docs/testing.md`](docs/testing.md) for the manual bring-up procedure (including restart resilience).

v1 milestone: full perp DEX with funding + liquidations + oracle + vault wired into the live per-block bridge flow. **In progress** — all subsystems exist as pure state machines; the remaining work is driving them from `LiveRethEvmBridge` per block in the production devnet path.

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
