//! Message envelope types per `specs/reeve-transport-security.md` §
//! Signed Message Envelope and `specs/reeve-domain-model.md` § Message
//! Envelope.
//!
//! An [`Envelope`] is the unit of communication between participants. It
//! carries all fields necessary for sender authentication, replay protection,
//! and body integrity verification. Signing and verification live in
//! `reeve-transport`; this module is pure data.
//!
//! # Wire encoding
//!
//! All byte fields (`nonce`, `payload_hash`, `signature`, `body`) are
//! base64-encoded in JSON using the unpadded standard alphabet — the same
//! alphabet `PublicKey` uses throughout the codebase. `created_at` uses
//! RFC 3339 format. UUIDs are hyphenated lowercase strings.
//!
//! # Validation at the boundary
//!
//! `Envelope::new` and the `Deserialize` impl are the two parse-don't-validate
//! entry points. Any `Envelope` value that exists in memory has already passed
//! all structural invariant checks; callers never need to re-validate.

use std::fmt;

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use time::{OffsetDateTime, UtcOffset};

use crate::id_newtype::uuid_v7_newtype;
use crate::identity::IdentityId;
use crate::key::KeyId;

/// Number of bytes in a [`Nonce`]. Sixteen bytes give 128 bits of entropy,
/// sufficient for replay-protection uniqueness within the retention window.
pub const NONCE_LEN: usize = 16;

/// Number of bytes in a [`PayloadHash`]. SHA-256 produces 32 bytes.
pub const PAYLOAD_HASH_LEN: usize = 32;

/// Number of bytes in an [`EnvelopeSignature`]. Ed25519 signatures are always
/// 64 bytes.
pub const SIGNATURE_LEN: usize = 64;

uuid_v7_newtype! {
    /// Globally unique identifier for a single message. `UUIDv7` per
    /// domain-model § Identifiers.
    ///
    /// Construct fresh IDs with [`MessageId::new`]; convert wire-form UUIDs
    /// through [`MessageId::try_from`], which rejects any other UUID version.
    pub MessageId,
    /// Errors that can occur when minting or wrapping a [`MessageId`].
    error MessageIdError,
    noun "message id",
}

/// Schema version discriminant for the signed envelope.
///
/// Only `V1` is valid at this revision. An unknown version number arriving
/// over the wire is rejected at the parse boundary so downstream code never
/// has to handle a future-version envelope it cannot interpret.
///
/// Wire form is a plain `u32` integer (e.g. `"schema_version": 1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SchemaVersion {
    /// Version 1 — the only version this build accepts; adding a new version
    /// requires a coordinated protocol change and a new enum arm.
    V1,
}

impl SchemaVersion {
    /// The `u32` wire representation of this version.
    pub fn as_u32(self) -> u32 {
        match self {
            Self::V1 => 1,
        }
    }
}

impl fmt::Display for SchemaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_u32())
    }
}

impl Serialize for SchemaVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(self.as_u32())
    }
}

impl<'de> Deserialize<'de> for SchemaVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let n = u32::deserialize(deserializer)?;
        match n {
            1 => Ok(Self::V1),
            _ => Err(de::Error::custom(EnvelopeError::UnsupportedSchemaVersion {
                actual: n,
            })),
        }
    }
}

/// 16-byte cryptographic nonce for replay protection.
///
/// Wire form is unpadded standard base64 (24 characters). Constructors reject
/// byte slices that are not exactly [`NONCE_LEN`] bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nonce([u8; NONCE_LEN]);

impl Nonce {
    /// Wrap a fixed-size byte array. Infallible because the length is already
    /// constrained by the type.
    pub fn from_bytes(bytes: [u8; NONCE_LEN]) -> Self {
        Self(bytes)
    }

    /// Parse from a byte slice of exactly [`NONCE_LEN`] bytes.
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, EnvelopeError> {
        let arr: [u8; NONCE_LEN] = bytes
            .try_into()
            .map_err(|_| EnvelopeError::InvalidNonceLength { len: bytes.len() })?;
        Ok(Self(arr))
    }

    /// Return the raw bytes.
    pub fn as_bytes(&self) -> &[u8; NONCE_LEN] {
        &self.0
    }
}

impl fmt::Debug for Nonce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Nonce({})", STANDARD_NO_PAD.encode(self.0))
    }
}

impl Serialize for Nonce {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD_NO_PAD.encode(self.0))
    }
}

