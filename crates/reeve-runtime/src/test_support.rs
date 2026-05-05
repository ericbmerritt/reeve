//! Shared test fixtures for reeve-runtime integration tests.

use reeve_transport::sign::sign_envelope;
use reeve_types::{
    Envelope, EnvelopeSignature, IdentityId, KeyId, Keypair, MessageId, Nonce, PayloadHash,
    SchemaVersion, NONCE_LEN, PAYLOAD_HASH_LEN, SIGNATURE_LEN,
};
use time::OffsetDateTime;

/// Create a temporary directory with mode 0o700 on Unix, or a plain tempdir
/// on non-Unix. Used in tests that must satisfy the registry and ledger mode
/// checks.
#[cfg(unix)]
pub(crate) fn secure_dir() -> tempfile::TempDir {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

#[cfg(not(unix))]
pub(crate) fn secure_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Build a signed envelope addressed to `recipient_id` and return both the
/// parsed envelope and its serialized JSON bytes. The envelope is timestamped
/// with `OffsetDateTime::now_utc()` and carries a fixed nonce/payload-hash
/// (sufficient for signing; the hash is not validated by the watcher).
///
/// Fixture limitation: placeholder hash sufficient for current pipeline
/// tests; if payload-hash validation is added, this fixture must be updated.
pub(crate) fn make_signed_envelope(
    keypair: &Keypair,
    sender_id: IdentityId,
    sender_key_id: KeyId,
    recipient_id: IdentityId,
) -> (Envelope, Vec<u8>) {
    signed_envelope_at(
        keypair,
        sender_id,
        sender_key_id,
        recipient_id,
        OffsetDateTime::now_utc(),
    )
}

/// Build a signed envelope addressed to `recipient_id` with an explicit
/// `created_at` timestamp, and return both the parsed envelope and its
/// serialized JSON bytes. Use this variant when a test must control the
/// timestamp (e.g., clock-skew checks, key-expiry checks).
///
/// Fixture limitation: placeholder hash sufficient for current pipeline
/// tests; if payload-hash validation is added, this fixture must be updated.
pub(crate) fn signed_envelope_at(
    keypair: &Keypair,
    sender_id: IdentityId,
    sender_key_id: KeyId,
    recipient_id: IdentityId,
    now: OffsetDateTime,
) -> (Envelope, Vec<u8>) {
    let placeholder = EnvelopeSignature::from_bytes([0u8; SIGNATURE_LEN]);
    let mut env = Envelope::new(
        SchemaVersion::V1,
        MessageId::new().unwrap(),
        sender_id,
        sender_key_id,
        recipient_id,
        now,
        Nonce::from_bytes([0xAAu8; NONCE_LEN]),
        PayloadHash::from_bytes([0xBBu8; PAYLOAD_HASH_LEN]),
        b"hello".to_vec(),
        placeholder,
    );
    let sig = sign_envelope(&env, keypair.private()).unwrap();
    env.signature = sig;
    let bytes = serde_json::to_vec(&env).unwrap();
    (env, bytes)
}
