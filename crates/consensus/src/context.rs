//! `OpenHlContext` — the central abstraction Malachite uses to know about our chain.
//!
//! Once this trait is implemented, the entire `malachitebft-core-consensus` and
//! `malachitebft-engine` machinery can drive consensus over our types.

use informalsystems_malachitebft_core_types::{
    Context, NilOrVal, Round, ValidatorSet as _, ValueId, VoteType,
};
use informalsystems_malachitebft_signing_ed25519::Ed25519;

use crate::types::{
    OpenHlAddress, OpenHlHeight, OpenHlProposal, OpenHlProposalPart, OpenHlValidator,
    OpenHlValidatorSet, OpenHlValue, OpenHlVote,
};

#[derive(Clone, Debug, Default)]
pub struct OpenHlContext;

impl Context for OpenHlContext {
    type Address = OpenHlAddress;
    type Height = OpenHlHeight;
    type ProposalPart = OpenHlProposalPart;
    type Proposal = OpenHlProposal;
    type Validator = OpenHlValidator;
    type ValidatorSet = OpenHlValidatorSet;
    type Value = OpenHlValue;
    type Vote = OpenHlVote;
    type Extension = ();
    type SigningScheme = Ed25519;

    /// Round-robin proposer selection by (height + round) modulo validator-set size.
    ///
    /// Validators are pre-sorted by (`voting_power` desc, address asc) in
    /// `OpenHlValidatorSet::new`, so every honest node picks the same proposer
    /// for the same (height, round) — the determinism the contract requires.
    fn select_proposer<'a>(
        &self,
        validator_set: &'a Self::ValidatorSet,
        height: Self::Height,
        round: Round,
    ) -> &'a Self::Validator {
        let count = validator_set.count();
        assert!(count > 0, "validator set is empty");
        let round_u64 = u64::try_from(round.as_i64().max(0)).unwrap_or(0);
        let index_u64 = height.0.wrapping_add(round_u64);
        let index = usize::try_from(index_u64).unwrap_or(usize::MAX) % count;
        validator_set
            .get_by_index(index)
            .expect("index < count by construction")
    }

    fn new_proposal(
        &self,
        height: Self::Height,
        round: Round,
        value: Self::Value,
        pol_round: Round,
        address: Self::Address,
    ) -> Self::Proposal {
        OpenHlProposal { height, round, value, pol_round, address }
    }

    fn new_prevote(
        &self,
        height: Self::Height,
        round: Round,
        value_id: NilOrVal<ValueId<Self>>,
        address: Self::Address,
    ) -> Self::Vote {
        OpenHlVote {
            height,
            round,
            value_id,
            vote_type: VoteType::Prevote,
            address,
        }
    }

    fn new_precommit(
        &self,
        height: Self::Height,
        round: Round,
        value_id: NilOrVal<ValueId<Self>>,
        address: Self::Address,
    ) -> Self::Vote {
        OpenHlVote {
            height,
            round,
            value_id,
            vote_type: VoteType::Precommit,
            address,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use informalsystems_malachitebft_core_types::{
        Height as HeightTrait, Proposal as ProposalTrait, Validator, ValidatorSet,
        Vote as VoteTrait,
    };
    use informalsystems_malachitebft_signing_ed25519::PrivateKey;
    use openhl_types::BlockHash;
    use rand::rngs::OsRng;

    fn validator(addr_byte: u8, power: u64) -> OpenHlValidator {
        let private = PrivateKey::generate(OsRng);
        let public = private.public_key();
        OpenHlValidator::new(OpenHlAddress([addr_byte; 20]), public, power)
    }

    #[test]
    fn validator_set_is_sorted_by_power_then_address() {
        let set = OpenHlValidatorSet::new(vec![
            validator(0x01, 100),
            validator(0x02, 300),
            validator(0x03, 200),
        ]);
        let powers: Vec<u64> = set
            .validators()
            .iter()
            .map(Validator::voting_power)
            .collect();
        assert_eq!(powers, vec![300, 200, 100]);
        assert_eq!(set.total_voting_power(), 600);
        assert_eq!(set.count(), 3);
    }

    #[test]
    fn select_proposer_round_robins_deterministically() {
        let ctx = OpenHlContext;
        let set = OpenHlValidatorSet::new(vec![
            validator(0x01, 100),
            validator(0x02, 100),
            validator(0x03, 100),
        ]);
        // Same height + round → same proposer across calls
        let h = OpenHlHeight(7);
        let p1 = ctx.select_proposer(&set, h, Round::new(0)).address;
        let p2 = ctx.select_proposer(&set, h, Round::new(0)).address;
        assert_eq!(p1, p2);

        // height + 1 picks the next validator in the rotation
        let p3 = ctx.select_proposer(&set, h.increment(), Round::new(0)).address;
        assert_ne!(p1, p3);
    }

    #[test]
    fn new_proposal_round_trips_fields() {
        let ctx = OpenHlContext;
        let addr = OpenHlAddress([0xaa; 20]);
        let value = OpenHlValue(BlockHash([0xbb; 32]));
        let proposal = ctx.new_proposal(
            OpenHlHeight(5),
            Round::new(1),
            value,
            Round::Nil,
            addr,
        );
        assert_eq!(ProposalTrait::height(&proposal), OpenHlHeight(5));
        assert_eq!(*ProposalTrait::value(&proposal), value);
        assert_eq!(*ProposalTrait::validator_address(&proposal), addr);
    }

    #[test]
    fn new_prevote_and_precommit_have_distinct_types() {
        let ctx = OpenHlContext;
        let addr = OpenHlAddress([0xaa; 20]);
        let vid: NilOrVal<BlockHash> = NilOrVal::Val(BlockHash([0xbb; 32]));
        let prevote = ctx.new_prevote(OpenHlHeight(5), Round::new(0), vid, addr);
        let precommit = ctx.new_precommit(OpenHlHeight(5), Round::new(0), vid, addr);
        assert_eq!(VoteTrait::vote_type(&prevote), VoteType::Prevote);
        assert_eq!(VoteTrait::vote_type(&precommit), VoteType::Precommit);
    }

    #[test]
    fn height_increment_and_decrement() {
        let h = OpenHlHeight::INITIAL;
        assert_eq!(h.as_u64(), 1);
        assert_eq!(h.increment().as_u64(), 2);
        assert_eq!(OpenHlHeight::ZERO.decrement(), None);
        assert_eq!(OpenHlHeight(5).decrement().unwrap().as_u64(), 4);
    }
}
