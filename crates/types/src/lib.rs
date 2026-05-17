//! Shared primitives and CL/EL contract types.

use serde::{Deserialize, Serialize};

/// 32-byte block hash, Ethereum convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlockHash(pub [u8; 32]);

/// Identifier returned by `build_payload`; used to retrieve the assembled block via `payload_ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PayloadId(pub u64);

/// Inputs to a payload-build job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PayloadAttrs {
    pub timestamp: u64,
    pub fee_recipient: [u8; 20],
    pub prev_randao: [u8; 32],
}

/// Verdict from `validate_payload`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PayloadStatus {
    Valid,
    Invalid,
    Syncing,
}

/// An executed block — the artifact a consensus round commits to. Minimal v0 shape; txs and receipts land per Module 2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutedBlock {
    pub hash: BlockHash,
    pub parent_hash: BlockHash,
    pub number: u64,
    pub state_root: [u8; 32],
}
