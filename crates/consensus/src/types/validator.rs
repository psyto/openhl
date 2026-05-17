use informalsystems_malachitebft_core_types::{Validator, ValidatorSet, VotingPower};
use informalsystems_malachitebft_signing_ed25519::PublicKey;

use crate::context::OpenHlContext;
use crate::types::OpenHlAddress;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenHlValidator {
    pub address: OpenHlAddress,
    pub public_key: PublicKey,
    pub voting_power: VotingPower,
}

impl OpenHlValidator {
    #[must_use]
    pub const fn new(address: OpenHlAddress, public_key: PublicKey, voting_power: VotingPower) -> Self {
        Self { address, public_key, voting_power }
    }
}

impl Validator<OpenHlContext> for OpenHlValidator {
    fn address(&self) -> &OpenHlAddress {
        &self.address
    }

    fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    fn voting_power(&self) -> VotingPower {
        self.voting_power
    }
}

/// A validator set, kept sorted by (`voting_power` desc, address asc).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenHlValidatorSet(Vec<OpenHlValidator>);

impl OpenHlValidatorSet {
    /// Construct a validator set and enforce the canonical sort order.
    #[must_use]
    pub fn new(mut validators: Vec<OpenHlValidator>) -> Self {
        validators.sort_by(|a, b| {
            b.voting_power
                .cmp(&a.voting_power)
                .then_with(|| a.address.cmp(&b.address))
        });
        Self(validators)
    }

    #[must_use]
    pub fn validators(&self) -> &[OpenHlValidator] {
        &self.0
    }
}

impl ValidatorSet<OpenHlContext> for OpenHlValidatorSet {
    fn count(&self) -> usize {
        self.0.len()
    }

    fn total_voting_power(&self) -> VotingPower {
        self.0.iter().map(|v| v.voting_power).sum()
    }

    fn get_by_address(&self, address: &OpenHlAddress) -> Option<&OpenHlValidator> {
        self.0.iter().find(|v| &v.address == address)
    }

    fn get_by_index(&self, index: usize) -> Option<&OpenHlValidator> {
        self.0.get(index)
    }
}
