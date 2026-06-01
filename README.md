# openhl

An open-source reference implementation of a Hyperliquid-shape L1: BFT consensus + EVM execution + a CLOB matching engine, with first-class vault primitives.

**Status:** All six modules live end-to-end on a multi-validator BFT devnet (verified at N=3). Block decisions reach quorum over libp2p via real `ProposalAndParts` streaming (Stages 13l–13n, 18a); on every commit the integration coordinator runs oracle → liquidation scan → ADL → vault mark-to-market → funding settlement (14a–15e). Per-account positions are produced by real CLOB fills routed through `openhl-clearing::apply_fill` (16a–17a). Collateral moves through `deposit`/`withdraw` primitives — Rust API on the bridge AND EVM precompiles for Solidity contracts — with mark-aware free-collateral checks (17j), revert-aware mutations production-wired (17k), tunable `LiquidationParams` (17l–17m), and a queryable `openhl_marginHealth` precompile + bridge accessor (17m–17n). External clients reach all of it over an `openhl_*` JSON-RPC namespace alongside Reth's standard `eth_*` (19a). Every committed block produces byte-identical state across validators; chain, accounts, and coordinator state all persist across restart. See the build arc below.

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
| 1 | Consensus substrate (Malachite + Reth) | `consensus`, `evm`, `node` | ✅ Stage 6 → 7d (single-validator); Stages 13l–13n add two-validator BFT; Stage 18a replaces 13n's deterministic-recompute trick with real `ProposalAndParts` streaming + `bridge.register_proposed_block` |
| 2 | CLOB matching engine | `clob`, `types`, `codec` | ✅ Stage 8a + 8d |
| 3 | Core ↔ EVM precompiles | `evm`, `clob` | ✅ Stage 9a–9e + 9c+ + 9d |
| 4 | Funding, oracle, liquidations | `funding`, `oracle`, `liquidation` | ✅ Stage 8b (funding) + 10a–10d (liquidation margin, insurance fund, scanner, ADL) + 11–11b (oracle aggregation + signed observations); driven per-block via Stages 14a–15d |
| 5 | Protocol-native vault primitive | `vault` | ✅ Stage 12 (share-based collateral pooling); marked-to-market per block via Stage 14a |
| 6 | Clearing layer (positions + collateral) | `clearing`, `evm` | ✅ Stage 16a–16d (apply_fill + bridge-owned accounts) + 17a (real fills create accounts) + 17b–17e (deposit/withdraw primitives + EVM precompiles) + 17f–17q (precompile hardening + margin model: bytecode-CALL test, margin-aware withdraw, mark-aware free collateral, revert-aware mutations production-wired, configurable `LiquidationParams`, `bridge.margin_health` + `openhl_margin_health` precompile, oracle-index mark on both bridge and coordinator sides + staleness defense) |

v0 milestone: single-validator devnet produces blocks end-to-end. **Achieved** at the end of Module 1 / Stage 7d.

Two-validator BFT milestone: two `openhl reth-devnet` processes reach consensus over libp2p and commit matching block hashes with identical bridge state. **Achieved** at Stage 13n. See [`docs/testing.md`](docs/testing.md) for the manual bring-up procedure (including restart resilience).

v1 milestone: per-block integration cascade runs across both validators — oracle aggregation → liquidation scan → ADL → vault mark-to-market → funding settlement → record application back to positions. **Achieved** at Stage 15d. Both validators arrive at byte-identical post-tick account state; the full safety net cascades from underwater positions to a resolved zero-position chain state in a single block on the synthetic seed. Coordinator state (insurance fund, vault NAV, oracle refresh marker, funding clock) and account state both persist across restart.

Clearing-layer milestone: per-account positions are produced by real CLOB fills (not direct injection), owned by the bridge, persisted across restart, and collateral moves through `deposit`/`withdraw` primitives callable both from Rust and from EVM smart contracts via precompiles. **Achieved** at Stage 17e.

