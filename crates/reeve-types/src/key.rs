//! Key record and public key primitives per `specs/reeve-domain-model.md` §
//! Security Layer § Key Record and `specs/reeve-transport-security.md` §
//! Identity and Key Model.
//!
//! `PublicKey` wraps the 32-byte ed25519 verifying key. `KeyRecord` is the
//! durable on-disk representation of a single keypair entry attached to an
//! `Identity`.

use std::fmt::{self, Write as _};

use base64::{engine::general_purpose::STANDARD_NO_PAD, Engine as _};
use ed25519_dalek::{VerifyingKey, PUBLIC_KEY_LENGTH};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;

use crate::id_newtype::uuid_v7_newtype;
use crate::identity::IdentityId;

/// Number of leading public-key bytes rendered in a fingerprint. Eight bytes
/// produce a sixteen-character hex string, broken into colon-separated pairs
/// for readability — long enough that operator-visible collisions are
/// astronomically unlikely, short enough to fit a TUI line.
const FINGERPRINT_PREFIX_BYTES: usize = 8;

uuid_v7_newtype! {
    /// Stable, opaque identifier for a key record. `UUIDv7` per domain-model §
    /// Identifiers.
    ///
    /// Always wraps a `UUIDv7`. Construct fresh IDs with [`KeyId::new`];
    /// convert wire-form UUIDs through [`KeyId::try_from`], which rejects any
    /// other UUID version.
    pub KeyId,
    /// Errors that can occur when minting or wrapping a [`KeyId`].
    error KeyIdError,
    noun "key id",
}

/// 32-byte ed25519 verifying (public) key.
///
/// Wire format is unpadded standard base64 — chosen for compactness in the
/// signed envelope and stability across the canonical JSON representation.
/// All constructors validate that the bytes form a valid Edwards point so
/// downstream code never has to handle a "well-typed but unusable" public key.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PublicKey(VerifyingKey);

impl PublicKey {
    /// Validate and wrap a 32-byte public-key encoding.
    pub fn from_bytes(bytes: &[u8; PUBLIC_KEY_LENGTH]) -> Result<Self, PublicKeyDecodeError> {
        let verifying_key = VerifyingKey::from_bytes(bytes)
            .map_err(|err| PublicKeyDecodeError::NotOnCurve(err.to_string()))?;
        Ok(Self(verifying_key))
    }

    /// Return the 32 raw bytes of the verifying key.
    pub fn to_bytes(&self) -> [u8; PUBLIC_KEY_LENGTH] {
        self.0.to_bytes()
    }

    /// Encode the public key as unpadded standard base64.
    pub fn to_base64(&self) -> String {
        STANDARD_NO_PAD.encode(self.0.to_bytes())
    }

    /// Parse an unpadded standard base64 public key.
    pub fn from_base64(encoded: &str) -> Result<Self, PublicKeyDecodeError> {
        let decoded = STANDARD_NO_PAD
            .decode(encoded.as_bytes())
            .map_err(|_| PublicKeyDecodeError::InvalidBase64)?;
        let actual_len = decoded.len();
        let bytes: [u8; PUBLIC_KEY_LENGTH] = decoded
            .try_into()
            .map_err(|_| PublicKeyDecodeError::InvalidLength { actual: actual_len })?;
        Self::from_bytes(&bytes)
    }

    /// Return the human-readable fingerprint: colon-separated hex pairs of
    /// the leading [`FINGERPRINT_PREFIX_BYTES`] bytes (e.g.
    /// `aa:bb:cc:dd:ee:ff:00:11`). Stable across runs for a given key.
    ///
    /// **Display only.** Never use this value for equality checks, key
    /// lookup, or trust comparisons. The 64-bit prefix is short enough that
    /// adversarial prefix collisions are computationally feasible; full
    /// public-key bytes are the only safe identity comparator.
    pub fn fingerprint(&self) -> String {
        let bytes = self.0.to_bytes();
        let mut out = String::with_capacity(FINGERPRINT_PREFIX_BYTES * 3);
        for (idx, byte) in bytes.iter().take(FINGERPRINT_PREFIX_BYTES).enumerate() {
            if idx > 0 {
                out.push(':');
            }
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// Construct from an `ed25519_dalek::VerifyingKey`.
    pub fn from_verifying_key(verifying_key: VerifyingKey) -> Self {
        Self(verifying_key)
    }

    /// Borrow the underlying `ed25519_dalek::VerifyingKey`. Infallible: the
    /// type only ever wraps a validated key.
    pub fn as_verifying_key(&self) -> &VerifyingKey {
        &self.0
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base64())
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.to_base64())
    }
}

