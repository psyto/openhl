//! JSON codec for the OpenHL consensus messages.
//!
//! Each `Codec<T>` impl serializes a Malachite message into a JSON blob and
//! back. Several Malachite types (`SignedMessage`, `PolkaCertificate`,
//! `RoundCertificate`, `StreamMessage`, etc.) use `derive_where` rather than
//! serde derives, so we route those through `Raw*` shim structs that hold
//! their fields explicitly. The shims live entirely inside this file — they
//! are not part of the public API.
//!
//! Stage 13m: this used to be a fully stubbed `CodecStub` set; with the
//! stubs in place the consensus engine could compile but a two-validator
//! libp2p run could never form a quorum because votes/proposals refused to
//! serialize.

use bytes::Bytes;
use informalsystems_malachitebft_app::types::codec::Codec;
use informalsystems_malachitebft_app::types::streaming::{StreamId, StreamMessage};
use informalsystems_malachitebft_app::types::sync::{RawDecidedValue, Request, Response, Status};
use informalsystems_malachitebft_app::types::{PeerId, ProposedValue, SignedConsensusMsg};
use informalsystems_malachitebft_engine::util::streaming::StreamContent;
use informalsystems_malachitebft_sync::{ValueRequest, ValueResponse};
use informalsystems_malachitebft_core_consensus::LivenessMsg;
use informalsystems_malachitebft_core_types::{
    CommitCertificate, CommitSignature, NilOrVal, PolkaCertificate, PolkaSignature, Round,
    RoundCertificate, RoundCertificateType, RoundSignature, SignedMessage, Validity, VoteType,
};
use informalsystems_malachitebft_signing_ed25519::Signature;
use openhl_types::BlockHash;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::context::OpenHlContext;
use crate::types::{
    OpenHlAddress, OpenHlHeight, OpenHlProposal, OpenHlProposalPart, OpenHlValue, OpenHlVote,
};

#[derive(Copy, Clone, Debug, Default)]
pub struct OpenHlCodec;

#[derive(Debug, Error)]
pub enum CodecError {
    #[error("json codec error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid peer id bytes: {0}")]
    PeerId(String),
}

// ---- ProposalPart (Stage 18a: now carries the proposer's block bytes) ----

impl Codec<OpenHlProposalPart> for OpenHlCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<OpenHlProposalPart, Self::Error> {
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn encode(&self, msg: &OpenHlProposalPart) -> Result<Bytes, Self::Error> {
        Ok(Bytes::from(serde_json::to_vec(msg)?))
    }
}

// ---- SignedConsensusMsg (votes + proposals) ------------------------------

#[derive(Serialize, Deserialize)]
struct RawSignedVote {
    message: OpenHlVote,
    signature: Signature,
}

#[derive(Serialize, Deserialize)]
struct RawSignedProposal {
    message: OpenHlProposal,
    signature: Signature,
}

#[derive(Serialize, Deserialize)]
enum RawSignedConsensusMsg {
    Vote(RawSignedVote),
    Proposal(RawSignedProposal),
}

impl From<SignedConsensusMsg<OpenHlContext>> for RawSignedConsensusMsg {
    fn from(msg: SignedConsensusMsg<OpenHlContext>) -> Self {
        match msg {
            SignedConsensusMsg::Vote(v) => Self::Vote(RawSignedVote {
                message: v.message,
                signature: v.signature,
            }),
            SignedConsensusMsg::Proposal(p) => Self::Proposal(RawSignedProposal {
                message: p.message,
                signature: p.signature,
            }),
        }
    }
}

impl From<RawSignedConsensusMsg> for SignedConsensusMsg<OpenHlContext> {
    fn from(raw: RawSignedConsensusMsg) -> Self {
        match raw {
            RawSignedConsensusMsg::Vote(v) => {
                SignedConsensusMsg::Vote(SignedMessage::new(v.message, v.signature))
            }
            RawSignedConsensusMsg::Proposal(p) => {
                SignedConsensusMsg::Proposal(SignedMessage::new(p.message, p.signature))
            }
        }
    }
}