What's still synthetic / next:
- **Boot scenario is fixed-but-realistic-shaped, with an operator escape hatch.** Stage 17h retired the MM (account 999) and replaced it with five accounts trading at fair value, Stage 17p re-tuned for the oracle-driven scan. Stage 19b adds `--seed-fixture <path.json>` so operators can demo any market shape without recompiling (default behavior unchanged; see the "Seed fixtures" section below). The cascade still springs fully-formed on tick 1; a chain-history seed where boot replays block-by-block from a persisted log is a separate follow-up.
- **Solidity-side test is bytecode-only; full-tx path foundation laid.** Stage 17f deploys a hand-rolled 26-byte wrapper at a contract address in an in-memory revm `CacheDB`, executes a transaction against it via `OpenHlEvmFactory`, and asserts that the EVM `CALL` into `openhl_deposit`/`openhl_withdraw` mutates the bridge's account map. Stage 19d ships a `MarginHealthReader` contract via genesis allocation, reachable via `eth_call`. Stage 20a adds `bridge.build_real_payload(parent, attrs)` which invokes Reth's actual `PayloadBuilderService` to produce a real `ExecutionPayloadV3` (mempool transactions executed, real state / receipts roots). Remaining for the full path: wire `build_real_payload` into `bridge.build_payload`'s production flow (replacing the synthesized header), add `engine.new_payload` on commit, and extend the 18a wire format to ship the full payload to followers. That's Stages 20b/20c.
- **Margin model is end-to-end production-shape.** Stage 17j upgrades the withdraw rule to `free = (collateral + uPnL) − |size| × mark × im_bps / 10⁴`. 17l → 17m make the full `LiquidationParams` runtime-tunable. 17m exposes `bridge.margin_health(account)`; 17n adds the same classifier as the `openhl_margin_health` precompile at `0x…0c1f`. 17o pipes `openhl-oracle`'s aggregated index through to the bridge / precompile as the canonical mark (falling back to CLOB midpoint pre-first-refresh); 17p aligns the integration coordinator's `OpenHlNode::tick` so the liquidation scan + ADL use the same oracle-preferred mark — `bridge.margin_health` now accurately predicts what the next tick's cascade will do. Stage 17q closes the stale-oracle gap: a freshness check (`OracleParams::aggregate_max_age_secs`, default 60s) gates the oracle's use as mark, so a publisher set that stops pushing falls back to the CLOB midpoint rather than letting an aging aggregate delay liquidations or fix the funding premium. CLOB midpoint stays the input to the funding-rate premium (`premium = mark − index`) where it's load-bearing.

## RPC

`bin/openhl reth-devnet` exposes Reth's standard `eth_*` namespace plus an
`openhl_*` namespace (Stage 19a) that wraps the bridge's accessors so a
frontend or trading client can query chain state without re-implementing
the engine.

| Method | Returns |
| --- | --- |
| `openhl_currentMark` | `Option<u64>` — CLOB midpoint, `null` if one-sided book |
| `openhl_oracleIndexPrice` | `Option<u64>` — aggregated oracle index, `null` before first refresh (Stage 17o) |
| `openhl_effectiveMark` | `Option<u64>` — what the bridge actually consults for margin: oracle index if set, else CLOB midpoint |
| `openhl_accounts` | `Vec<u64>` — every account id the bridge has seen |
| `openhl_accountSnapshot(account)` | `Option<{account, position_size, avg_entry, collateral}>` — `null` if unknown |
| `openhl_marginHealth(account)` | `Option<"Safe" \| "AtRisk" \| "Liquidatable" \| "Underwater">` — `null` if indeterminate |
| `openhl_liquidationParams` | `{initial_margin_bps, maintenance_margin_bps, liquidation_fee_bps}` |

