//! Business logic for `reeve envelope sign` and `reeve envelope verify`.
//!
//! Both subcommands are debug utilities: `sign` exercises the full sign path
//! from keychain to signed envelope JSON; `verify` reads that JSON back and
//! checks the signature against the sender's public key in the registry.
//! Neither subcommand is on the hot path — correctness over cleverness.

use std::io::{Read, Write};
use std::path::Path;

use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use reeve_runtime::{IdentityRegistry, OperatorKeyStore, StoredIdentity};
use reeve_transport::{sign_envelope, verify_envelope};
use reeve_types::{
    Envelope, EnvelopeSignature, IdentityId, KeyId, KeyState, MessageId, Nonce, PayloadHash,
    PrivateKey, SchemaVersion, NONCE_LEN, PAYLOAD_HASH_LEN, SIGNATURE_LEN,
};

use crate::identity::find_existing_operator;

/// Placeholder fill byte for envelope signatures during the sign sequence.
/// `sign_envelope` excludes the signature field from canonical bytes, so the
/// value here is irrelevant to correctness — but a non-zero sentinel makes
/// it visually distinct from a real signature when debugging.
const PLACEHOLDER_SIG_BYTE: u8 = 0xDE;

/// Errors that can occur during `reeve envelope sign` or `reeve envelope verify`.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum EnvelopeCliError {
    /// No operator identity is enrolled on this machine.
    NoOperatorEnrolled,
    /// The operator identity exists but has no key records. Re-enroll or
    /// restore the keychain entry.
    OperatorHasNoActiveKey { identity_id: IdentityId },
    /// The requested recipient identity was not found in the registry.
    RecipientNotFound { recipient_id: IdentityId },
    /// The sender identity in the envelope is not present in the registry.
    SenderNotFound { sender_id: IdentityId },
    /// The `sender_key_id` field in the envelope does not match any key record
    /// on the sender's identity in the registry.
    SenderKeyNotFound {
        sender_id: IdentityId,
        sender_key_id: KeyId,
    },
    /// The signing key has been revoked and verifies nothing.
    KeyRevoked {
        key_id: KeyId,
        revoked_at: OffsetDateTime,
    },
    /// The envelope was created after the key's deprecation validity window
    /// closed.
    KeyNoLongerValid {
        key_id: KeyId,
        valid_until: OffsetDateTime,
        envelope_created_at: OffsetDateTime,
    },
    /// The registry could not be listed or read.
    Registry(reeve_runtime::RegistryError),
    /// The OS keychain could not supply the operator's signing seed. If the
    /// registry has the entry but the keychain does not, re-enroll or restore
    /// the keychain entry for the identity.
    Keychain(reeve_runtime::KeychainError),
    /// Signing the envelope failed.
    Sign(reeve_transport::SignError),
    /// Verifying the envelope failed.
    Verify(reeve_transport::VerifyError),
    /// Serializing the envelope to JSON failed.
    Serialize(serde_json::Error),
    /// Deserializing the envelope from JSON failed.
    Deserialize(serde_json::Error),
    /// Reading or writing the envelope file failed.
    Io(std::io::Error),
    /// The message id could not be minted. This can occur due to clock skew
    /// or a monotonicity violation in the system clock.
    MintMessageId(reeve_types::MessageIdError),
}

