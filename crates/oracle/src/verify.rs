//! ECDSA signature verification (Stage 11b).
//!
//! Wraps `k256::ecdsa` so the rest of the crate doesn't depend on
//! `k256` types directly. Two responsibilities:
//!
//!   - [`verify_observation`] — confirm that a [`PriceObservation`]'s
//!     [`Signature`] was produced by the holder of the private key
//!     matching the given [`PublisherKey`].
//!   - [`test_sign_observation`] (test-only) — produce a valid
//!     signature for a fresh observation. Used in this crate's tests
//!     and would be removed (or feature-gated) once an external
//!     `openhl-oracle-test-utils` crate exists.
//!
//! ### Hash convention
//!
//! `k256::ecdsa::VerifyingKey::verify(msg, sig)` hashes `msg` with
//! SHA-256 internally (RFC 6979 default). We feed the raw 20-byte
//! [`PriceObservation::signed_bytes`] payload to `verify` rather than
//! pre-hashing — this keeps the publisher side simple too (they pass
//! the same 20 bytes to their `sign(...)` call).
//!
//! ### Why ECDSA over secp256k1 and not Ed25519
//!
//! openhl is an Ethereum-shape L1 (Reth + alloy). Every publisher
//! integration in the broader Ethereum ecosystem already has
//! secp256k1 keys; reusing that curve avoids forcing publishers to
//! manage a second key.

use crate::types::{PriceObservation, PublisherKey};
use k256::ecdsa::{signature::Verifier, VerifyingKey};

/// Verify that `observation.signature` is a valid ECDSA signature
/// over `observation.signed_bytes()` produced by the private key
/// matching `pubkey`.
///
/// Returns `true` on success, `false` on any failure — malformed key,
/// malformed signature, or mismatched signer. The caller translates
/// the bool into an [`crate::types::ObservationError`] variant
/// ([`crate::types::ObservationError::InvalidSignature`] in
/// [`crate::state::OracleState::ingest_signed`]).
#[must_use]
pub fn verify_observation(
    observation: &PriceObservation,
    pubkey: &PublisherKey,
) -> bool {
    let Ok(verifying_key) = VerifyingKey::from_sec1_bytes(&pubkey.0) else {
        return false;
    };
    let Ok(signature) = k256::ecdsa::Signature::from_slice(&observation.signature.0) else {
        return false;
    };
    verifying_key
        .verify(&observation.signed_bytes(), &signature)
        .is_ok()
}

#[cfg(test)]
pub(crate) mod test_signing {
    //! Test-only signing helpers. Publishers run external code; openhl
    //! only needs to verify, never sign — but tests need to produce
    //! authentic signed observations to exercise the verify path.

    use super::*;
    use crate::types::{FeedId, PriceObservation, Signature};
    use k256::ecdsa::{signature::Signer, SigningKey};
    use openhl_funding::IndexPrice;

    /// Build a [`SigningKey`] from a deterministic 32-byte seed. Used
    /// by tests that need a reproducible publisher identity.
    pub(crate) fn test_signing_key(seed: u8) -> SigningKey {
        // SEC1 secret key bytes must be a non-zero scalar < group order.
        // A buffer of `seed` repeated 32 times is well within range for
        // any 1..=u8::MAX seed, but reject 0 explicitly for clarity.
        assert!(seed != 0, "test_signing_key seed must be non-zero");
        let bytes = [seed; 32];
        SigningKey::from_slice(&bytes).expect("seed bytes form a valid secp256k1 scalar")
    }

    /// Public key (SEC1-compressed, 33 bytes) corresponding to a test
    /// signing key.
    pub(crate) fn test_publisher_key(seed: u8) -> PublisherKey {
        let sk = test_signing_key(seed);
        let vk = sk.verifying_key();
        let compressed = vk.to_encoded_point(true);
        let bytes = compressed.as_bytes();
        let mut out = [0u8; 33];
        out.copy_from_slice(bytes);
        PublisherKey(out)
    }

