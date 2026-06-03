# Testing Guide

## Default (CI-safe) test run

Run stable tests only:

```bash
cargo test -p openhl-consensus
```

Some diagnostics are intentionally marked `#[ignore]` because they are
environment-sensitive (sandboxed socket permissions and actor scheduling).

## Manual integration diagnostics

Run ignored diagnostics in a non-sandbox environment:

```bash
cargo test -p openhl-consensus -- --ignored --nocapture
```

Notable diagnostics:

- `engine_app::tests::first_block_via_engine_actors`
- `node::tests::start_engine_emits_initial_consensus_message`
- `node::tests::start_engine_emits_listening_event`

The Stage 17f Solidity-side precompile tests live in `openhl-evm` and
are ignored because they read from `precompiles::ACCOUNTS_STATE`, a
process-global the bridge installs on construction — any other test
that builds a `LiveRethEvmBridge` in parallel overwrites it. Run them
single-threaded:

```bash
cargo test -p openhl-evm via_evm_bytecode -- --ignored --test-threads=1
```

- `live_node::tests::deposit_via_evm_bytecode_mutates_bridge_accounts`
- `live_node::tests::withdraw_via_evm_bytecode_debits_bridge_accounts`
- `live_node::tests::deposit_via_evm_bytecode_rolls_back_on_revert` (Stage 17i)
- `live_node::tests::deposit_via_evm_bytecode_persists_on_return` (Stage 17i)
- `live_node::tests::deposit_via_evm_bytecode_rolls_back_on_revert_through_create_evm` (Stage 17k — production-wiring path)

## Smoke-testing the `openhl_*` RPC namespace (Stage 19a)

`bin/openhl reth-devnet` exposes the bridge's accessors over HTTP JSON-RPC on `127.0.0.1:8545` (default Reth bind, alongside the `eth_*` namespace). Quick way to confirm the surface end-to-end:

```bash
# 1. Boot a single-validator devnet for enough rounds that it
#    stays up long enough to curl. Background it; capture the PID.
TEMPDIR=$(mktemp -d)
openhl reth-devnet --moniker rpcsmoke --data-dir "$TEMPDIR" --rounds 60 \
  > /tmp/openhl-rpc.log 2>&1 &
RUN_PID=$!

# 2. Wait until Reth's HTTP RPC server logs "RPC HTTP server started".
until grep -q "RPC HTTP server started" /tmp/openhl-rpc.log; do sleep 1; done

# 3. Query each method.
for method in openhl_currentMark openhl_accounts openhl_liquidationParams; do
  echo "--- $method ---"
  curl -s -X POST -H 'Content-Type: application/json' \
    --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$method\",\"params\":[]}" \
    http://127.0.0.1:8545
  echo
done

# 4. Account-scoped methods take one positional u64 arg.
curl -s -X POST -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":2,"method":"openhl_marginHealth","params":[20]}' \
  http://127.0.0.1:8545

# 5. Cleanup.
kill $RUN_PID 2>/dev/null
rm -rf "$TEMPDIR" /tmp/openhl-rpc.log
```