impl<'de> Deserialize<'de> for Nonce {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let raw = STANDARD_NO_PAD
            .decode(encoded.as_bytes())
            .map_err(|_| de::Error::custom("nonce is not valid base64"))?;
        Self::try_from_slice(&raw).map_err(de::Error::custom)
    }
}

/// 32-byte SHA-256 content hash of the envelope body.
///
/// Wire form is unpadded standard base64 (43 characters). Constructors reject
/// byte slices that are not exactly [`PAYLOAD_HASH_LEN`] bytes.
///
/// The runtime verifies that this hash matches the body before delivery. Hash
/// computation lives in `reeve-transport`; this type is the container only.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PayloadHash([u8; PAYLOAD_HASH_LEN]);

impl PayloadHash {
    /// Wrap a fixed-size byte array. Infallible because the length is already
    /// constrained by the type.
    pub fn from_bytes(bytes: [u8; PAYLOAD_HASH_LEN]) -> Self {
        Self(bytes)
    }

    /// Parse from a byte slice of exactly [`PAYLOAD_HASH_LEN`] bytes.
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, EnvelopeError> {
        let arr: [u8; PAYLOAD_HASH_LEN] = bytes
            .try_into()
            .map_err(|_| EnvelopeError::InvalidPayloadHashLength { len: bytes.len() })?;
        Ok(Self(arr))
    }

    /// Return the raw bytes.
    pub fn as_bytes(&self) -> &[u8; PAYLOAD_HASH_LEN] {
        &self.0
    }
}

impl fmt::Debug for PayloadHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PayloadHash({})", STANDARD_NO_PAD.encode(self.0))
    }
}

impl Serialize for PayloadHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD_NO_PAD.encode(self.0))
    }
}

impl<'de> Deserialize<'de> for PayloadHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let raw = STANDARD_NO_PAD
            .decode(encoded.as_bytes())
            .map_err(|_| de::Error::custom("payload_hash is not valid base64"))?;
        Self::try_from_slice(&raw).map_err(de::Error::custom)
    }
}

/// 64-byte ed25519 signature over the canonical envelope bytes (excluding the
/// signature field itself).
///
/// Wire form is unpadded standard base64 (86 characters). Constructors reject
/// byte slices that are not exactly [`SIGNATURE_LEN`] bytes.
///
/// Signature verification lives in `reeve-transport`. This type is the
/// container; it deliberately does not implement signing or verification.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct EnvelopeSignature([u8; SIGNATURE_LEN]);

impl EnvelopeSignature {
    /// Wrap a fixed-size byte array. Infallible because the length is already
    /// constrained by the type.
    pub fn from_bytes(bytes: [u8; SIGNATURE_LEN]) -> Self {
        Self(bytes)
    }

    /// Parse from a byte slice of exactly [`SIGNATURE_LEN`] bytes.
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, EnvelopeError> {
        let arr: [u8; SIGNATURE_LEN] = bytes
            .try_into()
            .map_err(|_| EnvelopeError::InvalidSignatureLength { len: bytes.len() })?;
        Ok(Self(arr))
    }

    /// Return the raw bytes.
    pub fn as_bytes(&self) -> &[u8; SIGNATURE_LEN] {
        &self.0
    }
}

impl fmt::Debug for EnvelopeSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EnvelopeSignature({})", STANDARD_NO_PAD.encode(self.0))
    }
}

impl Serialize for EnvelopeSignature {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD_NO_PAD.encode(self.0))
    }
}

impl<'de> Deserialize<'de> for EnvelopeSignature {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        let raw = STANDARD_NO_PAD
            .decode(encoded.as_bytes())
            .map_err(|_| de::Error::custom("signature is not valid base64"))?;
        Self::try_from_slice(&raw).map_err(de::Error::custom)
    }
}

/// Errors that can occur when constructing or deserializing an [`Envelope`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnvelopeError {
    /// The `schema_version` field carries a version number this build does
    /// not recognise. The unknown version number is preserved for logging.
    UnsupportedSchemaVersion { actual: u32 },
    /// The byte slice supplied as a nonce was not exactly [`NONCE_LEN`] bytes.
    InvalidNonceLength { len: usize },
    /// The byte slice supplied as a payload hash was not exactly
    /// [`PAYLOAD_HASH_LEN`] bytes.
    InvalidPayloadHashLength { len: usize },
    /// The byte slice supplied as a signature was not exactly
    /// [`SIGNATURE_LEN`] bytes.
    InvalidSignatureLength { len: usize },
}

