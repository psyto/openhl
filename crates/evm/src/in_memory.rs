//! In-memory `ConsensusBridge` — a test double for the EL side.
//!
//! Useful for unit-testing the consensus crate without spinning up Reth. The
//! real Reth-backed implementation lives in `engine.rs` (lands in Module 1 L10).

use async_trait::async_trait;
use openhl_consensus::bridge::{BridgeError, ConsensusBridge};
use openhl_types::{BlockHash, ExecutedBlock, PayloadAttrs, PayloadId, PayloadStatus};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct InMemoryEvmBridge {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    next_payload_id: u64,
    pending: HashMap<u64, ExecutedBlock>,
    chain: HashMap<[u8; 32], ExecutedBlock>,
    head: Option<BlockHash>,
}

impl InMemoryEvmBridge {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ConsensusBridge for InMemoryEvmBridge {
    async fn build_payload(
        &self,
        parent: BlockHash,
        attrs: PayloadAttrs,
    ) -> Result<PayloadId, BridgeError> {
        let mut s = self.state.lock().expect("state mutex poisoned");
        let id = s.next_payload_id;
        s.next_payload_id += 1;

        let (parent_number, parent_timestamp) = s
            .chain
            .get(&parent.0)
            .map_or((0, 0), |b| (b.number, b.timestamp));
        let number = parent_number + 1;
        // Mirror LiveRethEvmBridge's timestamp derivation:
        // `max(attrs.timestamp, parent.timestamp + 1)`. Keeps the
        // in-memory bridge byte-deterministic across validators and
        // forces monotonic chain time even when the caller passes
        // attrs.timestamp = 0 (which is the engine_app default).
        let timestamp = attrs.timestamp.max(parent_timestamp + 1);

        let mut hash_bytes = [0u8; 32];
        hash_bytes[..8].copy_from_slice(&id.to_le_bytes());
        hash_bytes[8..16].copy_from_slice(&number.to_le_bytes());

        let block = ExecutedBlock {
            hash: BlockHash(hash_bytes),
            parent_hash: parent,
            number,
            state_root: [0u8; 32],
            timestamp,
        };
        s.pending.insert(id, block);
        Ok(PayloadId(id))
    }

    async fn payload_ready(&self, id: PayloadId) -> Result<ExecutedBlock, BridgeError> {
        let s = self.state.lock().expect("state mutex poisoned");
        let n = id.0;
        s.pending
            .get(&n)
            .cloned()
            .ok_or_else(|| BridgeError::Rejected(format!("unknown payload id {n}")))
    }

    async fn validate_payload(
        &self,
        _block: &ExecutedBlock,
    ) -> Result<PayloadStatus, BridgeError> {
        Ok(PayloadStatus::Valid)
    }

    async fn commit(&self, block_hash: BlockHash) -> Result<(), BridgeError> {
        let mut s = self.state.lock().expect("state mutex poisoned");
        let block = s
            .pending
            .values()
            .find(|b| b.hash == block_hash)
            .cloned()
            .ok_or_else(|| {
                let hex = hex_short(&block_hash.0);
                BridgeError::Rejected(format!("commit for unknown hash {hex}"))
            })?;
        s.chain.insert(block_hash.0, block);
        s.head = Some(block_hash);
        Ok(())
    }
}

fn hex_short(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(18);
    s.push_str("0x");
    for b in &bytes[..8] {
        write!(&mut s, "{b:02x}").expect("write to String never fails");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs() -> PayloadAttrs {
        PayloadAttrs {
            timestamp: 0,
            fee_recipient: [0u8; 20],
            prev_randao: [0u8; 32],
        }
    }

    #[tokio::test]
    async fn build_then_ready_returns_same_block() {
        let bridge = InMemoryEvmBridge::new();
        let parent = BlockHash([1u8; 32]);
        let id = bridge.build_payload(parent, attrs()).await.unwrap();
        let block = bridge.payload_ready(id).await.unwrap();
        assert_eq!(block.parent_hash, parent);
        assert_eq!(block.number, 1);
    }

    #[tokio::test]
    async fn validate_returns_valid() {
        let bridge = InMemoryEvmBridge::new();
        let block = ExecutedBlock {
            hash: BlockHash([2u8; 32]),
            parent_hash: BlockHash([1u8; 32]),
            number: 1,
            state_root: [0u8; 32],
            timestamp: 1,
        };
        let status = bridge.validate_payload(&block).await.unwrap();
        assert_eq!(status, PayloadStatus::Valid);
    }

    #[tokio::test]
    async fn commit_advances_head_and_records_block() {
        let bridge = InMemoryEvmBridge::new();
        let parent = BlockHash([1u8; 32]);
        let id = bridge.build_payload(parent, attrs()).await.unwrap();
        let block = bridge.payload_ready(id).await.unwrap();
        bridge.commit(block.hash).await.unwrap();
        let s = bridge.state.lock().unwrap();
        assert_eq!(s.head, Some(block.hash));
        assert!(s.chain.contains_key(&block.hash.0));
    }

    #[tokio::test]
    async fn build_on_committed_parent_increments_number() {
        let bridge = InMemoryEvmBridge::new();
        let genesis = BlockHash([1u8; 32]);
        let id1 = bridge.build_payload(genesis, attrs()).await.unwrap();
        let block1 = bridge.payload_ready(id1).await.unwrap();
        bridge.commit(block1.hash).await.unwrap();

        let id2 = bridge.build_payload(block1.hash, attrs()).await.unwrap();
        let block2 = bridge.payload_ready(id2).await.unwrap();
        assert_eq!(block2.number, 2);
        assert_eq!(block2.parent_hash, block1.hash);
    }

    #[tokio::test]
    async fn commit_unknown_hash_errors() {
        let bridge = InMemoryEvmBridge::new();
        let err = bridge.commit(BlockHash([9u8; 32])).await.unwrap_err();
        assert!(matches!(err, BridgeError::Rejected(_)));
    }
}