    /// Sign a fresh observation with the given signing key. Returns the
    /// observation with `signature` populated.
    pub(crate) fn sign_observation(
        feed: FeedId,
        price: IndexPrice,
        timestamp: u64,
        signing_key: &SigningKey,
    ) -> PriceObservation {
        let unsigned = PriceObservation::unsigned(feed, price, timestamp);
        let msg = unsigned.signed_bytes();
        let sig: k256::ecdsa::Signature = signing_key.sign(&msg);
        let bytes = sig.to_bytes();
        let mut sig_array = [0u8; 64];
        sig_array.copy_from_slice(&bytes);
        PriceObservation {
            signature: Signature(sig_array),
            ..unsigned
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_signing::{sign_observation, test_publisher_key, test_signing_key};
    use super::*;
    use crate::types::{FeedId, PriceObservation, Signature};
    use openhl_funding::IndexPrice;

    #[test]
    fn round_trip_verifies() {
        // Standard happy path: sign with seed=1, verify with the
        // corresponding pubkey.
        let sk = test_signing_key(1);
        let pk = test_publisher_key(1);
        let obs = sign_observation(FeedId(7), IndexPrice(100), 1000, &sk);
        assert!(verify_observation(&obs, &pk));
    }

    #[test]
    fn wrong_pubkey_rejected() {
        // Signed with key 1, verified with key 2 → fail.
        let sk = test_signing_key(1);
        let pk_other = test_publisher_key(2);
        let obs = sign_observation(FeedId(7), IndexPrice(100), 1000, &sk);
        assert!(!verify_observation(&obs, &pk_other));
    }

    #[test]
    fn tampered_price_rejected() {
        let sk = test_signing_key(1);
        let pk = test_publisher_key(1);
        let mut obs = sign_observation(FeedId(7), IndexPrice(100), 1000, &sk);
        // Mutate the price; signature now signs a stale payload.
        obs.price = IndexPrice(999);
        assert!(!verify_observation(&obs, &pk));
    }

    #[test]
    fn tampered_timestamp_rejected() {
        let sk = test_signing_key(1);
        let pk = test_publisher_key(1);
        let mut obs = sign_observation(FeedId(7), IndexPrice(100), 1000, &sk);
        obs.timestamp = 2000;
        assert!(!verify_observation(&obs, &pk));
    }

    #[test]
    fn tampered_feed_id_rejected() {
        let sk = test_signing_key(1);
        let pk = test_publisher_key(1);
        let mut obs = sign_observation(FeedId(7), IndexPrice(100), 1000, &sk);
        obs.feed = FeedId(99);
        assert!(!verify_observation(&obs, &pk));
    }

    #[test]
    fn malformed_signature_rejected() {
        let pk = test_publisher_key(1);
        let obs = PriceObservation {
            feed: FeedId(7),
            price: IndexPrice(100),
            timestamp: 1000,
            signature: Signature([0xFF; 64]), // not a valid (r, s)
        };
        assert!(!verify_observation(&obs, &pk));
    }

    #[test]
    fn malformed_pubkey_rejected() {
        let sk = test_signing_key(1);
        let bad_pk = PublisherKey([0xFF; 33]); // not a valid SEC1-compressed point
        let obs = sign_observation(FeedId(7), IndexPrice(100), 1000, &sk);
        assert!(!verify_observation(&obs, &bad_pk));
    }

    #[test]
    fn zero_signature_rejected() {
        // The unsigned/trust-bridge sentinel must not pass the verify
        // path — otherwise the unsigned ingest path's invariants leak
        // into the signed path.
        let sk = test_signing_key(1);
        let pk = test_publisher_key(1);
        let unsigned = PriceObservation::unsigned(FeedId(7), IndexPrice(100), 1000);
        assert!(!verify_observation(&unsigned, &pk));
        // Sanity: a real signature for the same payload would verify.
        let real = sign_observation(FeedId(7), IndexPrice(100), 1000, &sk);
        assert!(verify_observation(&real, &pk));
    }
}
