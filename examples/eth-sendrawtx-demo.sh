#!/usr/bin/env bash
#
# Stage 20e demo — submit a signed Ethereum transaction to the openhl
# devnet via `eth_sendRawTransaction`, watch it mine, and pull the
# receipt back out via `eth_getTransactionReceipt`.
#
# Boots `openhl reth-devnet` in the background, signs a 1-wei transfer
# from the pre-funded Anvil dev account 0 to `0xdead…0001`, submits via
# curl, polls for the receipt, and prints it. Tears the devnet down on
# exit (`trap`).
#
# Requirements: `cargo`, `curl`, `jq`. No external Ethereum tooling
# (cast / web3.py / ethers) needed — signing is done by
# `crates/evm/examples/sign-transfer.rs` (run via `cargo run`).
#
# Usage:
#   ./examples/eth-sendrawtx-demo.sh
#
# Environment overrides (optional):
#   ROUNDS=N     consensus rounds to drive (default 100 — enough
#                headroom for the boot delay + a tx mining + a few
#                receipt polls. The devnet exits after N rounds.)
#   RPC_PORT=P   RPC port to target (default 8545)

set -euo pipefail

ROUNDS=${ROUNDS:-100}
RPC_PORT=${RPC_PORT:-8545}
RPC_URL="http://127.0.0.1:${RPC_PORT}"

# A single mktemp for both the devnet data dir and the log file.
WORK_DIR=$(mktemp -d -t openhl-demo-XXXXXX)
DEVNET_LOG="${WORK_DIR}/devnet.log"

cleanup() {
    if [ -n "${DEVNET_PID:-}" ] && kill -0 "$DEVNET_PID" 2>/dev/null; then
        kill "$DEVNET_PID" 2>/dev/null || true
        wait "$DEVNET_PID" 2>/dev/null || true
    fi
    rm -rf "$WORK_DIR"
}
trap cleanup EXIT

# Convert hex (0x…) → decimal — used for printing block numbers.
hex_to_dec() { python3 -c "print(int('$1', 16))" 2>/dev/null || echo "$1"; }

# Repeated curl-RPC call. Args: <method> <params-json-array>.
rpc_call() {
    local method="$1" params="${2:-[]}"
    curl -s -X POST -H 'Content-Type: application/json' \
        --data "{\"jsonrpc\":\"2.0\",\"method\":\"${method}\",\"params\":${params},\"id\":1}" \
        "$RPC_URL"
}

echo "==> Pre-building the signing helper (so the eval below is fast)..."
cargo build -q -p openhl-evm --example sign-transfer

echo "==> Booting 'openhl reth-devnet' (data-dir=${WORK_DIR}, rounds=${ROUNDS})..."
cargo run -q -p openhl -- reth-devnet \
    --moniker demo --rounds "$ROUNDS" --data-dir "$WORK_DIR" \
    > "$DEVNET_LOG" 2>&1 &
DEVNET_PID=$!

echo "==> Waiting for RPC at ${RPC_URL}..."
for i in $(seq 1 60); do
    if rpc_call eth_chainId >/dev/null 2>&1; then
        echo "    RPC up after ${i}s"
        break
    fi
    sleep 1
done

CHAIN_ID=$(rpc_call eth_chainId | jq -r '.result')
echo "==> chain id = ${CHAIN_ID} ($(hex_to_dec "$CHAIN_ID"))"

DEV_ADDR="0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
BAL_BEFORE=$(rpc_call eth_getBalance "[\"$DEV_ADDR\",\"latest\"]" | jq -r '.result')
echo "==> dev account 0 balance BEFORE: ${BAL_BEFORE} wei"

echo "==> Signing 1-wei transfer from dev account 0 to 0xdead…0001..."
SIGN_OUT=$(cargo run -q -p openhl-evm --example sign-transfer)
eval "$SIGN_OUT"
echo "    TX_HASH = ${TX_HASH}"

echo "==> Posting eth_sendRawTransaction..."
SEND_RESP=$(rpc_call eth_sendRawTransaction "[\"$RAW_TX\"]")
RETURNED_HASH=$(echo "$SEND_RESP" | jq -r '.result // .error.message')
echo "    pool replied: ${RETURNED_HASH}"

echo "==> Polling eth_getTransactionReceipt..."
RECEIPT=""
for i in $(seq 1 60); do
    RECEIPT=$(rpc_call eth_getTransactionReceipt "[\"$TX_HASH\"]")
    BLK=$(echo "$RECEIPT" | jq -r '.result.blockNumber // empty')
    if [ -n "$BLK" ] && [ "$BLK" != "null" ]; then
        echo "    mined at block ${BLK} ($(hex_to_dec "$BLK")) after ${i}s"
        break
    fi
    sleep 1
done

echo
echo "==> Full receipt:"
echo "$RECEIPT" | jq .

BAL_AFTER=$(rpc_call eth_getBalance "[\"$DEV_ADDR\",\"latest\"]" | jq -r '.result')
echo
echo "==> dev account 0 balance AFTER:  ${BAL_AFTER} wei"
echo "    (1 wei transferred + 21000 × 1 gwei gas spent)"

echo
echo "==> Demo complete. Tearing down."
