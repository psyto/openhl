use informalsystems_malachitebft_core_types::ProposalPart;
use serde::{Deserialize, Serialize};

use crate::context::OpenHlContext;

/// Unit proposal part — `OpenHL` runs in `ValuePayload::ProposalOnly` mode, so
/// the entire value ships in the `Proposal` message and parts are unused.
/// The type is required by the `Context` trait surface anyway.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenHlProposalPart;

impl ProposalPart<OpenHlContext> for OpenHlProposalPart {
    fn is_first(&self) -> bool {
        true
    }

    fn is_last(&self) -> bool {
        true
    }
}
