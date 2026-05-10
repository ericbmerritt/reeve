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

// ── Inbox provisioning ───────────────────────────────────────────────────────

/// Create a minimal inbox layout at `root/inbox/{tmp,new}` with mode 0o700 on
/// Unix. Used by dispatcher tests whose `AgentRecord.inbox_dir` points to
/// `root/inbox/` and need the staging and delivery subdirectories to exist.
///
/// Note: this creates only the subdirectories required by the dispatcher.
/// Watcher tests that need `cur/`, `quarantine/`, and `archive/` should use
/// [`crate::inbox::InboxLayout::provision`] instead.
#[cfg(unix)]
pub(crate) fn provision_inbox(root: &Path) {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    for sub in &["inbox", "inbox/tmp", "inbox/new"] {
        let dir = root.join(sub);
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
}

#[cfg(not(unix))]
pub(crate) fn provision_inbox(root: &Path) {
    use std::fs;
    for sub in &["inbox", "inbox/tmp", "inbox/new"] {
        fs::create_dir_all(root.join(sub)).unwrap();
    }
}

// ── Capture actors ────────────────────────────────────────────────────────────

/// Generates a one-shot capture actor that receives one message of type
/// `$msg` and forwards it through a tokio oneshot sender.
macro_rules! oneshot_capture {
    ($name:ident, $msg:path) => {
        pub(crate) struct $name {
            pub(crate) tx: Option<tokio::sync::oneshot::Sender<$msg>>,
        }

        impl actix::Actor for $name {
            type Context = actix::Context<Self>;
        }

        impl actix::Handler<$msg> for $name {
            type Result = ();

            fn handle(&mut self, msg: $msg, _ctx: &mut actix::Context<Self>) {
                if let Some(tx) = self.tx.take() {
                    let _ = tx.send(msg);
                }
            }
        }
    };
}

oneshot_capture!(ToolResultCapture, crate::tool::ToolResult);
oneshot_capture!(ResponseCapture, crate::spawn_coordinator::SpawnResponse);
oneshot_capture!(SendResultCapture, crate::dispatcher::SendResult);
oneshot_capture!(SendFailedCapture, crate::dispatcher::SendFailed);

// ── Mock spawn coordinator ────────────────────────────────────────────────────

/// Test stub for [`crate::spawn_coordinator::SpawnCoordinator`].
///
/// Accepts [`crate::spawn_coordinator::SpawnRequest`] and always replies with
/// [`crate::spawn_coordinator::SpawnResponse::Success`] using the fixed agent
/// name `"mock-agent"`. Used in tool and agent tests that need a coordinator
/// in the actor tree without exercising the full provisioning sequence.
pub(crate) struct MockSpawnCoordinator;

impl actix::Actor for MockSpawnCoordinator {
    type Context = actix::Context<Self>;
}

impl actix::Handler<crate::spawn_coordinator::SpawnRequest> for MockSpawnCoordinator {
    type Result = ();

    fn handle(
        &mut self,
        msg: crate::spawn_coordinator::SpawnRequest,
        _ctx: &mut actix::Context<Self>,
    ) {
        let id = IdentityId::new().unwrap();
        msg.reply_to()
            .do_send(crate::spawn_coordinator::SpawnResponse::Success {
                agent_name: "mock-agent".to_owned(),
                agent_id: id,
            });
    }
}

/// Capturing coordinator: accepts [`crate::spawn_coordinator::SpawnRequest`],
/// stores the `system_prompt` into a shared slot, and replies `Success`.
///
/// Used in `T_SA7` to verify the `task`+`context` composition without requiring
/// the full provisioning sequence.
pub(crate) struct CapturingSpawnCoordinator {
    pub(crate) last_system_prompt: Arc<std::sync::Mutex<Option<String>>>,
}

impl actix::Actor for CapturingSpawnCoordinator {
    type Context = actix::Context<Self>;
}

impl actix::Handler<crate::spawn_coordinator::SpawnRequest> for CapturingSpawnCoordinator {
    type Result = ();

    fn handle(
        &mut self,
        msg: crate::spawn_coordinator::SpawnRequest,
        _ctx: &mut actix::Context<Self>,
    ) {
        *self.last_system_prompt.lock().unwrap() = Some(msg.system_prompt().to_owned());
        let id = IdentityId::new().unwrap();
        msg.reply_to()
            .do_send(crate::spawn_coordinator::SpawnResponse::Success {
                agent_name: "mock-agent".to_owned(),
                agent_id: id,
            });
    }
}

// ── Null inbox starter ────────────────────────────────────────────────────────

/// Test stub for a [`crate::supervisor::WatchInbox`] recipient.
///
/// Accepts [`crate::supervisor::WatchInbox`] messages and silently discards
/// them. Used in tests that need a valid `inbox_starter` recipient wired into
/// a [`crate::spawn_coordinator::SpawnCoordinator`] without actually starting
/// file watchers.
pub(crate) struct NullInboxStarter;

impl actix::Actor for NullInboxStarter {
    type Context = actix::Context<Self>;
}

impl actix::Handler<crate::supervisor::WatchInbox> for NullInboxStarter {
    type Result = ();

    fn handle(&mut self, _msg: crate::supervisor::WatchInbox, _ctx: &mut actix::Context<Self>) {}
}

// ── Persona config writer ─────────────────────────────────────────────────────

/// `model_pref` must match the prefix of the adapter id used by the test (the part before `'@'`).
pub(crate) fn write_persona_config(data_dir: &Path, name: &str, model_pref: &str) {
    let persona_dir = data_dir.join("personas").join(name);
    std::fs::create_dir_all(&persona_dir).unwrap();
    let config = format!(
        "name = \"{name}\"\nsystem_prompt = \"Be helpful.\"\nmodel_preferences = [\"{model_pref}\"]\n"
    );
    std::fs::write(persona_dir.join("config.toml"), config).unwrap();
}

// ── Shared registry builder ───────────────────────────────────────────────────

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
