# openhl

An open-source reference implementation of a Hyperliquid-shape L1: BFT consensus + EVM execution + a CLOB matching engine, with first-class vault primitives.

**Status:** early scaffolding. Not runnable yet. See the build arc below.

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

| # | Module | Crates touched |
| - | --- | --- |
| 1 | Consensus substrate (Malachite + Reth) | `consensus`, `evm`, `node` |
| 2 | CLOB matching engine | `clob`, `types`, `codec` |
| 3 | Core ↔ EVM precompiles | `evm`, `clob` |
| 4 | Funding, oracle, liquidations | `funding`, `oracle`, `liquidation` |
| 5 | Protocol-native vault primitive | `vault` |

v0 milestone: single-validator devnet produces blocks end-to-end (end of Module 1).

## Build

```bash
cargo check
cargo test
```

Requires Rust 1.85+ (pinned via `rust-toolchain.toml`).

## License

Dual-licensed under [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE), at your option.