impl std::fmt::Display for EnvelopeCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoOperatorEnrolled => {
                f.write_str("no operator identity enrolled; run `reeve identity enroll` first")
            }
            Self::OperatorHasNoActiveKey { identity_id } => write!(
                f,
                "operator {identity_id} exists in the registry but has no key records \
                 — re-enroll or restore the keychain entry",
            ),
            Self::RecipientNotFound { recipient_id } => {
                write!(f, "recipient identity {recipient_id} not found in registry")
            }
            Self::SenderNotFound { sender_id } => {
                write!(f, "sender identity {sender_id} not found in registry")
            }
            Self::SenderKeyNotFound {
                sender_id,
                sender_key_id,
            } => write!(
                f,
                "sender_key_id {sender_key_id} not found on identity {sender_id} in registry",
            ),
            Self::KeyRevoked { key_id, revoked_at } => write!(
                f,
                "envelope signed with revoked key {key_id} (revoked at {revoked_at})",
            ),
            Self::KeyNoLongerValid {
                key_id,
                valid_until,
                envelope_created_at,
            } => write!(
                f,
                "envelope signed with key {key_id} created after the key's deprecation \
                 cutoff (valid_until={valid_until}, envelope_created_at={envelope_created_at})",
            ),
            Self::Registry(err) => write!(f, "identity registry error: {err}"),
            Self::Keychain(err) => write!(f, "keychain error: {err}"),
            Self::Sign(err) => write!(f, "envelope sign error: {err}"),
            Self::Verify(err) => write!(f, "envelope verify error: {err}"),
            Self::Serialize(err) => write!(f, "envelope serialize error: {err}"),
            Self::Deserialize(err) => write!(f, "envelope deserialize error: {err}"),
            Self::Io(err) => write!(f, "IO error: {err}"),
            Self::MintMessageId(err) => write!(f, "failed to mint message id: {err}"),
        }
    }
}

impl std::error::Error for EnvelopeCliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registry(err) => Some(err),
            Self::Keychain(err) => Some(err),
            Self::Sign(err) => Some(err),
            Self::Verify(err) => Some(err),
            Self::Serialize(err) | Self::Deserialize(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::MintMessageId(err) => Some(err),
            Self::NoOperatorEnrolled
            | Self::OperatorHasNoActiveKey { .. }
            | Self::RecipientNotFound { .. }
            | Self::SenderNotFound { .. }
            | Self::SenderKeyNotFound { .. }
            | Self::KeyRevoked { .. }
            | Self::KeyNoLongerValid { .. } => None,
        }
    }
}

impl From<reeve_runtime::RegistryError> for EnvelopeCliError {
    fn from(err: reeve_runtime::RegistryError) -> Self {
        Self::Registry(err)
    }
}

impl From<reeve_runtime::KeychainError> for EnvelopeCliError {
    fn from(err: reeve_runtime::KeychainError) -> Self {
        Self::Keychain(err)
    }
}

impl From<reeve_transport::SignError> for EnvelopeCliError {
    fn from(err: reeve_transport::SignError) -> Self {
        Self::Sign(err)
    }
}

impl From<reeve_transport::VerifyError> for EnvelopeCliError {
    fn from(err: reeve_transport::VerifyError) -> Self {
        Self::Verify(err)
    }
}

impl From<std::io::Error> for EnvelopeCliError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Sign a fresh envelope addressed to `recipient_id` with `body` as the
/// payload, and write the signed JSON to `out`.
///
/// The `--body` value is visible in process listings on Unix; agents operating
/// in production should prefer passing payloads via stdin or a file rather
/// than the `--body` flag.
///
/// Sequence:
///   1. List the registry and locate the operator identity.
///   2. Locate the recipient identity by `recipient_id`.
///   3. Retrieve the operator's signing seed from `keychain`.
///   4. Build the envelope with a fresh `MessageId`, nonce, and SHA-256
///      payload hash.
///   5. Sign and embed the signature.
///   6. Serialize to pretty JSON and write to `out`.
pub(crate) fn sign(
    registry: &IdentityRegistry,
    keychain: &dyn OperatorKeyStore,
    recipient_id: IdentityId,
    body: &[u8],
    out: &mut impl Write,
) -> Result<(), EnvelopeCliError> {
    let stored = registry.list()?;

    let operator = find_existing_operator(&stored).ok_or(EnvelopeCliError::NoOperatorEnrolled)?;

    let recipient = find_by_identity_id(&stored, recipient_id)
        .ok_or(EnvelopeCliError::RecipientNotFound { recipient_id })?;

    let operator_key =
        operator
            .key_records()
            .first()
            .ok_or_else(|| EnvelopeCliError::OperatorHasNoActiveKey {
                identity_id: operator.identity().identity_id,
            })?;

    // Deprecated and revoked keys cannot authenticate new messages per
    // specs/reeve-transport-security.md § Identity and Key Model.
    match operator_key.state {
        KeyState::Active => {}
        KeyState::Revoked { revoked_at } => {
            return Err(EnvelopeCliError::KeyRevoked {
                key_id: operator_key.key_id,
                revoked_at,
            });
        }
        KeyState::Deprecated { valid_until } => {
            return Err(EnvelopeCliError::KeyNoLongerValid {
                key_id: operator_key.key_id,
                valid_until,
                envelope_created_at: OffsetDateTime::now_utc(),
            });
        }
    }

    let seed = keychain.retrieve(operator.identity().identity_id)?;
    let private_key = PrivateKey::from_seed_bytes(&seed);

    let message_id = MessageId::new().map_err(EnvelopeCliError::MintMessageId)?;
    let nonce = fresh_nonce();
    let payload_hash = sha256_hash(body);

    let mut envelope = Envelope::new(
        SchemaVersion::V1,
        message_id,
        operator.identity().identity_id,
        operator_key.key_id,
        recipient.identity().identity_id,
        OffsetDateTime::now_utc(),
        nonce,
        payload_hash,
        body.to_vec(),
        EnvelopeSignature::from_bytes([PLACEHOLDER_SIG_BYTE; SIGNATURE_LEN]),
    );

    let sig = sign_envelope(&envelope, &private_key)?;
    envelope.signature = sig;

    let json = serde_json::to_string_pretty(&envelope).map_err(EnvelopeCliError::Serialize)?;
    writeln!(out, "{json}")?;
    Ok(())
}

