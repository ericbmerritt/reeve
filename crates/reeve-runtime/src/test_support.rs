//! Shared test fixtures for reeve-runtime integration tests.

use std::path::Path;
use std::sync::Arc;

use reeve_transport::sign::sign_envelope;
use reeve_types::{
    Envelope, EnvelopeSignature, IdentityId, KeyId, Keypair, MessageId, Nonce, PayloadHash,
    SchemaVersion, NONCE_LEN, PAYLOAD_HASH_LEN, SIGNATURE_LEN,
};
use time::OffsetDateTime;

/// Set a file's mtime to year 2000 — far past any realistic retention
/// threshold. Used for tests that just need an "old enough" timestamp
/// and don't care about precise boundary semantics. For exact-boundary
/// tests, see [`set_mtime_at`].
///
/// Unix-only — gated `#[cfg(unix)]`.
#[cfg(unix)]
pub(crate) fn set_ancient_mtime(path: &Path) {
    let status = std::process::Command::new("touch")
        .args(["-t", "200001010000.00", path.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success(), "touch failed for {path:?}");
}

/// Set a file's mtime to a specific UTC timestamp via `touch -d` (RFC 3339).
///
/// **Caller must zero sub-second components** (`ts.replace_nanosecond(0).unwrap()`)
/// to avoid round-trip drift between `OffsetDateTime` nanosecond precision and
/// `touch -d`'s second-precision parsing. A `debug_assert!` enforces this.
///
/// Unix-only — gated `#[cfg(unix)]`.
#[cfg(unix)]
pub(crate) fn set_mtime_at(path: &Path, ts: OffsetDateTime) {
    use time::format_description::well_known::Rfc3339;
    debug_assert!(
        ts.nanosecond() == 0,
        "set_mtime_at requires sub-second components zeroed via replace_nanosecond(0)"
    );
    let formatted = ts.format(&Rfc3339).expect("format failed");
    let status = std::process::Command::new("touch")
        .args(["-d", &formatted, path.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success(), "touch -d failed for {path:?}");
}

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

// ── Shared mock adapter ───────────────────────────────────────────────────────

/// Parametric model adapter stub for tests.
///
/// Accepts any `&'static str` as the adapter id; `call()` always returns
/// `BadRequest` — sufficient for tests that exercise spawn/resolution logic
/// without requiring a live model.
pub(crate) struct MockAdapter {
    id: &'static str,
}

impl MockAdapter {
    pub(crate) fn new(id: &'static str) -> Self {
        Self { id }
    }
}

#[async_trait::async_trait]
impl reeve_adapter::Adapter for MockAdapter {
    fn id(&self) -> &str {
        self.id
    }

    fn capabilities(&self) -> reeve_adapter::Capabilities {
        reeve_adapter::Capabilities::new()
    }

    async fn call(
        &self,
        _messages: &[reeve_adapter::Message],
        _tools: &[reeve_adapter::Tool],
        _params: &reeve_adapter::Params,
    ) -> Result<reeve_adapter::Response, reeve_adapter::AdapterError> {
        Err(reeve_adapter::AdapterError::BadRequest {
            message: String::from("mock adapter does not support calls"),
        })
    }
}

/// Build the identity registry, watcher, and agent registry path for a test
/// data directory.
pub(crate) fn build_registries(
    data_dir: &Path,
) -> (
    Arc<crate::identity_registry::IdentityRegistry>,
    Arc<crate::watcher::Watcher>,
    std::path::PathBuf,
) {
    use crate::audit::AuditLog;
    use crate::identity_registry::IdentityRegistry;
    use crate::ledger::{DeliveryLedger, ReplayLedger};
    use crate::watcher::Watcher;

    let identity_registry = Arc::new(IdentityRegistry::open(data_dir.to_path_buf()).unwrap());
    let replay = Arc::new(ReplayLedger::open(data_dir.to_path_buf()).unwrap());
    let delivery = Arc::new(DeliveryLedger::open(data_dir.to_path_buf()).unwrap());
    let audit = Arc::new(AuditLog::open(data_dir.to_path_buf()).unwrap());
    let agent_registry_path = data_dir.join("agents").join("registry.toml");
    let watcher = Arc::new(Watcher::new(
        &identity_registry,
        &replay,
        delivery,
        audit,
        agent_registry_path.clone(),
    ));
    (identity_registry, watcher, agent_registry_path)
}
