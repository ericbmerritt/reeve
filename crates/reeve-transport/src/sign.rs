//! Ed25519 sign and verify for signed message envelopes.
//!
//! [`sign_envelope`] computes [`canonical_bytes`] over an [`Envelope`]
//! (excluding the `signature` field) and produces the 64-byte
//! [`EnvelopeSignature`] the caller embeds in the final envelope.
//!
//! [`verify_envelope`] recomputes the same canonical bytes and calls
//! `VerifyingKey::verify_strict` — the RFC 8032 §5.1.7 strict path that
//! rejects malleable signatures, low-order public keys, and other
//! non-canonical forms. The lax `verify` is not used.
//!
//! Neither function caches canonical bytes; correctness over cleverness.
//!
//! Note: this layer signs and verifies the canonical bytes. The envelope's
//! `nonce` and `created_at` fields are replay-defense primitives bound by
//! the signature, but their semantic enforcement (uniqueness, freshness)
//! is the delivery layer's responsibility (Phase 4+).

use std::fmt;

use ed25519_dalek::Signature;

use reeve_types::{Envelope, EnvelopeSignature, PrivateKey, PublicKey};

use crate::canonical::{canonical_bytes, CanonicalError};

/// Errors that can occur while signing an envelope.
#[derive(Debug)]
#[non_exhaustive]
pub enum SignError {
    /// Canonical serialization failed before signing could proceed.
    Canonical(CanonicalError),
}

impl fmt::Display for SignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Canonical(err) => write!(f, "envelope signing failed: {err}"),
        }
    }
}

impl std::error::Error for SignError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Canonical(err) => Some(err),
        }
    }
}

/// Errors that can occur while verifying an envelope.
#[derive(Debug)]
#[non_exhaustive]
pub enum VerifyError {
    /// Canonical serialization failed before verification could proceed.
    Canonical(CanonicalError),
    /// The signature bytes in the envelope did not pass ed25519 strict
    /// verification against the supplied public key.
    SignatureInvalid,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Canonical(err) => write!(f, "envelope verification failed: {err}"),
            Self::SignatureInvalid => {
                f.write_str("envelope signature is invalid or does not match the public key")
            }
        }
    }
}

impl std::error::Error for VerifyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Canonical(err) => Some(err),
            Self::SignatureInvalid => None,
        }
    }
}

/// Sign `envelope` with `private_key` and return the resulting signature.
///
/// Ed25519 is deterministic per RFC 8032 §5.1.6: signing the same envelope
/// with the same key always produces identical bytes. Callers can therefore
/// safely cache or compare signatures across invocations.
///
/// The signature covers the canonical JSON bytes of all envelope fields
/// **except** `signature` itself. The caller is responsible for embedding
/// the returned [`EnvelopeSignature`] into the final [`Envelope`].
///
/// `envelope.signature` is not read; any placeholder value is acceptable
/// because it is stripped during canonicalization.
///
/// # Errors
///
/// Returns [`SignError::Canonical`] if canonical serialization fails. For
/// well-typed [`Envelope`] values this path is unreachable in practice.
pub fn sign_envelope(
    envelope: &Envelope,
    private_key: &PrivateKey,
) -> Result<EnvelopeSignature, SignError> {
    let bytes = canonical_bytes(envelope).map_err(SignError::Canonical)?;
    let signature: Signature = private_key.sign(&bytes);
    Ok(EnvelopeSignature::from_bytes(signature.to_bytes()))
}