/// Verify the signature on an envelope read from `reader`.
///
/// Sequence:
///   1. Read and parse the JSON envelope from `reader`.
///   2. Look up the sender identity by `envelope.sender_id`.
///   3. Find the key record matching `envelope.sender_key_id`.
///   4. Check the key's lifecycle state: revoked keys are always rejected;
///      deprecated keys are rejected when `envelope.created_at` falls after
///      their `valid_until` cutoff.
///   5. Verify the signature.
///   6. On success, write a human-readable confirmation to `out`.
///
/// Note: replay protection (nonce uniqueness, freshness) is not enforced at
/// this layer — that is a delivery-layer concern (Phase 4+). The confirmation
/// line includes a caveat to that effect.
///
/// Note: `payload_hash` is verified to match `sha256(body)` by the signature
/// check, because `payload_hash` is included in the canonical bytes. A
/// separate explicit hash comparison (sha256(body) == `payload_hash`) would be
/// a defense-in-depth check not yet implemented here.
pub(crate) fn verify(
    registry: &IdentityRegistry,
    reader: &mut impl Read,
    out: &mut impl Write,
) -> Result<(), EnvelopeCliError> {
    let mut contents = String::new();
    reader.read_to_string(&mut contents)?;

    let envelope: Envelope =
        serde_json::from_str(&contents).map_err(EnvelopeCliError::Deserialize)?;

    let stored = registry.list()?;

    let sender = find_by_identity_id(&stored, envelope.sender_id).ok_or(
        EnvelopeCliError::SenderNotFound {
            sender_id: envelope.sender_id,
        },
    )?;

    let matching_count = sender
        .key_records()
        .iter()
        .filter(|kr| kr.key_id == envelope.sender_key_id)
        .count();
    debug_assert!(
        matching_count <= 1,
        "invariant violation: {} key records match key_id {}",
        matching_count,
        envelope.sender_key_id,
    );

    let key_record = sender
        .key_records()
        .iter()
        .find(|kr| kr.key_id == envelope.sender_key_id)
        .ok_or(EnvelopeCliError::SenderKeyNotFound {
            sender_id: envelope.sender_id,
            sender_key_id: envelope.sender_key_id,
        })?;

    match key_record.state {
        KeyState::Active => {}
        KeyState::Deprecated { valid_until } => {
            if envelope.created_at > valid_until {
                return Err(EnvelopeCliError::KeyNoLongerValid {
                    key_id: envelope.sender_key_id,
                    valid_until,
                    envelope_created_at: envelope.created_at,
                });
            }
        }
        KeyState::Revoked { revoked_at } => {
            return Err(EnvelopeCliError::KeyRevoked {
                key_id: envelope.sender_key_id,
                revoked_at,
            });
        }
    }

    verify_envelope(&envelope, &key_record.public_key)?;

    writeln!(
        out,
        "verified (replay protection not enforced at this layer): \
         sender={} message_id={} body_bytes={}",
        sender.identity().display_name,
        envelope.message_id,
        envelope.body.len(),
    )?;
    Ok(())
}

