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
