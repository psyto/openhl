use informalsystems_malachitebft_core_types::ProposalPart;
use serde::{Deserialize, Serialize};

use crate::context::OpenHlContext;
use crate::types::OpenHlHeight;
use informalsystems_malachitebft_core_types::Round;

use crate::types::OpenHlAddress;

/// Wire payload for one streamed proposal. Stage 18a inflates this from the
/// Stage 13l unit struct so the follower can install a proposer's block
/// without recomputing `build_payload` — see the module-level doc on
/// [`crate::engine_app`] for the bigger story.
///
/// The block-side payload (`block_bytes`) is **opaque** at this layer.
/// `LiveRethEvmBridge::encode_proposed_block` produces it, and
/// `LiveRethEvmBridge::register_proposed_block` consumes it. The consensus
/// crate just ferries bytes.
///
/// Streaming-wise this is a single-part proposal: `is_first()` and
/// `is_last()` both return `true`, and [`crate::engine_app`] always sends
/// exactly one `Data` stream message followed by `Fin`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenHlProposalPart {
    /// Consensus height the proposer is building at. The follower uses
    /// this when handing the assembled value back to Malachite via
    /// `AppMsg::ReceivedProposalPart`'s reply.
    pub height: OpenHlHeight,
    /// Consensus round.
    pub round: Round,
    /// "Previous-or-locked" round — Tendermint's POL semantics. Carried
    /// through unchanged.
    pub pol_round: Round,
    /// The proposer's validator address, used by Malachite to attribute
    /// the proposed value.
    pub proposer: OpenHlAddress,
    /// Bridge-encoded block bytes. Opaque to the consensus crate.
    pub block_bytes: Vec<u8>,
}

impl ProposalPart<OpenHlContext> for OpenHlProposalPart {
    fn is_first(&self) -> bool {
        true
    }

    fn is_last(&self) -> bool {
        true
    }
}