impl fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion { actual } => {
                write!(
                    f,
                    "unsupported schema version {actual}; only version 1 is accepted"
                )
            }
            Self::InvalidNonceLength { len } => {
                write!(f, "nonce must be exactly {NONCE_LEN} bytes; got {len}")
            }
            Self::InvalidPayloadHashLength { len } => {
                write!(
                    f,
                    "payload_hash must be exactly {PAYLOAD_HASH_LEN} bytes; got {len}"
                )
            }
            Self::InvalidSignatureLength { len } => {
                write!(
                    f,
                    "signature must be exactly {SIGNATURE_LEN} bytes; got {len}"
                )
            }
        }
    }
}

impl std::error::Error for EnvelopeError {}

/// The signed message envelope — the unit of communication between all Reeve
/// participants.
///
/// Every field maps directly to the schema defined in
/// `specs/reeve-transport-security.md` § Signed Message Envelope. The
/// envelope is serialized as JSON for the walking skeleton; canonical JSON
/// serialization for signing lands in `reeve-transport`.
///
/// `#[serde(deny_unknown_fields)]` enforces the spec requirement that an
/// envelope with an unknown top-level field is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// Envelope schema version.
    pub schema_version: SchemaVersion,
    /// Globally unique message identifier (`UUIDv7`).
    pub message_id: MessageId,
    /// Identity ID of the signing sender.
    pub sender_id: IdentityId,
    /// Key record ID of the key that produced `signature`.
    pub sender_key_id: KeyId,
    /// Identity ID of the intended recipient agent.
    pub recipient_id: IdentityId,
    /// Wall-clock timestamp when the envelope was created (RFC 3339, always
    /// UTC). Normalized to `UtcOffset::UTC` at both wire parse and constructor
    /// so the in-memory value has a stable representation regardless of the
    /// sender's local offset.
    #[serde(with = "created_at_serde")]
    pub created_at: OffsetDateTime,
    /// 16-byte nonce for replay protection, base64-encoded in JSON.
    pub nonce: Nonce,
    /// SHA-256 hash of `body`, base64-encoded in JSON. Verified by the
    /// runtime before delivery.
    pub payload_hash: PayloadHash,
    /// Raw message body, base64-encoded in JSON.
    #[serde(with = "body_serde")]
    pub body: Vec<u8>,
    /// Ed25519 signature over the canonical envelope bytes (all fields
    /// except `signature`), base64-encoded in JSON.
    pub signature: EnvelopeSignature,
}

impl Envelope {
    /// All typed fields have already passed their individual length / version
    /// checks at construction; this constructor assembles them without
    /// re-validating.
    ///
    /// The caller is responsible for supplying a `signature` that covers the
    /// canonical serialization and a `payload_hash` that matches `body`; both
    /// invariants are verified by `reeve-transport` at delivery time.
    ///
    /// `created_at` is normalized to `UtcOffset::UTC` regardless of the offset
    /// supplied by the caller.
    #[expect(
        clippy::too_many_arguments,
        reason = "all ten fields are mandatory per spec"
    )]
    pub fn new(
        schema_version: SchemaVersion,
        message_id: MessageId,
        sender_id: IdentityId,
        sender_key_id: KeyId,
        recipient_id: IdentityId,
        created_at: OffsetDateTime,
        nonce: Nonce,
        payload_hash: PayloadHash,
        body: Vec<u8>,
        signature: EnvelopeSignature,
    ) -> Self {
        Self {
            schema_version,
            message_id,
            sender_id,
            sender_key_id,
            recipient_id,
            created_at: created_at.to_offset(UtcOffset::UTC),
            nonce,
            payload_hash,
            body,
            signature,
        }
    }
}

/// Serde module for `body: Vec<u8>` — encoded as unpadded standard base64 in
/// JSON, matching the wire encoding of all other byte fields.
mod body_serde {
    use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
    use serde::{de, Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        body: &Vec<u8>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD_NO_PAD.encode(body))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<u8>, D::Error> {
        let encoded = String::deserialize(deserializer)?;
        STANDARD_NO_PAD
            .decode(encoded.as_bytes())
            .map_err(|_| de::Error::custom("body is not valid base64"))
    }
}

