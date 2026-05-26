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