/// Verify an envelope stored at `path`. Opens the file and delegates to
/// [`verify`], writing the confirmation to `out`.
pub(crate) fn verify_from_path(
    registry: &IdentityRegistry,
    path: &Path,
    out: &mut impl Write,
) -> Result<(), EnvelopeCliError> {
    let mut file = std::fs::File::open(path).map_err(EnvelopeCliError::Io)?;
    verify(registry, &mut file, out)
}

/// Generate a fresh 16-byte cryptographic nonce using the OS RNG.
fn fresh_nonce() -> Nonce {
    let mut bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut bytes);
    Nonce::from_bytes(bytes)
}

/// Compute the SHA-256 hash of `data` and return it as a [`PayloadHash`].
fn sha256_hash(data: &[u8]) -> PayloadHash {
    let digest = Sha256::digest(data);
    let mut bytes = [0u8; PAYLOAD_HASH_LEN];
    bytes.copy_from_slice(&digest);
    PayloadHash::from_bytes(bytes)
}

/// Find the first stored identity whose `identity_id` matches `id`.
fn find_by_identity_id(stored: &[StoredIdentity], id: IdentityId) -> Option<&StoredIdentity> {
    stored.iter().find(|s| s.identity().identity_id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    use reeve_runtime::keychain::memory::MemoryKeyStore;
    use reeve_types::{Identity, KeyRecord, Keypair};
    use tempfile::tempdir;
    use time::Duration;

    /// Bit to flip when constructing a tampered signature in tests.
    const FLIP_BIT: u8 = 0x01;

    #[cfg(unix)]
    fn secure_dir(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0o700 must succeed in tests");
    }

    #[cfg(not(unix))]
    fn secure_dir(_path: &Path) {}

    fn open_registry(dir: &Path) -> IdentityRegistry {
        secure_dir(dir);
        IdentityRegistry::open(dir.to_path_buf()).unwrap()
    }

    fn enroll_operator(
        registry: &IdentityRegistry,
        keychain: &MemoryKeyStore,
        name: &str,
    ) -> StoredIdentity {
        crate::identity::enroll(registry, keychain, name).unwrap()
    }

    fn register_recipient(registry: &IdentityRegistry, name: &str) -> StoredIdentity {
        let keypair = Keypair::generate();
        let (_, public) = keypair.into_parts();
        let identity = Identity::new_agent(name.to_owned(), IdentityId::new().unwrap()).unwrap();
        let key_record = KeyRecord::new(identity.identity_id, public).unwrap();
        let stored = StoredIdentity::new(identity, key_record).unwrap();
        registry.write(&stored).unwrap();
        stored
    }

    #[test]
    fn sign_happy_path_produces_valid_envelope_json() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        let operator = enroll_operator(&registry, &keychain, "Ada");
        let recipient = register_recipient(&registry, "worker-1");

        let mut out = Vec::new();
        sign(
            &registry,
            &keychain,
            recipient.identity().identity_id,
            b"hello",
            &mut out,
        )
        .unwrap();

        let json = String::from_utf8(out).unwrap();
        let envelope: Envelope = serde_json::from_str(&json).unwrap();

        assert_eq!(envelope.sender_id, operator.identity().identity_id);
        assert_eq!(envelope.recipient_id, recipient.identity().identity_id);
        assert_eq!(envelope.schema_version, SchemaVersion::V1);
        assert_eq!(envelope.body, b"hello");
    }

    #[test]
    fn sign_rejects_when_no_operator_is_enrolled() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        let recipient_id = IdentityId::new().unwrap();
        let err = sign(&registry, &keychain, recipient_id, b"body", &mut Vec::new()).unwrap_err();

        assert!(
            matches!(err, EnvelopeCliError::NoOperatorEnrolled),
            "expected NoOperatorEnrolled, got {err}"
        );
    }

    #[test]
    fn sign_rejects_when_recipient_not_in_registry() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        enroll_operator(&registry, &keychain, "Ada");

        let missing_id = IdentityId::new().unwrap();
        let err = sign(&registry, &keychain, missing_id, b"body", &mut Vec::new()).unwrap_err();

        assert!(
            matches!(err, EnvelopeCliError::RecipientNotFound { .. }),
            "expected RecipientNotFound, got {err}"
        );
    }

    #[test]
    fn verify_rejects_when_sender_not_in_registry() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        enroll_operator(&registry, &keychain, "Ada");
        let recipient = register_recipient(&registry, "worker-3");

        let mut signed_out = Vec::new();
        sign(
            &registry,
            &keychain,
            recipient.identity().identity_id,
            b"body",
            &mut signed_out,
        )
        .unwrap();

        let json = String::from_utf8(signed_out).unwrap();
        let mut envelope: serde_json::Value = serde_json::from_str(&json).unwrap();

        let foreign_id = IdentityId::new().unwrap();
        envelope["sender_id"] = serde_json::Value::String(foreign_id.to_string());

        let tampered = serde_json::to_string_pretty(&envelope).unwrap();

        let verify_dir = tempdir().unwrap();
        let verify_registry = open_registry(verify_dir.path());
        let err = verify(&verify_registry, &mut tampered.as_bytes(), &mut Vec::new()).unwrap_err();

        assert!(
            matches!(err, EnvelopeCliError::SenderNotFound { .. }),
            "expected SenderNotFound, got {err}"
        );
    }

    #[test]
    fn verify_rejects_when_sender_key_id_not_on_sender() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        enroll_operator(&registry, &keychain, "Ada");
        let recipient = register_recipient(&registry, "worker-4");

        let mut signed_out = Vec::new();
        sign(
            &registry,
            &keychain,
            recipient.identity().identity_id,
            b"body",
            &mut signed_out,
        )
        .unwrap();

        let json = String::from_utf8(signed_out).unwrap();
        let mut envelope: serde_json::Value = serde_json::from_str(&json).unwrap();

        let wrong_key_id = KeyId::new().unwrap();
        envelope["sender_key_id"] = serde_json::Value::String(wrong_key_id.to_string());

        let tampered = serde_json::to_string_pretty(&envelope).unwrap();
        let err = verify(&registry, &mut tampered.as_bytes(), &mut Vec::new()).unwrap_err();

        assert!(
            matches!(err, EnvelopeCliError::SenderKeyNotFound { .. }),
            "expected SenderKeyNotFound, got {err}"
        );
    }

    #[test]
    fn verify_rejects_tampered_signature() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        enroll_operator(&registry, &keychain, "Ada");
        let recipient = register_recipient(&registry, "worker-5");

        let mut signed_out = Vec::new();
        sign(
            &registry,
            &keychain,
            recipient.identity().identity_id,
            b"body",
            &mut signed_out,
        )
        .unwrap();

        let json = String::from_utf8(signed_out).unwrap();
        let envelope: Envelope = serde_json::from_str(&json).unwrap();

        let mut sig_bytes = *envelope.signature.as_bytes();
        sig_bytes[0] ^= FLIP_BIT;
        let bad_sig = EnvelopeSignature::from_bytes(sig_bytes);

        let tampered = Envelope::new(
            envelope.schema_version,
            envelope.message_id,
            envelope.sender_id,
            envelope.sender_key_id,
            envelope.recipient_id,
            envelope.created_at,
            envelope.nonce,
            envelope.payload_hash,
            envelope.body.clone(),
            bad_sig,
        );
        let tampered_json = serde_json::to_string_pretty(&tampered).unwrap();

        let err = verify(&registry, &mut tampered_json.as_bytes(), &mut Vec::new()).unwrap_err();

        assert!(
            matches!(err, EnvelopeCliError::Verify(_)),
            "expected Verify error, got {err}"
        );
    }

    #[test]
    fn verify_happy_path_after_sign_succeeds() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        enroll_operator(&registry, &keychain, "Ada");
        let recipient = register_recipient(&registry, "worker-2");

        let mut signed_out = Vec::new();
        sign(
            &registry,
            &keychain,
            recipient.identity().identity_id,
            b"round-trip",
            &mut signed_out,
        )
        .unwrap();

        let mut out = Vec::new();
        verify(&registry, &mut signed_out.as_slice(), &mut out).unwrap();

        let result = String::from_utf8(out).unwrap();
        assert!(
            result.contains("verified"),
            "expected verified output: {result}"
        );
        assert!(result.contains("Ada"), "expected sender name: {result}");
        assert!(
            result.contains("replay protection not enforced"),
            "expected replay caveat: {result}",
        );
    }

    #[test]
    fn error_display_no_operator_enrolled() {
        let err = EnvelopeCliError::NoOperatorEnrolled;
        assert!(err.to_string().contains("no operator"));
    }

    #[test]
    fn error_display_recipient_not_found() {
        let id = IdentityId::new().unwrap();
        let err = EnvelopeCliError::RecipientNotFound { recipient_id: id };
        let rendered = err.to_string();
        assert!(rendered.contains(&id.to_string()));
        assert!(rendered.contains("recipient"));
    }

    #[test]
    fn error_display_sender_not_found() {
        let id = IdentityId::new().unwrap();
        let err = EnvelopeCliError::SenderNotFound { sender_id: id };
        let rendered = err.to_string();
        assert!(rendered.contains(&id.to_string()));
        assert!(rendered.contains("sender"));
    }

    #[test]
    fn error_display_sender_key_not_found() {
        let sender_id = IdentityId::new().unwrap();
        let sender_key_id = KeyId::new().unwrap();
        let err = EnvelopeCliError::SenderKeyNotFound {
            sender_id,
            sender_key_id,
        };
        let rendered = err.to_string();
        assert!(rendered.contains(&sender_id.to_string()));
        assert!(rendered.contains(&sender_key_id.to_string()));
    }

    #[test]
    fn fresh_nonce_yields_distinct_consecutive_values() {
        let n1 = fresh_nonce();
        let n2 = fresh_nonce();
        assert_ne!(n1.as_bytes(), n2.as_bytes(), "two nonces should differ");
    }

    #[test]
    fn sign_produces_distinct_nonces_and_message_ids_per_call() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        enroll_operator(&registry, &keychain, "Newton");
        let recipient = register_recipient(&registry, "agent-n");

        let mut buf_a = Vec::new();
        sign(
            &registry,
            &keychain,
            recipient.identity().identity_id,
            b"hello",
            &mut buf_a,
        )
        .unwrap();

        let mut buf_b = Vec::new();
        sign(
            &registry,
            &keychain,
            recipient.identity().identity_id,
            b"hello",
            &mut buf_b,
        )
        .unwrap();

        let env_a: Envelope = serde_json::from_slice(&buf_a).unwrap();
        let env_b: Envelope = serde_json::from_slice(&buf_b).unwrap();
        assert_ne!(
            env_a.nonce.as_bytes(),
            env_b.nonce.as_bytes(),
            "successive sign calls must produce distinct nonces"
        );
        assert_ne!(
            env_a.message_id, env_b.message_id,
            "successive sign calls must produce distinct message_ids"
        );
    }

    #[test]
    fn sha256_hash_known_vector() {
        let hash = sha256_hash(b"");
        let expected: [u8; PAYLOAD_HASH_LEN] = [
            0xe3, 0xb0, 0xc4, 0x42, 0x98, 0xfc, 0x1c, 0x14, 0x9a, 0xfb, 0xf4, 0xc8, 0x99, 0x6f,
            0xb9, 0x24, 0x27, 0xae, 0x41, 0xe4, 0x64, 0x9b, 0x93, 0x4c, 0xa4, 0x95, 0x99, 0x1b,
            0x78, 0x52, 0xb8, 0x55,
        ];
        assert_eq!(hash.as_bytes(), &expected);
    }

    #[test]
    fn verify_file_path_helper_reads_from_path() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        enroll_operator(&registry, &keychain, "Dirac");
        let recipient = register_recipient(&registry, "agent-z");

        let mut signed_out = Vec::new();
        sign(
            &registry,
            &keychain,
            recipient.identity().identity_id,
            b"file body",
            &mut signed_out,
        )
        .unwrap();

        let envelope_path = dir.path().join("envelope.json");
        std::fs::write(&envelope_path, &signed_out).unwrap();

        let mut out = Vec::new();
        let verified = verify_from_path(&registry, &envelope_path, &mut out);
        assert!(
            verified.is_ok(),
            "verify_from_path must succeed: {verified:?}"
        );
    }

    #[test]
    fn verify_from_path_returns_io_error_for_missing_file() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let mut out = Vec::new();
        let result = verify_from_path(&registry, Path::new("/does/not/exist"), &mut out);
        assert!(
            matches!(result, Err(EnvelopeCliError::Io(_))),
            "expected Io error for missing file, got {result:?}",
        );
    }

    #[test]
    fn verify_returns_deserialize_error_for_malformed_json() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        enroll_operator(&registry, &keychain, "Euler");
        let mut out = Vec::new();
        let result = verify(
            &registry,
            &mut std::io::Cursor::new(b"not valid json"),
            &mut out,
        );
        assert!(
            matches!(result, Err(EnvelopeCliError::Deserialize(_))),
            "expected Deserialize error for malformed JSON, got {result:?}",
        );
    }

    /// Build a [`StoredIdentity`] whose key record has the given [`KeyState`]
    /// and `valid_from`, without going through the standard `enroll` path.
    /// Used to inject non-Active states into the registry for verification
    /// tests.
    ///
    /// `valid_from` must be set to a time that satisfies the registry's
    /// consistency constraints for the chosen state (e.g., before `revoked_at`
    /// for `Revoked`, before `valid_until` for `Deprecated`).
    fn register_operator_with_key_state(
        registry: &IdentityRegistry,
        keychain: &MemoryKeyStore,
        name: &str,
        valid_from: OffsetDateTime,
        state: KeyState,
    ) -> StoredIdentity {
        let keypair = Keypair::generate();
        let (private, public) = keypair.into_parts();

        let identity = Identity::new_operator(name.to_owned()).unwrap();
        let mut key_record = KeyRecord::new(identity.identity_id, public).unwrap();
        key_record.valid_from = valid_from;
        key_record.state = state;

        let stored = StoredIdentity::new(identity, key_record).unwrap();
        registry.write(&stored).unwrap();

        let seed = private.to_seed_bytes();
        keychain
            .store(stored.identity().identity_id, &seed)
            .unwrap();

        stored
    }

    #[test]
    fn verify_rejects_envelope_signed_with_revoked_key() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        // valid_from must be before revoked_at for the registry to accept the record.
        let valid_from = OffsetDateTime::now_utc() - Duration::hours(2);
        let revoked_at = valid_from + Duration::hours(1);
        register_operator_with_key_state(
            &registry,
            &keychain,
            "Eve",
            valid_from,
            KeyState::Revoked { revoked_at },
        );
        let recipient = register_recipient(&registry, "worker-r");

        // Sign using the actual private key (sign path only checks Active for
        // the outgoing operator key; revocation is a verify-time check).
        let mut signed_out = Vec::new();
        // Sign directly: bypass the sign() function's operator_key lookup
        // by constructing the envelope manually with the revoked key's key_id.
        {
            let stored = registry.list().unwrap();
            let operator = find_existing_operator(&stored).unwrap();
            let key_record = operator.key_records().first().unwrap();

            let seed = keychain.retrieve(operator.identity().identity_id).unwrap();
            let private_key = PrivateKey::from_seed_bytes(&seed);

            let message_id = MessageId::new().unwrap();
            let nonce = fresh_nonce();
            let payload_hash = sha256_hash(b"revoked body");

            let mut envelope = Envelope::new(
                SchemaVersion::V1,
                message_id,
                operator.identity().identity_id,
                key_record.key_id,
                recipient.identity().identity_id,
                OffsetDateTime::now_utc(),
                nonce,
                payload_hash,
                b"revoked body".to_vec(),
                EnvelopeSignature::from_bytes([PLACEHOLDER_SIG_BYTE; SIGNATURE_LEN]),
            );
            let sig = sign_envelope(&envelope, &private_key).unwrap();
            envelope.signature = sig;

            let json = serde_json::to_string_pretty(&envelope).unwrap();
            signed_out.extend_from_slice(json.as_bytes());
        }

        let err = verify(&registry, &mut signed_out.as_slice(), &mut Vec::new()).unwrap_err();
        assert!(
            matches!(err, EnvelopeCliError::KeyRevoked { .. }),
            "expected KeyRevoked, got {err}",
        );
    }

    #[test]
    fn verify_rejects_envelope_signed_with_deprecated_expired_key() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        // valid_from before valid_until (satisfies registry constraint);
        // valid_until in the past so any envelope created now is after it.
        let valid_from = OffsetDateTime::now_utc() - Duration::hours(4);
        let valid_until = OffsetDateTime::now_utc() - Duration::hours(2);
        register_operator_with_key_state(
            &registry,
            &keychain,
            "Fermat",
            valid_from,
            KeyState::Deprecated { valid_until },
        );
        let recipient = register_recipient(&registry, "worker-d");

        let mut signed_out = Vec::new();
        {
            let stored = registry.list().unwrap();
            let operator = find_existing_operator(&stored).unwrap();
            let key_record = operator.key_records().first().unwrap();

            let seed = keychain.retrieve(operator.identity().identity_id).unwrap();
            let private_key = PrivateKey::from_seed_bytes(&seed);

            let message_id = MessageId::new().unwrap();
            let nonce = fresh_nonce();
            let payload_hash = sha256_hash(b"deprecated body");

            // created_at is now — which is after valid_until.
            let mut envelope = Envelope::new(
                SchemaVersion::V1,
                message_id,
                operator.identity().identity_id,
                key_record.key_id,
                recipient.identity().identity_id,
                OffsetDateTime::now_utc(),
                nonce,
                payload_hash,
                b"deprecated body".to_vec(),
                EnvelopeSignature::from_bytes([PLACEHOLDER_SIG_BYTE; SIGNATURE_LEN]),
            );
            let sig = sign_envelope(&envelope, &private_key).unwrap();
            envelope.signature = sig;

            let json = serde_json::to_string_pretty(&envelope).unwrap();
            signed_out.extend_from_slice(json.as_bytes());
        }

        let err = verify(&registry, &mut signed_out.as_slice(), &mut Vec::new()).unwrap_err();
        assert!(
            matches!(err, EnvelopeCliError::KeyNoLongerValid { .. }),
            "expected KeyNoLongerValid, got {err}",
        );
    }

    #[test]
    fn verify_accepts_envelope_signed_within_deprecation_window() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        // valid_from in the past; valid_until in the future so now is within
        // the window and the envelope should verify.
        let valid_from = OffsetDateTime::now_utc() - Duration::hours(1);
        let valid_until = OffsetDateTime::now_utc() + Duration::hours(24);
        register_operator_with_key_state(
            &registry,
            &keychain,
            "Gauss",
            valid_from,
            KeyState::Deprecated { valid_until },
        );
        let recipient = register_recipient(&registry, "worker-w");

        let mut signed_out = Vec::new();
        {
            let stored = registry.list().unwrap();
            let operator = find_existing_operator(&stored).unwrap();
            let key_record = operator.key_records().first().unwrap();

            let seed = keychain.retrieve(operator.identity().identity_id).unwrap();
            let private_key = PrivateKey::from_seed_bytes(&seed);

            let message_id = MessageId::new().unwrap();
            let nonce = fresh_nonce();
            let created_at = OffsetDateTime::now_utc();
            let payload_hash = sha256_hash(b"within window");

            let mut envelope = Envelope::new(
                SchemaVersion::V1,
                message_id,
                operator.identity().identity_id,
                key_record.key_id,
                recipient.identity().identity_id,
                created_at,
                nonce,
                payload_hash,
                b"within window".to_vec(),
                EnvelopeSignature::from_bytes([PLACEHOLDER_SIG_BYTE; SIGNATURE_LEN]),
            );
            let sig = sign_envelope(&envelope, &private_key).unwrap();
            envelope.signature = sig;

            let json = serde_json::to_string_pretty(&envelope).unwrap();
            signed_out.extend_from_slice(json.as_bytes());
        }

        let mut out = Vec::new();
        verify(&registry, &mut signed_out.as_slice(), &mut out).unwrap();
        let result = String::from_utf8(out).unwrap();
        assert!(result.contains("verified"), "expected verified: {result}");
    }
}
