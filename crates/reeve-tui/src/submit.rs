//! Envelope signing and atomic submission for the Reeve TUI.
//!
//! [`submit_message`] builds a signed [`reeve_types::Envelope`] addressed from
//! the operator identity to the lead agent identity, then writes it to the lead
//! agent's maildir inbox via a tmp → rename atomic sequence following the
//! Maildir convention:
//!
//! 1. Write to `inbox/tmp/<uuid>.json`
//! 2. `rename` to `inbox/new/<uuid>.json`
//!
//! This guarantees the inbox watcher never observes a partial file.
//!
//! # Signing
//!
//! The lead agent's [`reeve_types::IdentityId`] is read from the spawn snapshot
//! (`agents/lead/agent.toml`). The operator's signing seed is retrieved from
//! the platform keystore. The envelope is built and signed using the same
//! pattern as [`reeve_cli::envelope::sign`].

use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use time::OffsetDateTime;

use reeve_runtime::{AgentDirs, IdentityRegistry, OperatorKeyStore, SpawnSnapshot, StoredIdentity};
use reeve_transport::sign_envelope;
use reeve_types::{
    Envelope, EnvelopeSignature, IdentityType, KeyId, KeyState, MessageId, Nonce, PayloadHash,
    PrivateKey, SchemaVersion, NONCE_LEN, PAYLOAD_HASH_LEN, SIGNATURE_LEN,
};

// ── Error type ─────────────────────────────────────────────────────────────────

/// Errors produced by [`submit_message`].
#[derive(Debug)]
pub enum SubmitError {
    /// A filesystem operation (open, write, rename) failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A fresh `UUIDv7` message id could not be minted (clock skew or
    /// monotonicity violation).
    MintMessageId(reeve_types::MessageIdError),
    /// JSON serialisation of the signed envelope failed.
    Serialize(serde_json::Error),
    /// The spawn snapshot (`agent.toml`) could not be parsed.
    ReadSnapshot(serde_json::Error),
    /// The spawn snapshot exists but `agent_id` is absent or not a valid UUID.
    AgentIdMissing,
    /// No operator identity is enrolled on this machine.
    NoOperatorEnrolled,
    /// The operator identity has no key records.
    OperatorHasNoActiveKey,
    /// The identity registry could not be listed or read.
    Registry(reeve_runtime::RegistryError),
    /// The OS keychain could not supply the operator's signing seed.
    Keychain(reeve_runtime::KeychainError),
    /// Signing the envelope failed.
    Sign(reeve_transport::SignError),
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "submit IO at {}: {source}", path.display())
            }
            Self::MintMessageId(err) => write!(f, "submit failed to mint message id: {err}"),
            Self::Serialize(err) => write!(f, "submit JSON serialization failed: {err}"),
            Self::ReadSnapshot(err) => {
                write!(f, "submit failed to parse spawn snapshot: {err}")
            }
            Self::AgentIdMissing => f.write_str(
                "spawn snapshot has no valid agent_id; \
                 restart the daemon to refresh it",
            ),
            Self::NoOperatorEnrolled => {
                f.write_str("no operator identity enrolled; run `reeve identity enroll` first")
            }
            Self::OperatorHasNoActiveKey => f.write_str(
                "operator identity has no key records; re-enroll or restore the keychain entry",
            ),
            Self::Registry(err) => write!(f, "identity registry error: {err}"),
            Self::Keychain(err) => write!(f, "keychain error: {err}"),
            Self::Sign(err) => write!(f, "envelope sign error: {err}"),
        }
    }
}

impl std::error::Error for SubmitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::MintMessageId(err) => Some(err),
            Self::Serialize(err) | Self::ReadSnapshot(err) => Some(err),
            Self::Registry(err) => Some(err),
            Self::Keychain(err) => Some(err),
            Self::Sign(err) => Some(err),
            Self::AgentIdMissing
            | Self::NoOperatorEnrolled
            | Self::OperatorHasNoActiveKey => None,
        }
    }
}

// ── submit_message ─────────────────────────────────────────────────────────────