/// Verify that `envelope.signature` was produced by `verifying_key` over the
/// envelope's canonical bytes.
///
/// Uses `VerifyingKey::verify_strict` (RFC 8032 §5.1.7) to reject malleable
/// signatures, low-order public keys, and other non-canonical forms.
///
/// # Errors
///
/// - [`VerifyError::Canonical`] — canonical serialization failed.
/// - [`VerifyError::SignatureInvalid`] — the signature did not verify.
pub fn verify_envelope(envelope: &Envelope, verifying_key: &PublicKey) -> Result<(), VerifyError> {
    let bytes = canonical_bytes(envelope).map_err(VerifyError::Canonical)?;
    let signature = Signature::from_bytes(envelope.signature.as_bytes());
    verifying_key
        .as_verifying_key()
        .verify_strict(&bytes, &signature)
        .map_err(|_| VerifyError::SignatureInvalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    use time::{macros::datetime, Duration};
    use uuid::Uuid;

    use reeve_types::{
        EnvelopeSignature, IdentityId, KeyId, Keypair, MessageId, Nonce, PayloadHash,
        SchemaVersion, NONCE_LEN, PAYLOAD_HASH_LEN, SIGNATURE_LEN,
    };

    fn fixed_envelope() -> Envelope {
        let msg_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000001").unwrap();
        let sender_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000002").unwrap();
        let key_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000003").unwrap();
        let recipient_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000004").unwrap();

        let message_id = MessageId::try_from(msg_uuid).unwrap();
        let sender_id = IdentityId::try_from(sender_uuid).unwrap();
        let sender_key_id = KeyId::try_from(key_uuid).unwrap();
        let recipient_id = IdentityId::try_from(recipient_uuid).unwrap();

        let nonce = Nonce::from_bytes([0xAA; NONCE_LEN]);
        let payload_hash = PayloadHash::from_bytes([0xBB; PAYLOAD_HASH_LEN]);
        let placeholder_sig = EnvelopeSignature::from_bytes([0x00; SIGNATURE_LEN]);

        Envelope::new(
            SchemaVersion::V1,
            message_id,
            sender_id,
            sender_key_id,
            recipient_id,
            datetime!(2026-05-04 00:00:00 UTC),
            nonce,
            payload_hash,
            b"hello reeve".to_vec(),
            placeholder_sig,
        )
    }

    fn signed_envelope(keypair: &Keypair) -> Envelope {
        let mut env = fixed_envelope();
        let sig = sign_envelope(&env, keypair.private()).unwrap();
        env.signature = sig;
        env
    }

    #[test]
    fn round_trip_sign_then_verify_succeeds() {
        let keypair = Keypair::generate();
        let env = signed_envelope(&keypair);
        verify_envelope(&env, keypair.public()).unwrap();
    }

    #[test]
    fn tamper_body_makes_verify_fail() {
        let keypair = Keypair::generate();
        let mut env = signed_envelope(&keypair);
        env.body[0] ^= 0x01;
        let result = verify_envelope(&env, keypair.public());
        assert!(
            matches!(result, Err(VerifyError::SignatureInvalid)),
            "tampered body must fail verification: {result:?}"
        );
    }

    #[test]
    fn tamper_message_id_makes_verify_fail() {
        let keypair = Keypair::generate();
        let signed = signed_envelope(&keypair);

        let other_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000099").unwrap();
        let other_id = MessageId::try_from(other_uuid).unwrap();

        let tampered = Envelope::new(
            signed.schema_version,
            other_id,
            signed.sender_id,
            signed.sender_key_id,
            signed.recipient_id,
            signed.created_at,
            signed.nonce,
            signed.payload_hash,
            signed.body.clone(),
            signed.signature,
        );

        let result = verify_envelope(&tampered, keypair.public());
        assert!(
            matches!(result, Err(VerifyError::SignatureInvalid)),
            "tampered message_id must fail verification: {result:?}"
        );
    }

    #[test]
    fn tamper_signature_bytes_makes_verify_fail() {
        let keypair = Keypair::generate();
        let signed = signed_envelope(&keypair);

        let mut sig_bytes = *signed.signature.as_bytes();
        sig_bytes[0] ^= 0x01;
        let bad_sig = EnvelopeSignature::from_bytes(sig_bytes);

        let tampered = Envelope::new(
            signed.schema_version,
            signed.message_id,
            signed.sender_id,
            signed.sender_key_id,
            signed.recipient_id,
            signed.created_at,
            signed.nonce,
            signed.payload_hash,
            signed.body.clone(),
            bad_sig,
        );

        let result = verify_envelope(&tampered, keypair.public());
        assert!(
            matches!(result, Err(VerifyError::SignatureInvalid)),
            "flipped signature byte must fail verification: {result:?}"
        );
    }

    #[test]
    fn wrong_key_makes_verify_fail() {
        let keypair_a = Keypair::generate();
        let keypair_b = Keypair::generate();

        let env = signed_envelope(&keypair_a);
        let result = verify_envelope(&env, keypair_b.public());
        assert!(
            matches!(result, Err(VerifyError::SignatureInvalid)),
            "wrong key must fail verification: {result:?}"
        );
    }

    // Note: a test for malleable-signature rejection (s-value beyond curve order)
    // is omitted because constructing such a signature requires raw
    // scalar manipulation outside ed25519-dalek's public API. `verify_strict`
    // is documented as rejecting these, and we trust dalek's own test suite to
    // cover that path.
    #[test]
    fn verify_uses_supplied_key_not_sender_key_id_field() {
        // sender_key_id in the fixed envelope is a placeholder UUID. Verification
        // binds to the supplied key, not to sender_key_id.
        let keypair = Keypair::generate();
        let env = signed_envelope(&keypair);
        verify_envelope(&env, keypair.public()).unwrap();
        let other = Keypair::generate();
        assert!(verify_envelope(&env, other.public()).is_err());
    }

    #[test]
    fn signing_is_deterministic() {
        let keypair = Keypair::generate();
        let env = fixed_envelope();

        let sig_a = sign_envelope(&env, keypair.private()).unwrap();
        let sig_b = sign_envelope(&env, keypair.private()).unwrap();

        assert_eq!(
            sig_a.as_bytes(),
            sig_b.as_bytes(),
            "ed25519 signing must be deterministic: signatures differ"
        );
    }

    #[test]
    fn sign_error_display_and_source() {
        use std::error::Error as _;

        use serde_json::Value;

        let raw_err: serde_json::Error = serde_json::from_str::<Value>("bad json").unwrap_err();
        let canonical_err = CanonicalError::Serialize(raw_err);
        let err = SignError::Canonical(canonical_err);

        let msg = err.to_string();
        assert!(
            msg.contains("envelope signing failed"),
            "unexpected display: {msg}"
        );
        assert!(
            err.source().is_some(),
            "SignError::Canonical must expose its source"
        );
    }

    #[test]
    fn verify_error_display_signature_invalid() {
        let err = VerifyError::SignatureInvalid;
        let msg = err.to_string();
        assert!(
            msg.contains("invalid") || msg.contains("signature"),
            "unexpected display: {msg}"
        );
    }

    #[test]
    fn verify_error_display_canonical_and_source() {
        use std::error::Error as _;

        use serde_json::Value;

        let raw_err: serde_json::Error = serde_json::from_str::<Value>("bad json").unwrap_err();
        let canonical_err = CanonicalError::Serialize(raw_err);
        let err = VerifyError::Canonical(canonical_err);

        let msg = err.to_string();
        assert!(
            msg.contains("envelope verification failed"),
            "unexpected display: {msg}"
        );
        assert!(
            err.source().is_some(),
            "VerifyError::Canonical must expose its source"
        );
    }

    #[test]
    fn tamper_sender_id_makes_verify_fail() {
        let keypair = Keypair::generate();
        let signed = signed_envelope(&keypair);

        let other_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000088").unwrap();
        let other_id = IdentityId::try_from(other_uuid).unwrap();

        let tampered = Envelope::new(
            signed.schema_version,
            signed.message_id,
            other_id,
            signed.sender_key_id,
            signed.recipient_id,
            signed.created_at,
            signed.nonce,
            signed.payload_hash,
            signed.body.clone(),
            signed.signature,
        );

        let result = verify_envelope(&tampered, keypair.public());
        assert!(
            matches!(result, Err(VerifyError::SignatureInvalid)),
            "tampered sender_id must fail verification: {result:?}"
        );
    }

    #[test]
    fn tamper_recipient_id_makes_verify_fail() {
        let keypair = Keypair::generate();
        let signed = signed_envelope(&keypair);

        let other_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000077").unwrap();
        let other_id = IdentityId::try_from(other_uuid).unwrap();

        let tampered = Envelope::new(
            signed.schema_version,
            signed.message_id,
            signed.sender_id,
            signed.sender_key_id,
            other_id,
            signed.created_at,
            signed.nonce,
            signed.payload_hash,
            signed.body.clone(),
            signed.signature,
        );

        let result = verify_envelope(&tampered, keypair.public());
        assert!(
            matches!(result, Err(VerifyError::SignatureInvalid)),
            "tampered recipient_id must fail verification: {result:?}"
        );
    }

    #[test]
    fn tamper_created_at_makes_verify_fail() {
        let keypair = Keypair::generate();
        let signed = signed_envelope(&keypair);

        let altered_time = signed.created_at + Duration::seconds(1);

        let tampered = Envelope::new(
            signed.schema_version,
            signed.message_id,
            signed.sender_id,
            signed.sender_key_id,
            signed.recipient_id,
            altered_time,
            signed.nonce,
            signed.payload_hash,
            signed.body.clone(),
            signed.signature,
        );

        let result = verify_envelope(&tampered, keypair.public());
        assert!(
            matches!(result, Err(VerifyError::SignatureInvalid)),
            "tampered created_at must fail verification: {result:?}"
        );
    }

    #[test]
    fn tamper_nonce_makes_verify_fail() {
        let keypair = Keypair::generate();
        let signed = signed_envelope(&keypair);

        let mut nonce_bytes = *signed.nonce.as_bytes();
        nonce_bytes[0] ^= 0x01;
        let altered_nonce = Nonce::from_bytes(nonce_bytes);

        let tampered = Envelope::new(
            signed.schema_version,
            signed.message_id,
            signed.sender_id,
            signed.sender_key_id,
            signed.recipient_id,
            signed.created_at,
            altered_nonce,
            signed.payload_hash,
            signed.body.clone(),
            signed.signature,
        );

        let result = verify_envelope(&tampered, keypair.public());
        assert!(
            matches!(result, Err(VerifyError::SignatureInvalid)),
            "tampered nonce must fail verification: {result:?}"
        );
    }

    #[test]
    fn tamper_payload_hash_makes_verify_fail() {
        let keypair = Keypair::generate();
        let signed = signed_envelope(&keypair);

        let mut hash_bytes = *signed.payload_hash.as_bytes();
        hash_bytes[0] ^= 0x01;
        let altered_hash = PayloadHash::from_bytes(hash_bytes);

        let tampered = Envelope::new(
            signed.schema_version,
            signed.message_id,
            signed.sender_id,
            signed.sender_key_id,
            signed.recipient_id,
            signed.created_at,
            signed.nonce,
            altered_hash,
            signed.body.clone(),
            signed.signature,
        );

        let result = verify_envelope(&tampered, keypair.public());
        assert!(
            matches!(result, Err(VerifyError::SignatureInvalid)),
            "tampered payload_hash must fail verification: {result:?}"
        );
    }

    #[test]
    fn empty_body_round_trip_succeeds() {
        let keypair = Keypair::generate();

        let msg_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000011").unwrap();
        let sender_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000012").unwrap();
        let key_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000013").unwrap();
        let recipient_uuid = Uuid::parse_str("01968e40-0000-7000-8000-000000000014").unwrap();

        let placeholder_sig = EnvelopeSignature::from_bytes([0x00; SIGNATURE_LEN]);
        let mut env = Envelope::new(
            SchemaVersion::V1,
            MessageId::try_from(msg_uuid).unwrap(),
            IdentityId::try_from(sender_uuid).unwrap(),
            KeyId::try_from(key_uuid).unwrap(),
            IdentityId::try_from(recipient_uuid).unwrap(),
            datetime!(2026-05-04 00:00:00 UTC),
            Nonce::from_bytes([0xCC; NONCE_LEN]),
            PayloadHash::from_bytes([0xDD; PAYLOAD_HASH_LEN]),
            Vec::new(),
            placeholder_sig,
        );

        let sig = sign_envelope(&env, keypair.private()).unwrap();
        env.signature = sig;

        verify_envelope(&env, keypair.public())
            .expect("empty body envelope must verify successfully");
    }
}
