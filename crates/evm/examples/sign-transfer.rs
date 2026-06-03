//! Stage 20e helper — sign a `TxLegacy` with the well-known Anvil
//! dev account 0 (the same address `bin/openhl reth-devnet` pre-funds
//! with 1000 ETH) and print `TX_HASH=…` + `RAW_TX=…` to stdout.
//!
//! The `examples/eth-sendrawtx-demo.sh` smoke script `eval`s the
//! output to populate two shell variables, then submits the raw tx
//! via `eth_sendRawTransaction`. Anyone can also invoke the example
//! directly to mint custom transactions:
//!
//! ```text
//! cargo run -q -p openhl-evm --example sign-transfer
//!   # → defaults: nonce=0, to=0xdead…, value=1 wei
//!
//! cargo run -q -p openhl-evm --example sign-transfer -- 7 \
//!   0x0000000000000000000000000000000000000bee 42
//!   # → nonce=7, to=0x…0bee, value=42 wei
//! ```
//!
//! The dev key (`0xac09…ff80`) is the canonical Anvil / Hardhat dev
//! key 0 — it's public on purpose so off-the-shelf tooling can sign
//! against the devnet without bespoke setup. Address:
//! `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266`.

use alloy_consensus::{SignableTransaction, TxEnvelope, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{hex, Address, TxKind, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;

const DEV_ANVIL_PRIVKEY: [u8; 32] = [
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff,
    0x94, 0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2,
    0xff, 0x80,
];

fn main() {
    let mut args = std::env::args().skip(1);
    let nonce: u64 = args
        .next()
        .map(|s| s.parse().expect("nonce must be a u64"))
        .unwrap_or(0);
    let to: Address = args
        .next()
        .map(|s| s.parse().expect("`to` must be a 0x… 20-byte address"))
        .unwrap_or_else(|| {
            // Default sink address — distinctive byte pattern so it's
            // visible in receipts / logs even without `cast` decoding.
            let mut bytes = [0u8; 20];
            bytes[0] = 0xde;
            bytes[1] = 0xad;
            Address::from(bytes)
        });
    let value_wei: u128 = args
        .next()
        .map(|s| s.parse().expect("value must be a u128 (wei)"))
        .unwrap_or(1);

    let signer = PrivateKeySigner::from_bytes(&DEV_ANVIL_PRIVKEY.into())
        .expect("PrivateKeySigner from Anvil dev key");

    let tx = TxLegacy {
        chain_id: Some(2600),
        nonce,
        gas_price: 1_000_000_000, // 1 gwei
        gas_limit: 21_000,
        to: TxKind::Call(to),
        value: U256::from(value_wei),
        input: Default::default(),
    };
    let sig = signer
        .sign_hash_sync(&tx.signature_hash())
        .expect("sign tx hash");
    let signed = tx.into_signed(sig);
    let tx_hash = *signed.hash();
    let envelope: TxEnvelope = signed.into();
    let raw = envelope.encoded_2718();

    // Shell-eval-friendly format: VAR=hex.
    println!("TX_HASH=0x{}", hex::encode(tx_hash));
    println!("RAW_TX=0x{}", hex::encode(raw));
}