```bash
curl -s -X POST -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"openhl_marginHealth","params":[20]}' \
  http://127.0.0.1:8545
# → {"jsonrpc":"2.0","id":1,"result":"Safe"}
```

### Reader contracts via `eth_call` (Stage 19d)

The precompile addresses (`0x…0c1b` … `0x…0c1f`) aren't directly addressable from standard Ethereum clients — viem / ethers / curl talk to **deployed contracts**, not precompile addresses. Stage 19d pre-deploys a tiny 26-byte wrapper at a fixed address via the dev chain's genesis allocation, so any standard ETH client can hit `openhl_margin_health` through `eth_call`:

| Reader | Address | Wraps |
| --- | --- | --- |
| MarginHealthReader | `0x0000000000000000000000000000000000011101` | `openhl_margin_health` at `0x…0c1f` |

Calldata is the 32-byte ABI-encoded account id; the response is a 32-byte word whose last byte is the discriminator (0 Indeterminate / 1 Safe / 2 AtRisk / 3 Liquidatable / 4 Underwater).

```bash
# Margin health for account 20 (Bob, after the boot cascade resolves):
curl -s -X POST -H 'Content-Type: application/json' http://127.0.0.1:8545 \
  --data '{"jsonrpc":"2.0","id":1,"method":"eth_call","params":[{
    "to":   "0x0000000000000000000000000000000000011101",
    "data": "0x0000000000000000000000000000000000000000000000000000000000000014"
  },"latest"]}'
# → {"result":"0x...01"}    (1 = Safe)
```

`eth_call` is read-only (no state mutation), which matches `openhl_margin_health`'s read-only semantics. Wrapping the mutating precompiles (`openhl_deposit` / `openhl_withdraw`) as reader contracts is a separate stage — it depends on `eth_sendRawTransaction` actually mining a block, which depends on `bridge.build_payload` integrating Reth's `PayloadBuilder`.

### WebSocket subscriptions (Stage 19c)

For push-style updates without polling, the same namespace exposes three subscriptions over WebSocket (`ws://127.0.0.1:8546`):

| Method | Item |
| --- | --- |
| `openhl_subscribeCurrentMark` | `Option<u64>` — CLOB midpoint, pushed on change |
| `openhl_subscribeEffectiveMark` | `Option<u64>` — oracle index if installed, else midpoint |
| `openhl_subscribeMarginHealth(account)` | `Option<"Safe" \| "AtRisk" \| "Liquidatable" \| "Underwater">` |

All three poll the bridge accessor server-side every 1s and emit only when the value differs from the previous emission (so an idle subscription stays cheap). Unsubscribe with the standard `_unsubscribe` companion method jsonrpsee generates per subscription.

```python
import asyncio, json, websockets
async def main():
    async with websockets.connect("ws://127.0.0.1:8546") as ws:
        await ws.send(json.dumps({"jsonrpc":"2.0","id":1,
            "method":"openhl_subscribeMarginHealth","params":[20]}))
        ack = await ws.recv()  # subscription id
        while True:
            msg = await ws.recv()  # pushes when health changes
            print(msg)
asyncio.run(main())
```

## Seed fixtures

The boot scenario `bin/openhl reth-devnet` runs out of the box (a hardcoded five-account trade sequence designed to demonstrate the cascade end-to-end) can be replaced with a JSON fixture via `--seed-fixture <path>` (Stage 19b). The fixture lists `submit_order` calls and `bridge.deposit` calls; everything else (oracle publishers, mark book interpretation, etc.) stays as-is.

```bash
openhl reth-devnet --moniker alice --data-dir /tmp/openhl-a \
    --seed-fixture examples/seed-default.json --rounds 3
```

`examples/seed-default.json` replays the hardcoded seed byte-identically — copy it and edit to demo a different market shape.

**Cross-validator note:** every validator MUST load the same fixture. The seed runs in production code paths and the resulting bridge state is part of the determinism contract — different fixtures → different initial state → consensus diverges.

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
