//! The CL/EL contract: four messages between consensus and execution.

use async_trait::async_trait;
use openhl_types::{BlockHash, ExecutedBlock, PayloadAttrs, PayloadId, PayloadStatus};
use thiserror::Error;

/// The four-message contract between BFT consensus and EVM execution.
///
/// Every interaction between `openhl-consensus` and `openhl-evm` flows through one of these methods. Anything else is a contract leak.
#[async_trait]
pub trait ConsensusBridge: Send + Sync {
    /// CL → EL: build a candidate block on `parent`. Returns immediately; await the block via [`Self::payload_ready`].
    async fn build_payload(
        &self,
        parent: BlockHash,
        attrs: PayloadAttrs,
    ) -> Result<PayloadId, BridgeError>;

    /// EL → CL: wait for an in-flight build to complete.
    async fn payload_ready(&self, id: PayloadId) -> Result<ExecutedBlock, BridgeError>;

    /// CL → EL: would this peer-proposed block execute cleanly?
    async fn validate_payload(
        &self,
        block: &ExecutedBlock,
    ) -> Result<PayloadStatus, BridgeError>;

    /// CL → EL: finalize this block. Fire-and-forget; failure halts the chain.
    async fn commit(&self, block_hash: BlockHash) -> Result<(), BridgeError>;

    /// CL → EL (proposer side, Stage 18a): serialise the just-built payload
    /// into bytes that can be shipped to follower validators inside an
    /// `OpenHlProposalPart`. The wire format is opaque to the consensus
    /// crate — only the bridge knows how to read it back. Called by the
    /// proposer after `payload_ready` succeeds for the height it just
    /// built.
    ///
    /// Default: returns `Err(BridgeError::Internal(...))` — implementations
    /// that don't participate in real cross-validator replication
    /// (in-memory test stubs, the legacy `RethEvmBridge` placeholder)
    /// can skip overriding this and rely on the existing 13n
    /// deterministic-recompute path.
    async fn encode_proposed_block(&self, _id: PayloadId) -> Result<Vec<u8>, BridgeError> {
        Err(BridgeError::Internal(eyre::eyre!(
            "encode_proposed_block not implemented for this ConsensusBridge"
        )))
    }

    /// CL → EL (follower side, Stage 18a): install a block the proposer
    /// already built, decoded from the bytes its `encode_proposed_block`
    /// produced. The bridge stores the block in its pending map under
    /// the same payload-id machinery a local `build_payload` would have
    /// used, so the follower's subsequent `commit(hash)` finds it
    /// without a recompute.
    ///
    /// Returns the decoded [`ExecutedBlock`] so the engine_app can wrap
    /// it in a `ProposedValue` for Malachite.
    ///
    /// Default: returns `Err(BridgeError::Internal(...))` for the same
    /// reason as [`Self::encode_proposed_block`].
    async fn register_proposed_block(
        &self,
        _bytes: &[u8],
    ) -> Result<ExecutedBlock, BridgeError> {
        Err(BridgeError::Internal(eyre::eyre!(
            "register_proposed_block not implemented for this ConsensusBridge"
        )))
    }
}

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("execution layer rejected payload: {0}")]
    Rejected(String),

    #[error("execution layer is syncing")]
    Syncing,

    #[error("internal: {0}")]
    Internal(#[from] eyre::Report),
}