/// Write `payload` to the lead agent's inbox as a signed envelope.
///
/// Reads the lead agent's identity from the spawn snapshot, retrieves the
/// operator's signing key from `keystore`, builds and signs an
/// [`reeve_types::Envelope`], then atomically writes it to
/// `inbox/new/<message_id>.json` via a tmp → rename.
///
/// # Errors
///
/// Returns [`SubmitError::AgentIdMissing`] when the spawn snapshot has no
/// valid `agent_id` (daemon not yet started or old snapshot). Returns
/// [`SubmitError::NoOperatorEnrolled`] when no operator is enrolled. Returns
/// [`SubmitError::Io`] on filesystem errors.
pub fn submit_message(
    payload: &str,
    dirs: &AgentDirs,
    registry: &IdentityRegistry,
    keystore: &dyn OperatorKeyStore,
) -> Result<(), SubmitError> {
    // 1. Read and parse the spawn snapshot to get the agent's identity id.
    let snapshot = read_snapshot(dirs)?;
    let recipient_id = snapshot
        .agent_identity_id()
        .ok_or(SubmitError::AgentIdMissing)?;

    // 2. Find the operator identity and its active key.
    let stored = registry.list().map_err(SubmitError::Registry)?;
    let operator = find_operator(&stored).ok_or(SubmitError::NoOperatorEnrolled)?;
    let operator_key = operator
        .key_records()
        .first()
        .ok_or(SubmitError::OperatorHasNoActiveKey)?;

    // Only Active keys may sign new messages; Deprecated and Revoked are rejected
    // by the Watcher's verification pipeline regardless, so bail early.
    if !matches!(operator_key.state, KeyState::Active) {
        return Err(SubmitError::OperatorHasNoActiveKey);
    }

    // 3. Retrieve the operator's private key seed from the keystore.
    let seed = keystore
        .retrieve(operator.identity().identity_id)
        .map_err(SubmitError::Keychain)?;
    let private_key = PrivateKey::from_seed_bytes(&seed);

    // 4. Build the envelope.
    let message_id = MessageId::new().map_err(SubmitError::MintMessageId)?;
    let nonce = fresh_nonce();
    let body = payload.as_bytes();
    let payload_hash = sha256_hash(body);
    let sender_id = operator.identity().identity_id;
    let sender_key_id: KeyId = operator_key.key_id;

    let mut envelope = Envelope::new(
        SchemaVersion::V1,
        message_id,
        sender_id,
        sender_key_id,
        recipient_id,
        OffsetDateTime::now_utc(),
        nonce,
        payload_hash,
        body.to_vec(),
        EnvelopeSignature::from_bytes([0xDE; SIGNATURE_LEN]),
    );

    // 5. Sign and embed the real signature.
    let sig = sign_envelope(&envelope, &private_key).map_err(SubmitError::Sign)?;
    envelope.signature = sig;

    // 6. Serialise the signed envelope.
    let json = serde_json::to_string(&envelope).map_err(SubmitError::Serialize)?;

    // 7. Atomic tmp → rename into inbox/new/.
    let filename = format!("{message_id}.json");
    let new_path = dirs.inbox_root().join("new").join(&filename);
    let tmp_dir = dirs.inbox_root().join("tmp");

    let mut tmp = NamedTempFile::new_in(&tmp_dir).map_err(|source| SubmitError::Io {
        path: tmp_dir.clone(),
        source,
    })?;

    tmp.write_all(json.as_bytes())
        .map_err(|source| SubmitError::Io {
            path: tmp_dir.clone(),
            source,
        })?;

    tmp.as_file()
        .sync_data()
        .map_err(|source| SubmitError::Io {
            path: tmp_dir.clone(),
            source,
        })?;

    #[cfg(unix)]
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| SubmitError::Io {
            path: tmp_dir.clone(),
            source,
        })?;

    tmp.persist(&new_path).map_err(|e| SubmitError::Io {
        path: new_path,
        source: e.error,
    })?;

    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Read and parse the spawn snapshot from `dirs.agent_toml_path()`.
fn read_snapshot(dirs: &AgentDirs) -> Result<SpawnSnapshot, SubmitError> {
    let path = dirs.agent_toml_path();
    let raw = fs::read_to_string(&path).map_err(|source| SubmitError::Io { path, source })?;
    toml::from_str(&raw).map_err(|e| {
        // toml::de::Error does not implement std::error::Error directly in all
        // versions; convert via its Display string into a serde_json::Error
        // proxy to keep the variant type consistent. We use a custom workaround:
        // wrap as an IO error so the caller gets a useful message.
        SubmitError::Io {
            path: dirs.agent_toml_path(),
            source: std::io::Error::other(e.to_string()),
        }
    })
}

/// Find the first stored identity with `IdentityType::Operator`.
fn find_operator(stored: &[StoredIdentity]) -> Option<&StoredIdentity> {
    stored
        .iter()
        .find(|s| s.identity().identity_type == IdentityType::Operator)
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
