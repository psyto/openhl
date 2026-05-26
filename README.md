# openhl

An open-source reference implementation of a Hyperliquid-shape L1: BFT consensus + EVM execution + a CLOB matching engine, with first-class vault primitives.

**Status:** Modules 1–3 shipped; Module 4 partial. Single-validator devnet produces blocks end-to-end (real Reth EVM + real Malachite BFT); CLOB matching engine wires fills through the bridge into committed payloads; custom EVM precompiles let smart contracts read CLOB state and place orders; funding state machine and liquidation margin math run as pure deterministic state machines. Insurance fund, oracle, and Module 5 vault primitive are next. See the build arc below.

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
| 1 | Consensus substrate (Malachite + Reth) | `consensus`, `evm`, `node` | ✅ Stage 6 → 7d |
| 2 | CLOB matching engine | `clob`, `types`, `codec` | ✅ Stage 8a + 8d |
| 3 | Core ↔ EVM precompiles | `evm`, `clob` | ✅ Stage 9a–9e + 9c+ + 9d |
| 4 | Funding, oracle, liquidations | `funding`, `oracle`, `liquidation` | 🟡 Partial — funding ✅ Stage 8b; liquidation 🟡 Stage 10a (margin math); insurance fund + oracle next |
| 5 | Protocol-native vault primitive | `vault` | ⬜ Pending |

v0 milestone: single-validator devnet produces blocks end-to-end. **Achieved** at the end of Module 1 / Stage 7d.

v1 milestone: full perp DEX with funding + liquidations + oracle wired into the bridge. **In progress** — Stage 10a (liquidation margin math) is the latest landed sub-stage.

## Build

```bash
cargo check
cargo test
```

Requires Rust 1.85+ (pinned via `rust-toolchain.toml`).

For environment-sensitive diagnostics and manual integration checks, see
[`docs/testing.md`](docs/testing.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE), at your option.