impl Codec<SignedConsensusMsg<OpenHlContext>> for OpenHlCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<SignedConsensusMsg<OpenHlContext>, Self::Error> {
        let raw: RawSignedConsensusMsg = serde_json::from_slice(&bytes)?;
        Ok(raw.into())
    }

    fn encode(&self, msg: &SignedConsensusMsg<OpenHlContext>) -> Result<Bytes, Self::Error> {
        let raw = RawSignedConsensusMsg::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

// ---- LivenessMsg (gossip heartbeats + skip certs) ------------------------

#[derive(Serialize, Deserialize)]
struct RawPolkaSignature {
    address: OpenHlAddress,
    signature: Signature,
}

#[derive(Serialize, Deserialize)]
struct RawPolkaCertificate {
    height: OpenHlHeight,
    round: Round,
    value_id: BlockHash,
    polka_signatures: Vec<RawPolkaSignature>,
}

#[derive(Serialize, Deserialize)]
struct RawRoundSignature {
    vote_type: VoteType,
    value_id: NilOrVal<BlockHash>,
    address: OpenHlAddress,
    signature: Signature,
}

#[derive(Serialize, Deserialize)]
struct RawRoundCertificate {
    height: OpenHlHeight,
    round: Round,
    cert_type: RoundCertificateType,
    round_signatures: Vec<RawRoundSignature>,
}

#[derive(Serialize, Deserialize)]
enum RawLivenessMsg {
    Vote(RawSignedVote),
    PolkaCertificate(RawPolkaCertificate),
    SkipRoundCertificate(RawRoundCertificate),
}

impl From<LivenessMsg<OpenHlContext>> for RawLivenessMsg {
    fn from(msg: LivenessMsg<OpenHlContext>) -> Self {
        match msg {
            LivenessMsg::Vote(v) => Self::Vote(RawSignedVote {
                message: v.message,
                signature: v.signature,
            }),
            LivenessMsg::PolkaCertificate(c) => Self::PolkaCertificate(RawPolkaCertificate {
                height: c.height,
                round: c.round,
                value_id: c.value_id,
                polka_signatures: c
                    .polka_signatures
                    .into_iter()
                    .map(|s| RawPolkaSignature {
                        address: s.address,
                        signature: s.signature,
                    })
                    .collect(),
            }),
            LivenessMsg::SkipRoundCertificate(c) => {
                Self::SkipRoundCertificate(RawRoundCertificate {
                    height: c.height,
                    round: c.round,
                    cert_type: c.cert_type,
                    round_signatures: c
                        .round_signatures
                        .into_iter()
                        .map(|s| RawRoundSignature {
                            vote_type: s.vote_type,
                            value_id: s.value_id,
                            address: s.address,
                            signature: s.signature,
                        })
                        .collect(),
                })
            }
        }
    }
}

impl From<RawLivenessMsg> for LivenessMsg<OpenHlContext> {
    fn from(raw: RawLivenessMsg) -> Self {
        match raw {
            RawLivenessMsg::Vote(v) => LivenessMsg::Vote(SignedMessage::new(v.message, v.signature)),
            RawLivenessMsg::PolkaCertificate(c) => LivenessMsg::PolkaCertificate(PolkaCertificate {
                height: c.height,
                round: c.round,
                value_id: c.value_id,
                polka_signatures: c
                    .polka_signatures
                    .into_iter()
                    .map(|s| PolkaSignature {
                        address: s.address,
                        signature: s.signature,
                    })
                    .collect(),
            }),
            RawLivenessMsg::SkipRoundCertificate(c) => {
                LivenessMsg::SkipRoundCertificate(RoundCertificate {
                    height: c.height,
                    round: c.round,
                    cert_type: c.cert_type,
                    round_signatures: c
                        .round_signatures
                        .into_iter()
                        .map(|s| RoundSignature {
                            vote_type: s.vote_type,
                            value_id: s.value_id,
                            address: s.address,
                            signature: s.signature,
                        })
                        .collect(),
                })
            }
        }
    }
}

impl Codec<LivenessMsg<OpenHlContext>> for OpenHlCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<LivenessMsg<OpenHlContext>, Self::Error> {
        let raw: RawLivenessMsg = serde_json::from_slice(&bytes)?;
        Ok(raw.into())
    }

    fn encode(&self, msg: &LivenessMsg<OpenHlContext>) -> Result<Bytes, Self::Error> {
        let raw = RawLivenessMsg::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

// ---- StreamMessage<ProposalPart> -----------------------------------------

#[derive(Serialize, Deserialize)]
enum RawStreamContent {
    Data(OpenHlProposalPart),
    Fin,
}

#[derive(Serialize, Deserialize)]
struct RawStreamMessage {
    stream_id: Bytes,
    sequence: u64,
    content: RawStreamContent,
}

impl From<StreamMessage<OpenHlProposalPart>> for RawStreamMessage {
    fn from(msg: StreamMessage<OpenHlProposalPart>) -> Self {
        Self {
            stream_id: msg.stream_id.to_bytes(),
            sequence: msg.sequence,
            content: match msg.content {
                StreamContent::Data(p) => RawStreamContent::Data(p),
                StreamContent::Fin => RawStreamContent::Fin,
            },
        }
    }
}

impl From<RawStreamMessage> for StreamMessage<OpenHlProposalPart> {
    fn from(raw: RawStreamMessage) -> Self {
        Self {
            stream_id: StreamId::new(raw.stream_id),
            sequence: raw.sequence,
            content: match raw.content {
                RawStreamContent::Data(p) => StreamContent::Data(p),
                RawStreamContent::Fin => StreamContent::Fin,
            },
        }
    }
}

impl Codec<StreamMessage<OpenHlProposalPart>> for OpenHlCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<StreamMessage<OpenHlProposalPart>, Self::Error> {
        let raw: RawStreamMessage = serde_json::from_slice(&bytes)?;
        Ok(raw.into())
    }

    fn encode(&self, msg: &StreamMessage<OpenHlProposalPart>) -> Result<Bytes, Self::Error> {
        let raw = RawStreamMessage::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

// ---- ProposedValue (WAL) -------------------------------------------------

/// Mirror of [`Validity`] — the upstream enum only derives borsh, not serde,
/// so we round-trip it through this serde-friendly twin.
#[derive(Serialize, Deserialize)]
enum RawValidity {
    Valid,
    Invalid,
}

impl From<Validity> for RawValidity {
    fn from(v: Validity) -> Self {
        match v {
            Validity::Valid => Self::Valid,
            Validity::Invalid => Self::Invalid,
        }
    }
}

impl From<RawValidity> for Validity {
    fn from(r: RawValidity) -> Self {
        match r {
            RawValidity::Valid => Self::Valid,
            RawValidity::Invalid => Self::Invalid,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct RawProposedValue {
    height: OpenHlHeight,
    round: Round,
    valid_round: Round,
    proposer: OpenHlAddress,
    value: OpenHlValue,
    validity: RawValidity,
}

impl From<ProposedValue<OpenHlContext>> for RawProposedValue {
    fn from(v: ProposedValue<OpenHlContext>) -> Self {
        Self {
            height: v.height,
            round: v.round,
            valid_round: v.valid_round,
            proposer: v.proposer,
            value: v.value,
            validity: v.validity.into(),
        }
    }
}

impl From<RawProposedValue> for ProposedValue<OpenHlContext> {
    fn from(r: RawProposedValue) -> Self {
        Self {
            height: r.height,
            round: r.round,
            valid_round: r.valid_round,
            proposer: r.proposer,
            value: r.value,
            validity: r.validity.into(),
        }
    }
}

impl Codec<ProposedValue<OpenHlContext>> for OpenHlCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<ProposedValue<OpenHlContext>, Self::Error> {
        let raw: RawProposedValue = serde_json::from_slice(&bytes)?;
        Ok(raw.into())
    }

    fn encode(&self, msg: &ProposedValue<OpenHlContext>) -> Result<Bytes, Self::Error> {
        let raw = RawProposedValue::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

// ---- sync::Status --------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct RawStatus {
    /// `PeerId`'s on-wire multihash form. The peer crate does provide a
    /// `serde` feature, but enabling it would require pulling that crate in
    /// as a workspace dep just for one type — round-tripping through
    /// `to_bytes`/`from_bytes` is simpler.
    peer_id: Bytes,
    tip_height: OpenHlHeight,
    history_min_height: OpenHlHeight,
}

impl From<Status<OpenHlContext>> for RawStatus {
    fn from(s: Status<OpenHlContext>) -> Self {
        Self {
            peer_id: Bytes::from(s.peer_id.to_bytes()),
            tip_height: s.tip_height,
            history_min_height: s.history_min_height,
        }
    }
}

impl TryFrom<RawStatus> for Status<OpenHlContext> {
    type Error = CodecError;

    fn try_from(r: RawStatus) -> Result<Self, Self::Error> {
        Ok(Self {
            peer_id: PeerId::from_bytes(&r.peer_id)
                .map_err(|e| CodecError::PeerId(e.to_string()))?,
            tip_height: r.tip_height,
            history_min_height: r.history_min_height,
        })
    }
}

impl Codec<Status<OpenHlContext>> for OpenHlCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<Status<OpenHlContext>, Self::Error> {
        let raw: RawStatus = serde_json::from_slice(&bytes)?;
        raw.try_into()
    }

    fn encode(&self, msg: &Status<OpenHlContext>) -> Result<Bytes, Self::Error> {
        let raw = RawStatus::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

// ---- sync::Request -------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct RawValueRequest {
    height: OpenHlHeight,
}

#[derive(Serialize, Deserialize)]
enum RawRequest {
    ValueRequest(RawValueRequest),
}

impl From<Request<OpenHlContext>> for RawRequest {
    fn from(r: Request<OpenHlContext>) -> Self {
        match r {
            Request::ValueRequest(vr) => Self::ValueRequest(RawValueRequest { height: vr.height }),
        }
    }
}

impl From<RawRequest> for Request<OpenHlContext> {
    fn from(raw: RawRequest) -> Self {
        match raw {
            RawRequest::ValueRequest(vr) => Self::ValueRequest(ValueRequest::new(vr.height)),
        }
    }
}

impl Codec<Request<OpenHlContext>> for OpenHlCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<Request<OpenHlContext>, Self::Error> {
        let raw: RawRequest = serde_json::from_slice(&bytes)?;
        Ok(raw.into())
    }

    fn encode(&self, msg: &Request<OpenHlContext>) -> Result<Bytes, Self::Error> {
        let raw = RawRequest::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

// ---- sync::Response ------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct RawCommitSignature {
    address: OpenHlAddress,
    signature: Signature,
}

#[derive(Serialize, Deserialize)]
struct RawCommitCertificate {
    height: OpenHlHeight,
    round: Round,
    value_id: BlockHash,
    commit_signatures: Vec<RawCommitSignature>,
}

#[derive(Serialize, Deserialize)]
struct RawDecided {
    value_bytes: Bytes,
    certificate: RawCommitCertificate,
}

#[derive(Serialize, Deserialize)]
struct RawValueResponse {
    height: OpenHlHeight,
    value: Option<RawDecided>,
}

#[derive(Serialize, Deserialize)]
enum RawResponse {
    ValueResponse(RawValueResponse),
}

impl From<Response<OpenHlContext>> for RawResponse {
    fn from(r: Response<OpenHlContext>) -> Self {
        match r {
            Response::ValueResponse(vr) => Self::ValueResponse(RawValueResponse {
                height: vr.height,
                value: vr.value.map(|d| RawDecided {
                    value_bytes: d.value_bytes,
                    certificate: RawCommitCertificate {
                        height: d.certificate.height,
                        round: d.certificate.round,
                        value_id: d.certificate.value_id,
                        commit_signatures: d
                            .certificate
                            .commit_signatures
                            .into_iter()
                            .map(|s| RawCommitSignature {
                                address: s.address,
                                signature: s.signature,
                            })
                            .collect(),
                    },
                }),
            }),
        }
    }
}

impl From<RawResponse> for Response<OpenHlContext> {
    fn from(raw: RawResponse) -> Self {
        match raw {
            RawResponse::ValueResponse(vr) => Self::ValueResponse(ValueResponse::new(
                vr.height,
                vr.value.map(|d| RawDecidedValue {
                    value_bytes: d.value_bytes,
                    certificate: CommitCertificate {
                        height: d.certificate.height,
                        round: d.certificate.round,
                        value_id: d.certificate.value_id,
                        commit_signatures: d
                            .certificate
                            .commit_signatures
                            .into_iter()
                            .map(|s| CommitSignature {
                                address: s.address,
                                signature: s.signature,
                            })
                            .collect(),
                    },
                }),
            )),
        }
    }
}

impl Codec<Response<OpenHlContext>> for OpenHlCodec {
    type Error = CodecError;

    fn decode(&self, bytes: Bytes) -> Result<Response<OpenHlContext>, Self::Error> {
        let raw: RawResponse = serde_json::from_slice(&bytes)?;
        Ok(raw.into())
    }

    fn encode(&self, msg: &Response<OpenHlContext>) -> Result<Bytes, Self::Error> {
        let raw = RawResponse::from(msg.clone());
        Ok(Bytes::from(serde_json::to_vec(&raw)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use informalsystems_malachitebft_app::types::codec::{
        ConsensusCodec, SyncCodec, WalCodec,
    };
    use informalsystems_malachitebft_signing_ed25519::PrivateKey;
    use rand::rngs::OsRng;

    fn assert_wal_codec<C: WalCodec<OpenHlContext>>() {}
    fn assert_consensus_codec<C: ConsensusCodec<OpenHlContext>>() {}
    fn assert_sync_codec<C: SyncCodec<OpenHlContext>>() {}

    #[test]
    fn openhl_codec_satisfies_all_three_super_traits() {
        assert_wal_codec::<OpenHlCodec>();
        assert_consensus_codec::<OpenHlCodec>();
        assert_sync_codec::<OpenHlCodec>();
    }

    fn sample_vote() -> OpenHlVote {
        OpenHlVote {
            height: OpenHlHeight(7),
            round: Round::new(2),
            value_id: NilOrVal::Val(BlockHash([0x42; 32])),
            vote_type: VoteType::Prevote,
            address: OpenHlAddress([0xaa; 20]),
        }
    }

    fn sample_proposal() -> OpenHlProposal {
        OpenHlProposal {
            height: OpenHlHeight(7),
            round: Round::new(2),
            value: OpenHlValue(BlockHash([0x11; 32])),
            pol_round: Round::Nil,
            address: OpenHlAddress([0xbb; 20]),
        }
    }

    fn sample_signature() -> Signature {
        let sk = PrivateKey::generate(OsRng);
        sk.sign(b"hello")
    }

    #[test]
    fn proposal_part_round_trips() {
        let codec = OpenHlCodec;
        let part = OpenHlProposalPart {
            height: OpenHlHeight(11),
            round: Round::new(0),
            pol_round: Round::Nil,
            proposer: OpenHlAddress([0xab; 20]),
            block_bytes: vec![0xde, 0xad, 0xbe, 0xef],
        };
        let bytes = Codec::<OpenHlProposalPart>::encode(&codec, &part).unwrap();
        let decoded: OpenHlProposalPart = codec.decode(bytes).unwrap();
        assert_eq!(decoded, part);
    }

    #[test]
    fn signed_consensus_msg_vote_round_trips() {
        let codec = OpenHlCodec;
        let msg = SignedConsensusMsg::<OpenHlContext>::Vote(SignedMessage::new(
            sample_vote(),
            sample_signature(),
        ));
        let bytes = codec.encode(&msg).unwrap();
        let decoded: SignedConsensusMsg<OpenHlContext> = codec.decode(bytes).unwrap();
        match (msg, decoded) {
            (SignedConsensusMsg::Vote(a), SignedConsensusMsg::Vote(b)) => {
                assert_eq!(a.message, b.message);
                assert_eq!(a.signature, b.signature);
            }
            _ => panic!("variant mismatch"),
        }
    }

    #[test]
    fn signed_consensus_msg_proposal_round_trips() {
        let codec = OpenHlCodec;
        let msg = SignedConsensusMsg::<OpenHlContext>::Proposal(SignedMessage::new(
            sample_proposal(),
            sample_signature(),
        ));
        let bytes = codec.encode(&msg).unwrap();
        let decoded: SignedConsensusMsg<OpenHlContext> = codec.decode(bytes).unwrap();
        match (msg, decoded) {
            (SignedConsensusMsg::Proposal(a), SignedConsensusMsg::Proposal(b)) => {
                assert_eq!(a.message, b.message);
                assert_eq!(a.signature, b.signature);
            }
            _ => panic!("variant mismatch"),
        }
    }

    #[test]
    fn liveness_vote_round_trips() {
        let codec = OpenHlCodec;
        let msg = LivenessMsg::<OpenHlContext>::Vote(SignedMessage::new(
            sample_vote(),
            sample_signature(),
        ));
        let bytes = codec.encode(&msg).unwrap();
        let decoded: LivenessMsg<OpenHlContext> = codec.decode(bytes).unwrap();
        match (msg, decoded) {
            (LivenessMsg::Vote(a), LivenessMsg::Vote(b)) => {
                assert_eq!(a.message, b.message);
                assert_eq!(a.signature, b.signature);
            }
            _ => panic!("variant mismatch"),
        }
    }

    #[test]
    fn proposed_value_round_trips() {
        let codec = OpenHlCodec;
        let msg = ProposedValue::<OpenHlContext> {
            height: OpenHlHeight(9),
            round: Round::new(0),
            valid_round: Round::Nil,
            proposer: OpenHlAddress([0xcc; 20]),
            value: OpenHlValue(BlockHash([0x77; 32])),
            validity: Validity::Valid,
        };
        let bytes = codec.encode(&msg).unwrap();
        let decoded: ProposedValue<OpenHlContext> = codec.decode(bytes).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn sync_request_round_trips() {
        let codec = OpenHlCodec;
        let msg = Request::<OpenHlContext>::ValueRequest(ValueRequest::new(OpenHlHeight(5)));
        let bytes = codec.encode(&msg).unwrap();
        let decoded: Request<OpenHlContext> = codec.decode(bytes).unwrap();
        match (msg, decoded) {
            (Request::ValueRequest(a), Request::ValueRequest(b)) => {
                assert_eq!(a.height, b.height);
            }
        }
    }

    #[test]
    fn stream_message_fin_round_trips() {
        let codec = OpenHlCodec;
        let msg = StreamMessage::<OpenHlProposalPart> {
            stream_id: StreamId::new(Bytes::from_static(b"openhl-stream-42")),
            sequence: 11,
            content: StreamContent::Fin,
        };
        let bytes = codec.encode(&msg).unwrap();
        let decoded: StreamMessage<OpenHlProposalPart> = codec.decode(bytes).unwrap();
        assert_eq!(decoded.stream_id, msg.stream_id);
        assert_eq!(decoded.sequence, msg.sequence);
        assert!(matches!(decoded.content, StreamContent::Fin));
    }
}