impl Serialize for PublicKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_base64())
    }
}

impl<'de> Deserialize<'de> for PublicKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::from_base64(&raw).map_err(de::Error::custom)
    }
}

/// Errors that can occur when decoding a [`PublicKey`] from its wire form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicKeyDecodeError {
    /// The input was not valid unpadded standard base64.
    InvalidBase64,
    /// The decoded bytes were not exactly 32 long.
    InvalidLength { actual: usize },
    /// The 32 bytes do not form a valid ed25519 verifying key (not on the
    /// Edwards curve, or otherwise rejected by the dalek decoder).
    NotOnCurve(String),
}

impl fmt::Display for PublicKeyDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBase64 => f.write_str("public key is not valid base64"),
            Self::InvalidLength { actual } => {
                write!(f, "public key is not 32 bytes: got {actual}")
            }
            Self::NotOnCurve(detail) => {
                write!(
                    f,
                    "public key bytes are not a valid ed25519 point: {detail}"
                )
            }
        }
    }
}

impl std::error::Error for PublicKeyDecodeError {}

/// Lifecycle state of a [`KeyRecord`].
///
/// A key record is exactly one of: actively trusted, deprecated with a hard
/// validity ceiling (deprecated keys verify only messages whose `created_at`
/// falls within the validity window), or revoked (verifies nothing).
/// Combinations like "active with validity ceiling" or "deprecated with no
/// ceiling" are not representable.
///
/// On disk the lifecycle is encoded as the flat shape `status` +
/// `valid_until` + `revoked_at` (per `specs/reeve-transport-security.md` §
/// Identity and Key Model); custom serde reconciles the flat form with this
/// in-memory sum type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    /// Currently trusted. No validity ceiling.
    Active,
    /// Trusted only for messages whose `created_at` is within
    /// `[valid_from, valid_until]`. Set when an identity rotates keys.
    Deprecated { valid_until: OffsetDateTime },
    /// Verifies nothing. Once revoked, stays revoked.
    Revoked { revoked_at: OffsetDateTime },
}

/// Errors that can occur when reconciling on-disk key lifecycle fields with
/// the in-memory [`KeyState`] sum type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyStateError {
    /// `status: active` may not carry a `valid_until`.
    ActiveWithValidUntil,
    /// `status: active` may not carry a `revoked_at`.
    ActiveWithRevokedAt,
    /// `status: deprecated` requires `valid_until` and forbids `revoked_at`.
    DeprecatedFlatFieldMismatch,
    /// `status: revoked` requires `revoked_at` and forbids `valid_until`.
    RevokedFlatFieldMismatch,
    /// `status: deprecated` carries a `valid_until` that is not strictly
    /// after `valid_from`. The deprecation window must be a real (non-empty,
    /// forward-running) interval.
    DeprecatedWindowInverted {
        valid_from: OffsetDateTime,
        valid_until: OffsetDateTime,
    },
    /// `status: revoked` carries a `revoked_at` that is strictly before
    /// `valid_from`. A key cannot be revoked before it began being valid;
    /// equality is allowed (revoked at the same instant it became valid).
    RevocationBeforeStart {
        valid_from: OffsetDateTime,
        revoked_at: OffsetDateTime,
    },
}

impl fmt::Display for KeyStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ActiveWithValidUntil => f.write_str(
                "key record status is active but carries a valid_until; active keys have no validity ceiling",
            ),
            Self::ActiveWithRevokedAt => f.write_str(
                "key record status is active but carries a revoked_at; active keys are not revoked",
            ),
            Self::DeprecatedFlatFieldMismatch => f.write_str(
                "key record status is deprecated but lifecycle fields are missing valid_until or include revoked_at",
            ),
            Self::RevokedFlatFieldMismatch => f.write_str(
                "key record status is revoked but lifecycle fields are missing revoked_at or include valid_until",
            ),
            Self::DeprecatedWindowInverted {
                valid_from,
                valid_until,
            } => write!(
                f,
                "key record deprecation window is inverted: valid_until ({valid_until}) must be strictly after valid_from ({valid_from})",
            ),
            Self::RevocationBeforeStart {
                valid_from,
                revoked_at,
            } => write!(
                f,
                "key record revocation precedes its start: revoked_at ({revoked_at}) is before valid_from ({valid_from})",
            ),
        }
    }
}