/// Serde module for `created_at: OffsetDateTime` — RFC 3339 on the wire,
/// normalized to `UtcOffset::UTC` on deserialize so the in-memory value is
/// always UTC regardless of the sender's local offset.
mod created_at_serde {
    use serde::{Deserializer, Serializer};
    use time::serde::rfc3339;
    use time::{OffsetDateTime, UtcOffset};

    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde with-module serialize requires &T"
    )]
    pub(super) fn serialize<S: Serializer>(t: &OffsetDateTime, s: S) -> Result<S::Ok, S::Error> {
        rfc3339::serialize(&t.to_offset(UtcOffset::UTC), s)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<OffsetDateTime, D::Error> {
        let parsed = rfc3339::deserialize(d)?;
        Ok(parsed.to_offset(UtcOffset::UTC))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use time::macros::datetime;

    fn sample_nonce() -> Nonce {
        Nonce::from_bytes([0xAB; NONCE_LEN])
    }

    fn sample_hash() -> PayloadHash {
        PayloadHash::from_bytes([0xCD; PAYLOAD_HASH_LEN])
    }

    fn sample_sig() -> EnvelopeSignature {
        EnvelopeSignature::from_bytes([0xEF; SIGNATURE_LEN])
    }

    fn sample_envelope() -> Envelope {
        Envelope::new(
            SchemaVersion::V1,
            MessageId::new().unwrap(),
            IdentityId::new().unwrap(),
            KeyId::new().unwrap(),
            IdentityId::new().unwrap(),
            datetime!(2026-05-04 12:00:00 UTC),
            sample_nonce(),
            sample_hash(),
            b"hello reeve".to_vec(),
            sample_sig(),
        )
    }

    #[test]
    fn envelope_round_trip_json() {
        let env = sample_envelope();
        let json = serde_json::to_string(&env).unwrap();
        let decoded: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, env);
    }

    #[test]
    fn schema_version_serializes_as_integer_one() {
        let env = sample_envelope();
        let json = serde_json::to_string(&env).unwrap();
        assert!(
            json.contains("\"schema_version\":1"),
            "expected integer 1: {json}"
        );
    }

    #[test]
    fn envelope_deserialize_rejects_unknown_schema_version() {
        let env = sample_envelope();
        let mut value: serde_json::Value = serde_json::to_value(&env).unwrap();
        value["schema_version"] = serde_json::Value::Number(99.into());
        let result: Result<Envelope, _> = serde_json::from_value(value);
        let err = result.expect_err("schema_version 99 must be rejected");
        assert!(
            err.to_string().contains("unsupported schema version 99"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn envelope_deserialize_rejects_unknown_top_level_field() {
        let env = sample_envelope();
        let mut value: serde_json::Value = serde_json::to_value(&env).unwrap();
        value["unexpected_field"] = serde_json::Value::String("surprise".into());
        let result: Result<Envelope, _> = serde_json::from_value(value);
        assert!(result.is_err(), "unknown top-level field must be rejected");
    }

    #[test]
    fn nonce_try_from_slice_rejects_wrong_length() {
        let short = [0u8; NONCE_LEN - 1];
        let err = Nonce::try_from_slice(&short).unwrap_err();
        assert_eq!(
            err,
            EnvelopeError::InvalidNonceLength { len: NONCE_LEN - 1 }
        );

        let long = [0u8; NONCE_LEN + 1];
        let err = Nonce::try_from_slice(&long).unwrap_err();
        assert_eq!(
            err,
            EnvelopeError::InvalidNonceLength { len: NONCE_LEN + 1 }
        );
    }

    #[test]
    fn payload_hash_try_from_slice_rejects_wrong_length() {
        let short = [0u8; PAYLOAD_HASH_LEN - 1];
        let err = PayloadHash::try_from_slice(&short).unwrap_err();
        assert_eq!(
            err,
            EnvelopeError::InvalidPayloadHashLength {
                len: PAYLOAD_HASH_LEN - 1
            }
        );

        let long = [0u8; PAYLOAD_HASH_LEN + 1];
        let err = PayloadHash::try_from_slice(&long).unwrap_err();
        assert_eq!(
            err,
            EnvelopeError::InvalidPayloadHashLength {
                len: PAYLOAD_HASH_LEN + 1
            }
        );
    }

    #[test]
    fn envelope_signature_try_from_slice_rejects_wrong_length() {
        let short = [0u8; SIGNATURE_LEN - 1];
        let err = EnvelopeSignature::try_from_slice(&short).unwrap_err();
        assert_eq!(
            err,
            EnvelopeError::InvalidSignatureLength {
                len: SIGNATURE_LEN - 1
            }
        );

        let long = [0u8; SIGNATURE_LEN + 1];
        let err = EnvelopeSignature::try_from_slice(&long).unwrap_err();
        assert_eq!(
            err,
            EnvelopeError::InvalidSignatureLength {
                len: SIGNATURE_LEN + 1
            }
        );
    }

    #[test]
    fn message_id_is_uuid_v7() {
        let id = MessageId::new().unwrap();
        assert_eq!(id.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn message_id_display_is_deterministic_uuid_string() {
        let id = MessageId::new().unwrap();
        let rendered = id.to_string();
        let parsed: uuid::Uuid = rendered.parse().unwrap();
        assert_eq!(parsed, *id.as_uuid());
    }

    #[test]
    fn message_id_serde_round_trip() {
        let id = MessageId::new().unwrap();
        let json = serde_json::to_string(&id).unwrap();
        let decoded: MessageId = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, id);
    }

    #[test]
    fn two_message_ids_are_distinct() {
        let a = MessageId::new().unwrap();
        let b = MessageId::new().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn body_round_trips_empty() {
        let mut env = sample_envelope();
        env.body = Vec::new();
        let json = serde_json::to_string(&env).unwrap();
        let decoded: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.body, Vec::<u8>::new());
    }

    #[test]
    fn body_round_trips_large() {
        let mut env = sample_envelope();
        env.body = vec![0xFFu8; 65_536];
        let json = serde_json::to_string(&env).unwrap();
        let decoded: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.body.len(), 65_536);
        assert!(decoded.body.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn created_at_rfc3339_round_trip() {
        let env = sample_envelope();
        let json = serde_json::to_string(&env).unwrap();
        assert!(
            json.contains("2026-05-04"),
            "expected RFC 3339 date in JSON: {json}"
        );
        let decoded: Envelope = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.created_at, env.created_at);
    }

    #[test]
    fn nonce_base64_is_unpadded() {
        let nonce = sample_nonce();
        let json = serde_json::to_string(&nonce).unwrap();
        assert!(!json.contains('='), "expected unpadded base64: {json}");
    }

    #[test]
    fn payload_hash_base64_is_unpadded() {
        let hash = sample_hash();
        let json = serde_json::to_string(&hash).unwrap();
        assert!(!json.contains('='), "expected unpadded base64: {json}");
    }

    #[test]
    fn signature_base64_is_unpadded() {
        let sig = sample_sig();
        let json = serde_json::to_string(&sig).unwrap();
        assert!(!json.contains('='), "expected unpadded base64: {json}");
    }

    #[test]
    fn envelope_error_display_unsupported_version() {
        let err = EnvelopeError::UnsupportedSchemaVersion { actual: 42 };
        assert!(
            err.to_string().contains("42"),
            "expected version number in error: {err}"
        );
        assert!(
            err.to_string().contains("unsupported schema version"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn envelope_error_display_invalid_nonce_length() {
        let err = EnvelopeError::InvalidNonceLength { len: 5 };
        assert!(
            err.to_string().contains('5'),
            "expected length in error: {err}"
        );
    }

    #[test]
    fn envelope_error_display_invalid_payload_hash_length() {
        let err = EnvelopeError::InvalidPayloadHashLength { len: 10 };
        assert!(
            err.to_string().contains("10"),
            "expected length in error: {err}"
        );
    }

    #[test]
    fn envelope_error_display_invalid_signature_length() {
        let err = EnvelopeError::InvalidSignatureLength { len: 20 };
        assert!(
            err.to_string().contains("20"),
            "expected length in error: {err}"
        );
    }

    #[test]
    fn schema_version_v1_as_u32_is_one() {
        assert_eq!(SchemaVersion::V1.as_u32(), 1);
    }

    #[test]
    fn schema_version_display_matches_wire_form() {
        assert_eq!(SchemaVersion::V1.to_string(), "1");
    }

    #[test]
    fn message_id_error_display_uses_message_id_noun() {
        let err = MessageIdError::NotV7 { actual_version: 4 };
        assert!(
            err.to_string().contains("message id"),
            "expected 'message id' in error: {err}"
        );
    }

    #[test]
    fn nonce_from_bytes_as_bytes_round_trip() {
        let raw = [0x12u8; NONCE_LEN];
        let nonce = Nonce::from_bytes(raw);
        assert_eq!(nonce.as_bytes(), &raw);
    }

    #[test]
    fn payload_hash_from_bytes_as_bytes_round_trip() {
        let raw = [0x34u8; PAYLOAD_HASH_LEN];
        let hash = PayloadHash::from_bytes(raw);
        assert_eq!(hash.as_bytes(), &raw);
    }

    #[test]
    fn envelope_signature_from_bytes_as_bytes_round_trip() {
        let raw = [0x56u8; SIGNATURE_LEN];
        let sig = EnvelopeSignature::from_bytes(raw);
        assert_eq!(sig.as_bytes(), &raw);
    }

    #[test]
    fn nonce_deserialize_rejects_base64_with_wrong_decoded_length() {
        // Valid base64 that decodes to 15 bytes (one short of NONCE_LEN).
        let short = STANDARD_NO_PAD.encode([0u8; NONCE_LEN - 1]);
        let json = format!("\"{short}\"");
        let result: Result<Nonce, _> = serde_json::from_str(&json);
        assert!(
            result.is_err(),
            "base64 decoding to wrong length must be rejected"
        );
    }

    #[test]
    fn payload_hash_deserialize_rejects_base64_with_wrong_decoded_length() {
        let short = STANDARD_NO_PAD.encode([0u8; PAYLOAD_HASH_LEN - 1]);
        let json = format!("\"{short}\"");
        let result: Result<PayloadHash, _> = serde_json::from_str(&json);
        assert!(
            result.is_err(),
            "base64 decoding to wrong length must be rejected"
        );
    }

    #[test]
    fn envelope_signature_deserialize_rejects_base64_with_wrong_decoded_length() {
        let short = STANDARD_NO_PAD.encode([0u8; SIGNATURE_LEN - 1]);
        let json = format!("\"{short}\"");
        let result: Result<EnvelopeSignature, _> = serde_json::from_str(&json);
        assert!(
            result.is_err(),
            "base64 decoding to wrong length must be rejected"
        );
    }

    #[test]
    fn envelope_deserialize_rejects_invalid_base64_body() {
        let env = sample_envelope();
        let mut value: serde_json::Value = serde_json::to_value(&env).unwrap();
        value["body"] = serde_json::Value::String("not base64!!!".to_owned());
        let result: Result<Envelope, _> = serde_json::from_value(value);
        let err = result.expect_err("invalid base64 body must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("base64") || msg.contains("invalid"),
            "expected base64 error, got: {msg}"
        );
    }

    #[test]
    fn envelope_deserialize_rejects_non_v7_message_id() {
        let env = sample_envelope();
        let mut value: serde_json::Value = serde_json::to_value(&env).unwrap();
        value["message_id"] =
            serde_json::Value::String("550e8400-e29b-41d4-a716-446655440000".to_owned());
        let result: Result<Envelope, _> = serde_json::from_value(value);
        let err = result.expect_err("non-v7 message_id must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("UUIDv7") || msg.contains("v7") || msg.contains("message id"),
            "expected UUID version error, got: {msg}"
        );
    }

    #[test]
    fn created_at_with_nonzero_utc_offset_round_trips_via_serde() {
        use time::UtcOffset;
        // Build a valid envelope JSON and then mutate `created_at` to carry a
        // non-UTC offset (+05:30) before deserializing. This exercises the
        // `created_at_serde::deserialize` normalization path directly.
        let env = sample_envelope();
        let mut value: serde_json::Value = serde_json::to_value(&env).unwrap();
        // Replace with the same instant expressed in +05:30.
        value["created_at"] = serde_json::Value::String("2026-05-04T17:30:00+05:30".to_owned());
        let decoded: Envelope = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.created_at.offset(), UtcOffset::UTC);
        assert_eq!(
            decoded.created_at.unix_timestamp(),
            datetime!(2026-05-04 12:00:00 UTC).unix_timestamp()
        );
    }

    #[test]
    fn envelope_new_normalizes_non_utc_created_at() {
        use time::UtcOffset;
        let offset = UtcOffset::from_hms(5, 30, 0).unwrap();
        let non_utc = datetime!(2026-05-04 12:00:00 UTC).to_offset(offset);
        let env = Envelope::new(
            SchemaVersion::V1,
            MessageId::new().unwrap(),
            IdentityId::new().unwrap(),
            KeyId::new().unwrap(),
            IdentityId::new().unwrap(),
            non_utc,
            sample_nonce(),
            sample_hash(),
            b"test".to_vec(),
            sample_sig(),
        );
        assert_eq!(env.created_at.offset(), UtcOffset::UTC);
        assert_eq!(
            env.created_at.unix_timestamp(),
            datetime!(2026-05-04 12:00:00 UTC).unix_timestamp()
        );
    }
}
