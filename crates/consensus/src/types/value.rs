use informalsystems_malachitebft_core_types::Value;
use openhl_types::BlockHash;

/// The value consensus agrees on: an EVM block, identified by its block hash.
///
/// For v0 we store only the hash since the EVM bridge is the source of truth
/// for block contents. Module 2 will extend this to carry the full block once
/// the CLOB starts producing fills that need to be ordered alongside EVM txs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpenHlValue(pub BlockHash);

impl Value for OpenHlValue {
    type Id = BlockHash;

    fn id(&self) -> Self::Id {
        self.0
    }
}