impl std::error::Error for KeyStateError {}

/// On-disk discriminator for [`KeyState`] (per
/// `specs/reeve-transport-security.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum KeyStatusFlat {
    Active,
    Deprecated,
    Revoked,
}

/// A single keypair entry associated with an [`Identity`](crate::Identity).
///
/// Mirrors domain-model § Security Layer § Key Record. The on-disk registry
/// stores `KeyRecord` as TOML; private key material is held in the OS
/// keychain and never appears in this struct.
///
/// The lifecycle fields `status` / `valid_until` / `revoked_at` from the
/// on-disk shape are reconciled into a [`KeyState`] sum type at parse time
/// so impossible flat combinations are rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyRecord {
    pub key_id: KeyId,
    pub identity_id: IdentityId,
    pub public_key: PublicKey,
    pub valid_from: OffsetDateTime,
    pub state: KeyState,
}

impl KeyRecord {
    /// Construct a fresh active key record valid from now.
    pub fn new(identity_id: IdentityId, public_key: PublicKey) -> Result<Self, KeyIdError> {
        Ok(Self {
            key_id: KeyId::new()?,
            identity_id,
            public_key,
            valid_from: OffsetDateTime::now_utc(),
            state: KeyState::Active,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct KeyRecordFlat {
    key_id: KeyId,
    identity_id: IdentityId,
    public_key: PublicKey,
    status: KeyStatusFlat,
    #[serde(with = "time::serde::rfc3339")]
    valid_from: OffsetDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    valid_until: Option<OffsetDateTime>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    revoked_at: Option<OffsetDateTime>,
}

impl Serialize for KeyRecord {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (status, valid_until, revoked_at) = match self.state {
            KeyState::Active => (KeyStatusFlat::Active, None, None),
            KeyState::Deprecated { valid_until } => {
                (KeyStatusFlat::Deprecated, Some(valid_until), None)
            }
            KeyState::Revoked { revoked_at } => (KeyStatusFlat::Revoked, None, Some(revoked_at)),
        };
        let flat = KeyRecordFlat {
            key_id: self.key_id,
            identity_id: self.identity_id,
            public_key: self.public_key,
            status,
            valid_from: self.valid_from,
            valid_until,
            revoked_at,
        };
        flat.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for KeyRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let flat = KeyRecordFlat::deserialize(deserializer)?;
        let state = match (flat.status, flat.valid_until, flat.revoked_at) {
            (KeyStatusFlat::Active, None, None) => KeyState::Active,
            (KeyStatusFlat::Active, Some(_), _) => {
                return Err(de::Error::custom(KeyStateError::ActiveWithValidUntil));
            }
            (KeyStatusFlat::Active, _, Some(_)) => {
                return Err(de::Error::custom(KeyStateError::ActiveWithRevokedAt));
            }
            (KeyStatusFlat::Deprecated, Some(valid_until), None) => {
                if valid_until <= flat.valid_from {
                    return Err(de::Error::custom(KeyStateError::DeprecatedWindowInverted {
                        valid_from: flat.valid_from,
                        valid_until,
                    }));
                }
                KeyState::Deprecated { valid_until }
            }
            (KeyStatusFlat::Deprecated, _, _) => {
                return Err(de::Error::custom(
                    KeyStateError::DeprecatedFlatFieldMismatch,
                ));
            }
            (KeyStatusFlat::Revoked, None, Some(revoked_at)) => {
                if revoked_at < flat.valid_from {
                    return Err(de::Error::custom(KeyStateError::RevocationBeforeStart {
                        valid_from: flat.valid_from,
                        revoked_at,
                    }));
                }
                KeyState::Revoked { revoked_at }
            }
            (KeyStatusFlat::Revoked, _, _) => {
                return Err(de::Error::custom(KeyStateError::RevokedFlatFieldMismatch));
            }
        };
        Ok(Self {
            key_id: flat.key_id,
            identity_id: flat.identity_id,
            public_key: flat.public_key,
            valid_from: flat.valid_from,
            state,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;
    use time::Duration;
    use uuid::Uuid;

    fn sample_public_key() -> PublicKey {
        let signing_key = SigningKey::generate(&mut OsRng);
        PublicKey::from_verifying_key(signing_key.verifying_key())
    }

    fn sample_key_bytes() -> [u8; PUBLIC_KEY_LENGTH] {
        let signing_key = SigningKey::generate(&mut OsRng);
        signing_key.verifying_key().to_bytes()
    }

    #[test]
    fn key_id_is_uuid_v7() {
        let id = KeyId::new().unwrap();
        assert_eq!(id.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn key_id_serde_round_trip() {
        let id = KeyId::new().unwrap();
        let json = serde_json::to_string(&id).unwrap();
        let decoded: KeyId = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, id);
    }

    #[test]
    fn key_id_try_from_rejects_v4_uuid() {
        let v4 = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let result = KeyId::try_from(v4);
        assert_eq!(result, Err(KeyIdError::NotV7 { actual_version: 4 }));
    }

    #[test]
    fn key_id_deserialize_rejects_v4_uuid() {
        let json = "\"550e8400-e29b-41d4-a716-446655440000\"";
        let result: Result<KeyId, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "expected v4 UUID to be rejected on deserialize"
        );
    }

    #[test]
    fn public_key_from_bytes_validates_curve_point() {
        let bytes = sample_key_bytes();
        let key = PublicKey::from_bytes(&bytes).unwrap();
        assert_eq!(key.to_bytes(), bytes);
    }

    #[test]
    fn public_key_from_bytes_rejects_non_curve_input() {
        // y = 2 with high bit clear: a value the Edwards y-coordinate may
        // take but for which no x exists on the curve, so dalek's decompress
        // fails. Discovered empirically against ed25519-dalek 2.x.
        let mut bytes = [0_u8; PUBLIC_KEY_LENGTH];
        bytes[0] = 2;
        let result = PublicKey::from_bytes(&bytes);
        assert!(
            matches!(result, Err(PublicKeyDecodeError::NotOnCurve(_))),
            "expected NotOnCurve, got {result:?}"
        );
    }

    #[test]
    fn public_key_base64_round_trip() {
        let key = sample_public_key();
        let encoded = key.to_base64();
        assert!(!encoded.contains('='), "expected no-padding encoding");
        let decoded = PublicKey::from_base64(&encoded).unwrap();
        assert_eq!(decoded.to_bytes(), key.to_bytes());
    }

    #[test]
    fn public_key_serde_round_trip_matches_bytes() {
        let key = sample_public_key();
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, format!("\"{}\"", key.to_base64()));
        let decoded: PublicKey = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.to_bytes(), key.to_bytes());
    }

    #[test]
    fn public_key_from_base64_rejects_short_input() {
        let result = PublicKey::from_base64("AA");
        assert_eq!(
            result,
            Err(PublicKeyDecodeError::InvalidLength { actual: 1 })
        );
    }

    #[test]
    fn public_key_from_base64_rejects_padded_input() {
        let result = PublicKey::from_base64("AA==");
        assert_eq!(result, Err(PublicKeyDecodeError::InvalidBase64));
    }

    #[test]
    fn public_key_from_base64_rejects_oversize_input() {
        let oversize = STANDARD_NO_PAD.encode([0_u8; PUBLIC_KEY_LENGTH + 4]);
        let result = PublicKey::from_base64(&oversize);
        assert_eq!(
            result,
            Err(PublicKeyDecodeError::InvalidLength {
                actual: PUBLIC_KEY_LENGTH + 4
            })
        );
    }

    #[test]
    fn public_key_from_base64_rejects_empty_input() {
        let result = PublicKey::from_base64("");
        assert_eq!(
            result,
            Err(PublicKeyDecodeError::InvalidLength { actual: 0 })
        );
    }

    #[test]
    fn public_key_from_base64_rejects_garbage() {
        let result = PublicKey::from_base64("!!!!");
        assert_eq!(result, Err(PublicKeyDecodeError::InvalidBase64));
    }

    #[test]
    fn fingerprint_is_stable_for_the_same_key() {
        let key = sample_public_key();
        assert_eq!(key.fingerprint(), key.fingerprint());
    }

    #[test]
    fn fingerprint_format_is_colon_hex_pairs() {
        let key = sample_public_key();
        let bytes = key.to_bytes();
        let expected = format!(
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]
        );
        assert_eq!(key.fingerprint(), expected);
    }

    #[test]
    fn fingerprint_differs_between_distinct_keys() {
        let a = sample_public_key();
        let b = sample_public_key();
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn debug_wraps_base64_in_publickey_marker() {
        let key = sample_public_key();
        let rendered = format!("{key:?}");
        assert!(rendered.starts_with("PublicKey("));
        assert!(rendered.contains(&key.to_base64()));
    }

    #[test]
    fn display_renders_canonical_base64() {
        let key = sample_public_key();
        assert_eq!(format!("{key}"), key.to_base64());
    }

    #[test]
    fn key_record_new_defaults_to_active_now() {
        let identity = IdentityId::new().unwrap();
        let key = sample_public_key();
        let before = OffsetDateTime::now_utc();
        let record = KeyRecord::new(identity, key).unwrap();
        let after = OffsetDateTime::now_utc();

        assert_eq!(record.identity_id, identity);
        assert_eq!(record.public_key.to_bytes(), key.to_bytes());
        assert_eq!(record.state, KeyState::Active);
        assert!(record.valid_from >= before - Duration::seconds(1));
        assert!(record.valid_from <= after + Duration::seconds(1));
    }

    #[test]
    fn key_record_new_mints_distinct_key_ids() {
        let identity = IdentityId::new().unwrap();
        let key = sample_public_key();
        let a = KeyRecord::new(identity, key).unwrap();
        let b = KeyRecord::new(identity, key).unwrap();
        assert_ne!(a.key_id, b.key_id);
    }

    #[test]
    fn key_record_serde_round_trip_active() {
        let identity = IdentityId::new().unwrap();
        let record = KeyRecord::new(identity, sample_public_key()).unwrap();
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"status\":\"active\""));
        assert!(!json.contains("valid_until"));
        assert!(!json.contains("revoked_at"));
        let decoded: KeyRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn key_record_serde_round_trip_deprecated_preserves_some_branch() {
        let identity = IdentityId::new().unwrap();
        let mut record = KeyRecord::new(identity, sample_public_key()).unwrap();
        let until = OffsetDateTime::now_utc() + Duration::days(30);
        record.state = KeyState::Deprecated { valid_until: until };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"status\":\"deprecated\""));
        assert!(json.contains("valid_until"));
        let decoded: KeyRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn key_record_serde_round_trip_revoked_preserves_some_branch() {
        let identity = IdentityId::new().unwrap();
        let mut record = KeyRecord::new(identity, sample_public_key()).unwrap();
        let revoked = OffsetDateTime::now_utc();
        record.state = KeyState::Revoked {
            revoked_at: revoked,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"status\":\"revoked\""));
        assert!(json.contains("revoked_at"));
        let decoded: KeyRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn key_record_deserialize_rejects_active_with_valid_until() {
        let identity = IdentityId::new().unwrap();
        let key = sample_public_key();
        let json = serde_json::json!({
            "key_id": KeyId::new().unwrap(),
            "identity_id": identity,
            "public_key": key,
            "status": "active",
            "valid_from": "2026-01-01T00:00:00Z",
            "valid_until": "2026-06-01T00:00:00Z",
        });
        let result: Result<KeyRecord, _> = serde_json::from_value(json);
        let err = result.expect_err("active+valid_until must be rejected");
        assert!(
            err.to_string()
                .contains(&KeyStateError::ActiveWithValidUntil.to_string()),
            "expected ActiveWithValidUntil, got {err}"
        );
    }

    #[test]
    fn key_record_deserialize_rejects_active_with_revoked_at() {
        let identity = IdentityId::new().unwrap();
        let key = sample_public_key();
        let json = serde_json::json!({
            "key_id": KeyId::new().unwrap(),
            "identity_id": identity,
            "public_key": key,
            "status": "active",
            "valid_from": "2026-01-01T00:00:00Z",
            "revoked_at": "2026-06-01T00:00:00Z",
        });
        let result: Result<KeyRecord, _> = serde_json::from_value(json);
        let err = result.expect_err("active+revoked_at must be rejected");
        assert!(
            err.to_string()
                .contains(&KeyStateError::ActiveWithRevokedAt.to_string()),
            "expected ActiveWithRevokedAt, got {err}"
        );
    }

    #[test]
    fn key_record_deserialize_rejects_deprecated_without_valid_until() {
        let identity = IdentityId::new().unwrap();
        let key = sample_public_key();
        let json = serde_json::json!({
            "key_id": KeyId::new().unwrap(),
            "identity_id": identity,
            "public_key": key,
            "status": "deprecated",
            "valid_from": "2026-01-01T00:00:00Z",
        });
        let result: Result<KeyRecord, _> = serde_json::from_value(json);
        let err = result.expect_err("deprecated without valid_until must be rejected");
        assert!(
            err.to_string()
                .contains(&KeyStateError::DeprecatedFlatFieldMismatch.to_string()),
            "expected DeprecatedFlatFieldMismatch, got {err}"
        );
    }

    #[test]
    fn key_record_deserialize_rejects_revoked_without_revoked_at() {
        let identity = IdentityId::new().unwrap();
        let key = sample_public_key();
        let json = serde_json::json!({
            "key_id": KeyId::new().unwrap(),
            "identity_id": identity,
            "public_key": key,
            "status": "revoked",
            "valid_from": "2026-01-01T00:00:00Z",
        });
        let result: Result<KeyRecord, _> = serde_json::from_value(json);
        let err = result.expect_err("revoked without revoked_at must be rejected");
        assert!(
            err.to_string()
                .contains(&KeyStateError::RevokedFlatFieldMismatch.to_string()),
            "expected RevokedFlatFieldMismatch, got {err}"
        );
    }

    #[test]
    fn key_record_deserialize_rejects_deprecated_window_equal_endpoints() {
        let identity = IdentityId::new().unwrap();
        let key = sample_public_key();
        let json = serde_json::json!({
            "key_id": KeyId::new().unwrap(),
            "identity_id": identity,
            "public_key": key,
            "status": "deprecated",
            "valid_from": "2026-01-01T00:00:00Z",
            "valid_until": "2026-01-01T00:00:00Z",
        });
        let result: Result<KeyRecord, _> = serde_json::from_value(json);
        let err = result.expect_err("deprecated with valid_until == valid_from must be rejected");
        assert!(
            err.to_string().contains("deprecation window is inverted"),
            "expected DeprecatedWindowInverted, got {err}"
        );
    }

    #[test]
    fn key_record_deserialize_rejects_deprecated_window_inverted() {
        let identity = IdentityId::new().unwrap();
        let key = sample_public_key();
        let json = serde_json::json!({
            "key_id": KeyId::new().unwrap(),
            "identity_id": identity,
            "public_key": key,
            "status": "deprecated",
            "valid_from": "2026-06-01T00:00:00Z",
            "valid_until": "2026-01-01T00:00:00Z",
        });
        let result: Result<KeyRecord, _> = serde_json::from_value(json);
        let err = result.expect_err("deprecated with valid_until < valid_from must be rejected");
        let expected = KeyStateError::DeprecatedWindowInverted {
            valid_from: time::macros::datetime!(2026-06-01 00:00:00 UTC),
            valid_until: time::macros::datetime!(2026-01-01 00:00:00 UTC),
        };
        assert!(
            err.to_string().contains(&expected.to_string()),
            "expected DeprecatedWindowInverted, got {err}"
        );
    }

    #[test]
    fn key_record_deserialize_rejects_revocation_before_start() {
        let identity = IdentityId::new().unwrap();
        let key = sample_public_key();
        let json = serde_json::json!({
            "key_id": KeyId::new().unwrap(),
            "identity_id": identity,
            "public_key": key,
            "status": "revoked",
            "valid_from": "2026-06-01T00:00:00Z",
            "revoked_at": "2026-01-01T00:00:00Z",
        });
        let result: Result<KeyRecord, _> = serde_json::from_value(json);
        let err = result.expect_err("revoked with revoked_at < valid_from must be rejected");
        let expected = KeyStateError::RevocationBeforeStart {
            valid_from: time::macros::datetime!(2026-06-01 00:00:00 UTC),
            revoked_at: time::macros::datetime!(2026-01-01 00:00:00 UTC),
        };
        assert!(
            err.to_string().contains(&expected.to_string()),
            "expected RevocationBeforeStart, got {err}"
        );
    }

    #[test]
    fn key_record_deserialize_rejects_deprecated_with_revoked_at() {
        let json = serde_json::json!({
            "key_id": KeyId::new().unwrap(),
            "identity_id": IdentityId::new().unwrap(),
            "public_key": sample_public_key(),
            "status": "deprecated",
            "valid_from": "2026-01-01T00:00:00Z",
            "valid_until": "2026-06-01T00:00:00Z",
            "revoked_at": "2026-03-01T00:00:00Z",
        });
        let result: Result<KeyRecord, _> = serde_json::from_value(json);
        let err = result.expect_err("deprecated with revoked_at must be rejected");
        assert!(
            err.to_string()
                .contains(&KeyStateError::DeprecatedFlatFieldMismatch.to_string()),
            "expected DeprecatedFlatFieldMismatch, got {err}"
        );
    }

    #[test]
    fn key_record_deserialize_rejects_deprecated_with_only_revoked_at() {
        let json = serde_json::json!({
            "key_id": KeyId::new().unwrap(),
            "identity_id": IdentityId::new().unwrap(),
            "public_key": sample_public_key(),
            "status": "deprecated",
            "valid_from": "2026-01-01T00:00:00Z",
            "revoked_at": "2026-03-01T00:00:00Z",
        });
        let result: Result<KeyRecord, _> = serde_json::from_value(json);
        let err = result.expect_err("deprecated with only revoked_at must be rejected");
        assert!(
            err.to_string()
                .contains(&KeyStateError::DeprecatedFlatFieldMismatch.to_string()),
            "expected DeprecatedFlatFieldMismatch, got {err}"
        );
    }

    #[test]
    fn key_record_deserialize_rejects_revoked_with_only_valid_until() {
        let json = serde_json::json!({
            "key_id": KeyId::new().unwrap(),
            "identity_id": IdentityId::new().unwrap(),
            "public_key": sample_public_key(),
            "status": "revoked",
            "valid_from": "2026-01-01T00:00:00Z",
            "valid_until": "2026-06-01T00:00:00Z",
        });
        let result: Result<KeyRecord, _> = serde_json::from_value(json);
        let err = result.expect_err("revoked with only valid_until must be rejected");
        assert!(
            err.to_string()
                .contains(&KeyStateError::RevokedFlatFieldMismatch.to_string()),
            "expected RevokedFlatFieldMismatch, got {err}"
        );
    }

    #[test]
    fn key_record_deserialize_rejects_revoked_with_valid_until_and_revoked_at() {
        let json = serde_json::json!({
            "key_id": KeyId::new().unwrap(),
            "identity_id": IdentityId::new().unwrap(),
            "public_key": sample_public_key(),
            "status": "revoked",
            "valid_from": "2026-01-01T00:00:00Z",
            "valid_until": "2026-06-01T00:00:00Z",
            "revoked_at": "2026-03-01T00:00:00Z",
        });
        let result: Result<KeyRecord, _> = serde_json::from_value(json);
        let err = result.expect_err("revoked with valid_until and revoked_at must be rejected");
        assert!(
            err.to_string()
                .contains(&KeyStateError::RevokedFlatFieldMismatch.to_string()),
            "expected RevokedFlatFieldMismatch, got {err}"
        );
    }

    #[test]
    fn key_id_error_display_uses_key_id_noun() {
        let err = KeyIdError::NotV7 { actual_version: 4 };
        assert!(
            err.to_string().contains("key id"),
            "expected 'key id' in error: {err}"
        );
    }

    #[test]
    fn key_record_deserialize_accepts_revocation_at_same_instant_as_start() {
        let identity = IdentityId::new().unwrap();
        let key = sample_public_key();
        let json = serde_json::json!({
            "key_id": KeyId::new().unwrap(),
            "identity_id": identity,
            "public_key": key,
            "status": "revoked",
            "valid_from": "2026-06-01T00:00:00Z",
            "revoked_at": "2026-06-01T00:00:00Z",
        });
        let decoded: KeyRecord = serde_json::from_value(json).unwrap();
        assert_eq!(
            decoded.state,
            KeyState::Revoked {
                revoked_at: time::macros::datetime!(2026-06-01 00:00:00 UTC),
            }
        );
    }
}