Expected post-seed responses (Stage 17h boot scenario): `currentMark` → `96`, `accounts` → `[10,20,30,40,50]`, `liquidationParams` → `{1000, 200, 150}` bps, `marginHealth(20)` → `"Safe"` (Bob is post-cascade flat once the run has progressed; on the very first tick he reads as `"Liquidatable"` per the seed contract — see `bin/openhl`'s seed docstring).

## Startup fail-fast behavior

`OpenHlNode::start()` now waits for the first consensus app message by
default. If none arrives within 5 seconds, startup fails with an error instead
of returning a stalled handle.

For constrained test environments, disable this check:

```rust
node.without_startup_ready_check()
```

## Two-validator devnet bring-up (Stage 13l)

Stage 13l wires `peer_multiaddr` entries from the `--validators` JSON
into Malachite's `consensus.p2p.persistent_peers`. With this in place,
two `openhl reth-devnet` instances on the same host can form a quorum.

### Step 1 — generate two validator keys

Run each node once with a distinct `--data-dir` and no `--validators`
flag; that writes a fresh `validator-key.json` under
`<data-dir>/validator-key.json` and prints the public key in the log.
Stop both processes after the key is written (Ctrl-C is fine).

```bash
openhl reth-devnet --moniker alice --data-dir /tmp/openhl-a --rounds 0
openhl reth-devnet --moniker bob   --data-dir /tmp/openhl-b --rounds 0
```

### Step 2 — write a shared `validators.json`

Use the `pubkey_hex` from each `validator-key.json`. Both nodes must
load the same file so they agree on the validator set.

```json
{
  "validators": [
    {
      "pubkey_hex": "<alice's 64-hex pubkey>",
      "voting_power": 1,
      "peer_multiaddr": "/ip4/127.0.0.1/tcp/26656"
    },
    {
      "pubkey_hex": "<bob's 64-hex pubkey>",
      "voting_power": 1,
      "peer_multiaddr": "/ip4/127.0.0.1/tcp/26657"
    }
  ]
}
```

### Step 3 — boot both nodes against the shared validator set

Each node binds the listen port advertised in its `peer_multiaddr`,
points at the shared validators file, and uses a non-default
`--rpc-bind` so the two Reth RPCs don't collide:

```bash
openhl reth-devnet \
    --moniker alice --data-dir /tmp/openhl-a \
    --validators /tmp/validators.json \
    --listen-addr /ip4/0.0.0.0/tcp/26656 \
    --rpc-bind 127.0.0.1:8545 \
    --rounds 3
```

```bash
openhl reth-devnet \
    --moniker bob --data-dir /tmp/openhl-b \
    --validators /tmp/validators.json \
    --listen-addr /ip4/0.0.0.0/tcp/26657 \
    --rpc-bind 127.0.0.1:8546 \
    --rounds 3
```

Each process logs `persistent peers = 1 peer(s)` and a `dial[0]` line
showing the *other* validator's multiaddr (self is filtered out).
Both nodes should converge on the same decided block hashes for each
height.

### Step 4 (optional) — verify restart resilience

Re-run step 3 with the **same** `--data-dir`s. Each node loads its
persisted bridge snapshot (Stage 13g), validator key (Stage 13h),
consensus height (Stage 13i), and Malachite WAL, and continues from
the prior tip — log lines read:

```
loaded snapshot  = 3 block(s); head = 7c10b6df…
driving run_engine_app for 3 decision(s) starting at height 4…
```

After the second run, both `bridge/state.json` files should show 6
blocks and identical heads:

```bash
diff \
  <(jq -S '.chain | keys' /tmp/openhl-a/bridge/state.json) \
  <(jq -S '.chain | keys' /tmp/openhl-b/bridge/state.json)
# no output → identical
```

### Generalizing to N validators

The bring-up generalizes to any validator count — the binary reads
the full set from `--validators` and dials every peer except itself.
Verified at N=3 (alice/bob/carol):

1. Generate three keys: run each node once single-validator
   (`--data-dir` distinct, no `--validators`, `--rounds 1`), then
   stop. Each writes `<data-dir>/validator-key.json`.
2. Write a `validators.json` with all three `pubkey_hex` entries and
   three distinct `peer_multiaddr`s (e.g., tcp/27656, /27657, /27658).
3. Wipe everything except `validator-key.json` in each data dir.
4. Boot all three with the shared file, matching `--listen-addr`s,
   and distinct `--rpc-bind`s.

Each process logs `persistent peers = 2 peer(s)` with two `dial[N]`
lines (self filtered from the three-entry set). With three
equal-weight validators, Malachite's >2/3 quorum needs all three to
vote, so all three must be live. On success every node's
`bridge/state.json` (chain map + accounts) and
`coordinator/state.json` are byte-identical:

```bash
diff <(jq -S . /tmp/v-a/coordinator/state.json) \
     <(jq -S . /tmp/v-b/coordinator/state.json)   # no output
diff <(jq -S . /tmp/v-a/coordinator/state.json) \
     <(jq -S . /tmp/v-c/coordinator/state.json)   # no output
```

No code is N-specific; the dial-list construction (Stage 13l) and
the consensus validator set already handle arbitrary N.

### Verifying real-payload canonicalisation on every validator (Stage 20c-2)

Through Stage 20c-1 the follower validators' Reth side stayed at
genesis — only the proposer's `engine.new_payload` ran, because
the Stage 18a `ProposedBlockWire` carried only the Header. Stage
20c-2 extends the wire with `ExecutionData`; followers install it
via their own `engine.new_payload` so every validator's Reth
canonicalises in lockstep with consensus.

Verify by hitting each node's `eth_blockNumber` against its
distinct `--rpc-bind` after a few rounds. All three should
report the same nonzero block number:

```bash
for port in 8545 8546 8547; do
  curl -s -X POST -H 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    http://127.0.0.1:$port | jq -r '.result'
done
# expect three matching nonzero results, e.g. 0x14 / 0x14 / 0x14
```

Pre-20c-2, the proposer's port returned a nonzero value while
the other two returned `0x0`. Verified at N=3 (alice/bob/carol).
