//! [`MessageDispatcher`] actor: sign and deposit envelopes into a recipient's
//! `inbox/new/` directory.
//!
//! Accepts [`SendMessage`] requests, looks up sender and recipient in the
//! [`AgentRegistry`], signs the envelope with the sender's on-disk keypair,
//! and atomically deposits it into `recipient_inbox/new/`. Replies with
//! [`SendResult`] on success or [`SendFailed`] on any failure.
//!
//! The dispatcher holds an `Arc<AgentRegistry>` snapshot. Keypairs are NOT
//! held in actor state — they are loaded from disk per-dispatch via
//! [`crate::agent_registry::generate_or_load_keypair`]. A crash never loses
//! keys, and the supervisor can restart the actor without reinitializing key
//! material.
//!
//! ## One-shot reply semantics
//!
//! `reply_to` is typed `Option<Recipient<_>>` and consumed with `.take()` so
//! the first caller delivers and all subsequent paths (error, drop) see `None`
//! and skip — no double-fire, no leak.

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use actix::{Actor, Context, Handler, Message, Recipient, Supervised};
use reeve_types::{
    Envelope, EnvelopeSignature, IdentityId, KeyState, Keypair, MessageId, SchemaVersion,
    SIGNATURE_LEN,
};
use time::OffsetDateTime;
use tracing::warn;

use crate::agent_registry::{
    generate_or_load_keypair, AgentRecord, AgentRegistry, ValidatedAgentName,
};
use crate::fs_util::atomic_write_file;
use crate::identity_registry::{IdentityRegistry, RegistryError};
use crate::verify::MAX_ENVELOPE_BYTES;

/// Mode for envelope files deposited in `inbox/new/`.
const ENVELOPE_FILE_MODE: u32 = 0o600;

// ── Error type ─────────────────────────────────────────────────────────────────

/// Errors that can occur while dispatching a message.
#[derive(Debug)]
pub enum SendError {
    /// The named recipient is not in the agent registry.
    RecipientNotFound { to_name: String },
    /// The sender identity ID is not in the agent registry.
    SenderNotFound { from_id: IdentityId },
    /// The sender's identity is in the agent registry but has no key record
    /// in the identity registry. The identity must be enrolled before sending.
    KeyNotFound { identity_id: IdentityId },
    /// Failed to look up the sender identity in the identity registry. Distinct
    /// from `KeyNotFound`: this variant means the registry itself returned an
    /// error (I/O, parse, etc.), not that the identity is simply absent.
    IdentityLookupFailed { source: RegistryError },
    /// Failed to load or derive the sender's keypair from disk.
    KeypairLoad {
        path: PathBuf,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Envelope signing failed.
    SigningFailed(String),
    /// I/O error while writing the envelope file.
    Io { path: PathBuf, source: io::Error },
    /// Message body or serialized envelope exceeds the maximum allowed size.
    /// `len` is the raw body length when the pre-check fires, or the
    /// serialized envelope length when the post-check fires.
    BodyTooLarge { len: usize },
    /// A fresh message ID could not be generated (clock skew or monotonicity
    /// violation — not a signing operation).
    MessageIdFailed(String),
    /// A path in the inbox layout was a symlink; delivery refused as a
    /// security policy. The `path` is the symlinked inbox component.
    SymlinkRejected { path: PathBuf },
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RecipientNotFound { to_name } => {
                write!(f, "recipient '{to_name}' not found in agent registry")
            }
            Self::SenderNotFound { from_id } => {
                write!(f, "sender identity '{from_id}' not found in agent registry")
            }
            Self::KeyNotFound { identity_id } => {
                write!(
                    f,
                    "no key record in identity registry for sender {identity_id}"
                )
            }
            Self::IdentityLookupFailed { source } => {
                write!(f, "identity registry lookup failed: {source}")
            }
            Self::KeypairLoad { path, source } => {
                write!(
                    f,
                    "failed to load keypair from {}: {source}",
                    path.display()
                )
            }
            Self::SigningFailed(detail) => write!(f, "envelope signing failed: {detail}"),
            Self::Io { path, source } => {
                write!(f, "envelope write IO at {}: {source}", path.display())
            }
            Self::BodyTooLarge { len } => {
                write!(
                    f,
                    "message body or serialized envelope ({len} bytes) exceeds the {MAX_ENVELOPE_BYTES}-byte cap"
                )
            }
            Self::MessageIdFailed(detail) => {
                write!(f, "failed to generate message ID: {detail}")
            }
            Self::SymlinkRejected { path } => {
                write!(
                    f,
                    "inbox path is a symlink (delivery refused): {}",
                    path.display()
                )
            }
        }
    }
}

impl SendError {
    /// Classifier for the error variant. Used in `warn!` and surfaced to model
    /// output by tool handlers as a stable, path-free identifier. The `Io` and
    /// `KeypairLoad` variants embed filesystem paths in their `Display` output;
    /// callers that report errors to untrusted readers must use this instead of
    /// `to_string()`.
    #[must_use]
    pub fn category(&self) -> &'static str {
        match self {
            Self::RecipientNotFound { .. } => "RecipientNotFound",
            Self::SenderNotFound { .. } => "SenderNotFound",
            Self::KeyNotFound { .. } => "KeyNotFound",
            Self::IdentityLookupFailed { .. } => "IdentityLookupFailed",
            Self::KeypairLoad { .. } => "KeypairLoad",
            Self::SigningFailed(_) => "SigningFailed",
            Self::Io { .. } => "Io",
            Self::BodyTooLarge { .. } => "BodyTooLarge",
            Self::MessageIdFailed(_) => "MessageIdFailed",
            Self::SymlinkRejected { .. } => "SymlinkRejected",
        }
    }
}

impl std::error::Error for SendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::KeypairLoad { source, .. } => Some(source.as_ref()),
            Self::Io { source, .. } => Some(source),
            Self::IdentityLookupFailed { source } => Some(source),
            Self::RecipientNotFound { .. }
            | Self::SenderNotFound { .. }
            | Self::KeyNotFound { .. }
            | Self::SigningFailed(_)
            | Self::BodyTooLarge { .. }
            | Self::MessageIdFailed(_)
            | Self::SymlinkRejected { .. } => None,
        }
    }
}

// ── Messages ──────────────────────────────────────────────────────────────────

/// Dispatch a signed message from `from_id` to `to_name`.
pub struct SendMessage {
    /// Identity of the sending agent. Must be set from the calling agent
    /// actor's own identity state by the runtime, not from untrusted tool
    /// call input.
    pub from_id: IdentityId,
    /// Agent name of the recipient (used for registry lookup).
    pub to_name: ValidatedAgentName,
    /// Message body. The dispatcher enforces the size cap; content
    /// validation is the caller's responsibility before constructing
    /// `SendMessage`.
    pub body: String,
    /// One-shot reply channel for success. First successful call delivers;
    /// error paths and drop see `None` and skip — `.take()` semantics enforced
    /// by the handler.
    pub reply_to: Option<Recipient<SendResult>>,
    /// One-shot error channel. On failure the handler takes this and delivers
    /// a [`SendFailed`] carrying the typed error. When `None`, dispatch errors
    /// are logged at `warn!` and dropped — callers that need error feedback
    /// must provide a channel.
    ///
    /// Note: `SendError::Io` and `SendError::KeypairLoad` variants expose
    /// filesystem paths in their `Display` output. Tool handlers that surface
    /// error details to model output must use `error.category()` rather than
    /// `error.to_string()` to avoid leaking filesystem paths.
    pub error_to: Option<Recipient<SendFailed>>,
}

impl Message for SendMessage {
    type Result = ();
}

/// Successful dispatch outcome.
#[derive(Debug)]
pub struct SendResult {
    /// The message ID embedded in the deposited envelope.
    pub message_id: MessageId,
}

impl Message for SendResult {
    type Result = ();
}

/// Failed dispatch outcome.
#[derive(Debug)]
pub struct SendFailed {
    pub error: SendError,
}

impl Message for SendFailed {
    type Result = ();
}

// ── Actor ─────────────────────────────────────────────────────────────────────

/// Supervised actor that signs and deposits message envelopes.
///
/// Holds a snapshot of the agent registry and the identity registry. Keypairs
/// are loaded from disk on each dispatch so a crash never loses key material
/// and the supervisor can restart the actor cleanly.
pub struct MessageDispatcher {
    agent_registry: Arc<AgentRegistry>,
    identity_registry: Arc<IdentityRegistry>,
}

impl MessageDispatcher {
    /// Construct a dispatcher with the given registry snapshots.
    pub fn new(
        agent_registry: Arc<AgentRegistry>,
        identity_registry: Arc<IdentityRegistry>,
    ) -> Self {
        Self {
            agent_registry,
            identity_registry,
        }
    }
}

impl Actor for MessageDispatcher {
    type Context = Context<Self>;
}

impl Supervised for MessageDispatcher {}

impl Handler<SendMessage> for MessageDispatcher {
    type Result = ();

    fn handle(&mut self, mut msg: SendMessage, _ctx: &mut Context<Self>) {
        match dispatch(
            &self.agent_registry,
            &self.identity_registry,
            msg.from_id,
            &msg.to_name,
            &msg.body,
        ) {
            Ok(message_id) => {
                if let Some(tx) = msg.reply_to.take() {
                    tx.do_send(SendResult { message_id });
                }
            }
            Err(err) => {
                warn!(
                    from_id = %msg.from_id,
                    to_name = %msg.to_name,
                    error = err.category(),
                    "MessageDispatcher: send failed"
                );
                if let Some(tx) = msg.error_to.take() {
                    tx.do_send(SendFailed { error: err });
                }
            }
        }
    }
}

// ── Core dispatch logic ───────────────────────────────────────────────────────

fn dispatch(
    registry: &AgentRegistry,
    identity_registry: &IdentityRegistry,
    from_id: IdentityId,
    to_name: &ValidatedAgentName,
    body: &str,
) -> Result<MessageId, SendError> {
    let recipient_record =
        registry
            .lookup(to_name.as_str())
            .ok_or_else(|| SendError::RecipientNotFound {
                to_name: to_name.to_string(),
            })?;

    let sender_record = registry
        .list()
        .find(|r| r.identity_id == from_id)
        .ok_or(SendError::SenderNotFound { from_id })?;

    // The real KeyId is what verify.rs will check on the receiving side;
    // it must come from the identity registry, not be synthesized locally.
    let stored = identity_registry
        .lookup(sender_record.identity_id)
        .map_err(|source| SendError::IdentityLookupFailed { source })?
        .ok_or(SendError::KeyNotFound {
            identity_id: sender_record.identity_id,
        })?;
    let active_record = stored
        .key_records()
        .iter()
        .find(|kr| kr.state == KeyState::Active)
        .ok_or(SendError::KeyNotFound {
            identity_id: sender_record.identity_id,
        })?;
    let sender_key_id = active_record.key_id;

    // Fast pre-check: body at or above MAX_ENVELOPE_BYTES will produce a
    // serialized envelope that exceeds the cap. Reject here to avoid
    // keypair disk I/O and pointless crypto work on oversized input. The
    // post-serialization check below is still required for bodies below
    // the limit whose envelope overhead pushes the total over the cap.
    if body.len() >= MAX_ENVELOPE_BYTES {
        return Err(SendError::BodyTooLarge { len: body.len() });
    }

    let keypair = load_keypair(sender_record)?;

    let message_id = MessageId::new().map_err(|e| SendError::MessageIdFailed(e.to_string()))?;

    // Deferred to after remaining failure checks (registry lookups, pre-size check,
    // keypair load) to avoid allocating for messages that will be rejected
    // before this point.
    let body_bytes = body.as_bytes().to_vec();
    let placeholder_sig = EnvelopeSignature::from_bytes([0xDE; SIGNATURE_LEN]);
    let nonce = reeve_transport::fresh_nonce();
    let payload_hash = reeve_transport::sha256_payload_hash(&body_bytes);

    let mut envelope = Envelope::new(
        SchemaVersion::V1,
        message_id,
        sender_record.identity_id,
        sender_key_id,
        recipient_record.identity_id,
        OffsetDateTime::now_utc(),
        nonce,
        payload_hash,
        body_bytes,
        placeholder_sig,
    );

    let sig = reeve_transport::sign::sign_envelope(&envelope, keypair.private())
        .map_err(|e| SendError::SigningFailed(e.to_string()))?;
    envelope.signature = sig;

    // serde_json::to_vec cannot fail on Envelope: body_serde uses base64::encode
    // (infallible), created_at_serde formats a UTC-normalized OffsetDateTime,
    // and all remaining fields use derived Serialize with only primitive types.
    let envelope_bytes = serde_json::to_vec(&envelope)
        .unwrap_or_else(|e| unreachable!("Envelope serialization is infallible: {e}"));

    // Body size cap — must precede disk write. Checked against the serialized
    // envelope (not the raw body) because JSON overhead (~400 B for base64
    // fields, UUIDs, timestamps) means a body at the limit produces an envelope
    // that exceeds it.
    if envelope_bytes.len() > MAX_ENVELOPE_BYTES {
        return Err(SendError::BodyTooLarge {
            len: envelope_bytes.len(),
        });
    }

    deposit_envelope(recipient_record, &message_id.to_string(), &envelope_bytes)?;

    Ok(message_id)
}

/// Load the sender's keypair from the identity key file adjacent to their inbox.
fn load_keypair(sender: &AgentRecord) -> Result<Keypair, SendError> {
    // AgentDirs lays out: root/inbox/, root/identity.key.
    // AgentRecord.inbox_dir == root/inbox/ → parent is root/.
    let agent_root = sender
        .inbox_dir
        .parent()
        .ok_or_else(|| SendError::KeypairLoad {
            path: sender.inbox_dir.clone(),
            source: Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                "inbox_dir has no parent; cannot locate identity.key",
            )),
        })?;
    let key_path = agent_root.join("identity.key");
    generate_or_load_keypair(&key_path).map_err(|e| SendError::KeypairLoad {
        path: key_path,
        source: Box::new(e),
    })
}

fn deposit_envelope(
    recipient: &AgentRecord,
    message_id: &str,
    bytes: &[u8],
) -> Result<(), SendError> {
    let inbox_root = &recipient.inbox_dir;

    // Check inbox_root itself before constructing subpaths — a symlinked
    // inbox_root would cause the subpath checks to follow through the symlink.
    let root_meta = std::fs::symlink_metadata(inbox_root).map_err(|source| SendError::Io {
        path: inbox_root.clone(),
        source,
    })?;
    if root_meta.file_type().is_symlink() {
        return Err(SendError::SymlinkRejected {
            path: inbox_root.clone(),
        });
    }

    let tmp_dir = inbox_root.join("tmp");
    let new_dir = inbox_root.join("new");
    let new_path = new_dir.join(message_id);

    for dir in [&tmp_dir, &new_dir] {
        let meta = std::fs::symlink_metadata(dir).map_err(|source| SendError::Io {
            path: dir.clone(),
            source,
        })?;
        if meta.file_type().is_symlink() {
            return Err(SendError::SymlinkRejected { path: dir.clone() });
        }
    }

    atomic_write_file(&new_path, &tmp_dir, bytes, ENVELOPE_FILE_MODE).map_err(|source| {
        SendError::Io {
            path: new_path,
            source,
        }
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::time::Duration;

    use time::OffsetDateTime;

    use reeve_types::{Identity, IdentityId, KeyRecord, KeyState};

    use crate::agent_registry::{
        generate_or_load_keypair, AgentRecord, AgentRegistry, AgentStatus, ValidatedAgentName,
    };
    use crate::identity_registry::{IdentityRegistry, StoredIdentity};
    use crate::test_support::{provision_inbox, secure_dir, SendFailedCapture, SendResultCapture};
    use crate::verify::MAX_ENVELOPE_BYTES;

    use super::*;

    // ── Test helpers ──────────────────────────────────────────────────────────

    /// Build a two-agent registry (sender + recipient) with a paired identity
    /// registry. Returns `(agent_registry, identity_registry, sender_root,
    /// recipient_root, sender_id, recipient_id)`.
    fn build_test_registry(
        base: &std::path::Path,
    ) -> (
        AgentRegistry,
        IdentityRegistry,
        PathBuf,
        PathBuf,
        IdentityId,
        IdentityId,
    ) {
        let agent_registry_path = base.join("registry.toml");
        let mut agent_registry = AgentRegistry::open(agent_registry_path).unwrap();

        let identity_registry_dir = base.join("identities");
        fs::create_dir_all(&identity_registry_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&identity_registry_dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let identity_registry = IdentityRegistry::open(identity_registry_dir).unwrap();

        let operator_id = IdentityId::new().unwrap();

        // Sender agent
        let sender_root = base.join("sender");
        fs::create_dir_all(&sender_root).unwrap();
        provision_inbox(&sender_root);
        let sender_id = IdentityId::new().unwrap();
        let sender_record = AgentRecord {
            name: ValidatedAgentName::new("sender").unwrap(),
            identity_id: sender_id,
            inbox_dir: sender_root.join("inbox"),
            persona_name: None,
            spawned_at: OffsetDateTime::now_utc(),
            status: AgentStatus::Running,
        };
        agent_registry.register(sender_record).unwrap();

        // Register sender in the identity registry with a real keypair.
        let sender_keypair = generate_or_load_keypair(&sender_root.join("identity.key")).unwrap();
        let sender_public = *sender_keypair.public();
        let sender_identity = Identity::new_agent("sender".to_owned(), operator_id).unwrap();
        // Identity::new_agent generates a fresh identity_id; override it to
        // match the agent record.
        let sender_identity = {
            let mut id = sender_identity;
            id.identity_id = sender_id;
            id
        };
        let sender_key_record = KeyRecord::new(sender_id, sender_public).unwrap();
        let sender_stored = StoredIdentity::new(sender_identity, sender_key_record).unwrap();
        identity_registry.write(&sender_stored).unwrap();

        // Recipient agent
        let recipient_root = base.join("recipient");
        fs::create_dir_all(&recipient_root).unwrap();
        provision_inbox(&recipient_root);
        let recipient_id = IdentityId::new().unwrap();
        let recipient_record = AgentRecord {
            name: ValidatedAgentName::new("recipient").unwrap(),
            identity_id: recipient_id,
            inbox_dir: recipient_root.join("inbox"),
            persona_name: None,
            spawned_at: OffsetDateTime::now_utc(),
            status: AgentStatus::Running,
        };
        agent_registry.register(recipient_record).unwrap();

        (
            agent_registry,
            identity_registry,
            sender_root,
            recipient_root,
            sender_id,
            recipient_id,
        )
    }

    // ── D1: happy path ────────────────────────────────────────────────────────

    /// D1: Dispatching to a known recipient deposits a file in `inbox/new/`,
    /// the reply carries a valid `message_id`, the deposited envelope contains
    /// the correct sender, recipient, and body, and the envelope signature is valid.
    #[test]
    fn dispatch_happy_path_deposits_file_and_returns_message_id() {
        let tmp = secure_dir();
        let base = tmp.path();

        let (
            agent_registry,
            identity_registry,
            sender_root,
            recipient_root,
            sender_id,
            recipient_id,
        ) = build_test_registry(base);

        let arc_agent = Arc::new(agent_registry);
        let arc_identity = Arc::new(identity_registry);

        actix::System::new().block_on(async {
            let (tx, rx) = tokio::sync::oneshot::channel::<SendResult>();

            let capture = SendResultCapture { tx: Some(tx) }.start();

            let dispatcher =
                MessageDispatcher::new(Arc::clone(&arc_agent), Arc::clone(&arc_identity));
            let dispatcher_addr = actix::Supervisor::start(move |_| dispatcher);

            dispatcher_addr.do_send(SendMessage {
                from_id: sender_id,
                to_name: ValidatedAgentName::new("recipient").unwrap(),
                body: "hello from sender".to_owned(),
                reply_to: Some(capture.recipient()),
                error_to: None,
            });

            let result = tokio::time::timeout(Duration::from_millis(500), rx)
                .await
                .expect("timed out waiting for SendResult")
                .expect("oneshot sender dropped");

            let new_dir = recipient_root.join("inbox").join("new");
            let entries: Vec<_> = fs::read_dir(&new_dir).unwrap().flatten().collect();
            assert_eq!(
                entries.len(),
                1,
                "exactly one file should be in recipient inbox/new/"
            );
            assert_eq!(
                entries[0].file_name().to_string_lossy(),
                result.message_id.to_string(),
                "filename must match the message_id"
            );

            // Verify the deposited envelope has the correct routing and body.
            let bytes = fs::read(entries[0].path()).unwrap();
            let envelope: Envelope = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                envelope.sender_id, sender_id,
                "envelope sender_id must match the sending agent"
            );
            assert_eq!(
                envelope.recipient_id, recipient_id,
                "envelope recipient_id must match the intended recipient"
            );
            assert!(
                String::from_utf8(envelope.body.clone())
                    .unwrap()
                    .contains("hello from sender"),
                "envelope body must contain the original message"
            );

            // Verify the signature is valid (not a zero placeholder).
            let sender_keypair =
                generate_or_load_keypair(&sender_root.join("identity.key")).unwrap();
            let verify_result =
                reeve_transport::sign::verify_envelope(&envelope, sender_keypair.public());
            assert!(
                verify_result.is_ok(),
                "deposited envelope signature must be valid: {verify_result:?}"
            );

            actix::System::current().stop();
        });
    }

    // ── D2: unknown recipient ─────────────────────────────────────────────────

    /// D2: Sending to an unknown recipient delivers `SendFailed` with
    /// `RecipientNotFound` on the error channel.
    #[test]
    fn dispatch_unknown_recipient_delivers_error() {
        let tmp = secure_dir();
        let base = tmp.path();

        let (agent_registry, identity_registry, _sender_root, _recipient_root, sender_id, _) =
            build_test_registry(base);

        let arc_agent = Arc::new(agent_registry);
        let arc_identity = Arc::new(identity_registry);

        actix::System::new().block_on(async {
            let (err_tx, err_rx) = tokio::sync::oneshot::channel::<SendFailed>();
            let err_capture = SendFailedCapture { tx: Some(err_tx) }.start();

            let dispatcher =
                MessageDispatcher::new(Arc::clone(&arc_agent), Arc::clone(&arc_identity));
            let dispatcher_addr = actix::Supervisor::start(move |_| dispatcher);

            dispatcher_addr.do_send(SendMessage {
                from_id: sender_id,
                to_name: ValidatedAgentName::new("nonexistent-agent").unwrap(),
                body: "should not arrive".to_owned(),
                reply_to: None,
                error_to: Some(err_capture.recipient()),
            });

            let result = tokio::time::timeout(Duration::from_millis(500), err_rx)
                .await
                .expect("timed out waiting for SendFailed")
                .expect("oneshot sender dropped");

            assert!(
                matches!(result.error, SendError::RecipientNotFound { .. }),
                "expected RecipientNotFound, got {:?}",
                result.error
            );

            actix::System::current().stop();
        });
    }

    // ── D3: unknown sender ────────────────────────────────────────────────────

    /// D3: Sending with an unknown `from_id` delivers `SendFailed` with
    /// `SenderNotFound` on the error channel.
    #[test]
    fn dispatch_unknown_sender_delivers_error() {
        let tmp = secure_dir();
        let base = tmp.path();

        let (agent_registry, identity_registry, _sender_root, _recipient_root, _, _) =
            build_test_registry(base);

        let arc_agent = Arc::new(agent_registry);
        let arc_identity = Arc::new(identity_registry);
        let unknown_from_id = IdentityId::new().unwrap();

        actix::System::new().block_on(async {
            let (err_tx, err_rx) = tokio::sync::oneshot::channel::<SendFailed>();
            let err_capture = SendFailedCapture { tx: Some(err_tx) }.start();

            let dispatcher =
                MessageDispatcher::new(Arc::clone(&arc_agent), Arc::clone(&arc_identity));
            let dispatcher_addr = actix::Supervisor::start(move |_| dispatcher);

            dispatcher_addr.do_send(SendMessage {
                from_id: unknown_from_id,
                to_name: ValidatedAgentName::new("recipient").unwrap(),
                body: "should not arrive".to_owned(),
                reply_to: None,
                error_to: Some(err_capture.recipient()),
            });

            let result = tokio::time::timeout(Duration::from_millis(500), err_rx)
                .await
                .expect("timed out waiting for SendFailed")
                .expect("oneshot sender dropped");

            assert!(
                matches!(result.error, SendError::SenderNotFound { .. }),
                "expected SenderNotFound, got {:?}",
                result.error
            );

            actix::System::current().stop();
        });
    }

    // ── D4: missing inbox/tmp → Io error ─────────────────────────────────────

    /// D4: When `inbox/tmp/` does not exist for the recipient, `dispatch()`
    /// returns `Err(SendError::Io)`.
    #[test]
    fn dispatch_missing_tmp_dir_returns_io_error() {
        let tmp = secure_dir();
        let base = tmp.path();

        let (agent_registry, identity_registry, _sender_root, recipient_root, sender_id, _) =
            build_test_registry(base);

        // Remove recipient's inbox/tmp/.
        fs::remove_dir(recipient_root.join("inbox").join("tmp")).unwrap();

        let to_name = ValidatedAgentName::new("recipient").unwrap();
        let err = dispatch(
            &agent_registry,
            &identity_registry,
            sender_id,
            &to_name,
            "test body",
        )
        .unwrap_err();

        assert!(
            matches!(err, SendError::Io { .. }),
            "expected SendError::Io when tmp dir is missing, got {err:?}"
        );
    }

    // ── D5: body too large → BodyTooLarge error ───────────────────────────────

    /// D5: A body well over `MAX_ENVELOPE_BYTES` produces a serialized envelope
    /// that also exceeds `MAX_ENVELOPE_BYTES` and is rejected with
    /// `SendError::BodyTooLarge` before any disk write.
    #[test]
    fn dispatch_body_too_large_returns_error() {
        let tmp = secure_dir();
        let base = tmp.path();

        let (agent_registry, identity_registry, _sender_root, _recipient_root, sender_id, _) =
            build_test_registry(base);

        // A body more than 1 MiB over the limit guarantees the serialized
        // envelope exceeds MAX_ENVELOPE_BYTES even after JSON overhead.
        let oversized_body = "x".repeat(MAX_ENVELOPE_BYTES + 1);
        let to_name = ValidatedAgentName::new("recipient").unwrap();

        let err = dispatch(
            &agent_registry,
            &identity_registry,
            sender_id,
            &to_name,
            &oversized_body,
        )
        .unwrap_err();

        assert!(
            matches!(err, SendError::BodyTooLarge { len } if len > MAX_ENVELOPE_BYTES),
            "expected BodyTooLarge with envelope len > MAX_ENVELOPE_BYTES, got {err:?}"
        );
    }

    // ── D8: body at envelope limit → BodyTooLarge ────────────────────────────

    /// D8: A body exactly at `MAX_ENVELOPE_BYTES` is rejected by the fast
    /// pre-check (`body.len() >= MAX_ENVELOPE_BYTES`) before any crypto work.
    /// The returned `len` equals the raw body length, not a serialized
    /// envelope size.
    #[test]
    fn dispatch_body_at_envelope_limit_returns_error() {
        let tmp = secure_dir();
        let base = tmp.path();

        let (agent_registry, identity_registry, _sender_root, _recipient_root, sender_id, _) =
            build_test_registry(base);

        let at_limit_body = "x".repeat(MAX_ENVELOPE_BYTES);
        let to_name = ValidatedAgentName::new("recipient").unwrap();

        let err = dispatch(
            &agent_registry,
            &identity_registry,
            sender_id,
            &to_name,
            &at_limit_body,
        )
        .unwrap_err();

        assert!(
            matches!(err, SendError::BodyTooLarge { len } if len >= MAX_ENVELOPE_BYTES),
            "expected BodyTooLarge with len >= MAX_ENVELOPE_BYTES, got {err:?}"
        );
    }

    // ── D9: identity registry lookup error → IdentityLookupFailed ────────────

    /// D9: When the sender's identity registry file is a symlink, `lookup()`
    /// returns `Err(RegistryError::SymlinkedRegistryFile)`, which `dispatch()`
    /// maps to `Err(SendError::IdentityLookupFailed)`.
    ///
    /// This exercises the error path for `registry.lookup() → Err(...)`,
    /// distinct from `Ok(None)` (`KeyNotFound`) and `Ok(Some(_))` (success).
    #[test]
    fn dispatch_identity_lookup_error_returns_identity_lookup_failed() {
        use std::os::unix::fs::symlink;

        let tmp = secure_dir();
        let base = tmp.path();

        let (agent_registry, identity_registry, _sender_root, _recipient_root, sender_id, _) =
            build_test_registry(base);

        // Replace the sender's registry file with a symlink. The identity
        // registry's `lookup()` rejects symlinked entry files with
        // `Err(RegistryError::SymlinkedRegistryFile)`, which the dispatcher
        // maps to `SendError::IdentityLookupFailed`.
        let registry_file = identity_registry.toml_path(sender_id);
        let outside = secure_dir();
        let target = outside.path().join("evil.toml");
        fs::write(&target, b"identity = {}\n").unwrap();
        fs::remove_file(&registry_file).unwrap();
        symlink(&target, &registry_file).unwrap();

        let to_name = ValidatedAgentName::new("recipient").unwrap();
        let err = dispatch(
            &agent_registry,
            &identity_registry,
            sender_id,
            &to_name,
            "hello",
        )
        .unwrap_err();

        assert!(
            matches!(err, SendError::IdentityLookupFailed { .. }),
            "expected IdentityLookupFailed when registry lookup returns Err, got {err:?}"
        );
    }

    // ── D6: sender not in identity registry → KeyNotFound ────────────────────

    /// D6: When a sender is registered in `AgentRegistry` but has no entry in
    /// `IdentityRegistry`, `dispatch()` returns `Err(SendError::KeyNotFound)`.
    ///
    /// Uses `build_test_registry` for the boilerplate, then adds a third
    /// "orphan-sender" agent that exists only in `AgentRegistry` — the
    /// scenario this test targets.
    #[test]
    fn dispatch_fails_when_sender_not_in_identity_registry() {
        let tmp = secure_dir();
        let base = tmp.path();

        let (mut agent_registry, identity_registry, _sender_root, _recipient_root, _, _) =
            build_test_registry(base);

        // Add a third agent that is in AgentRegistry but NOT IdentityRegistry.
        let orphan_root = base.join("orphan");
        fs::create_dir_all(&orphan_root).unwrap();
        provision_inbox(&orphan_root);
        let orphan_id = IdentityId::new().unwrap();
        let orphan_record = AgentRecord {
            name: ValidatedAgentName::new("orphan-sender").unwrap(),
            identity_id: orphan_id,
            inbox_dir: orphan_root.join("inbox"),
            persona_name: None,
            spawned_at: OffsetDateTime::now_utc(),
            status: AgentStatus::Running,
        };
        agent_registry.register(orphan_record).unwrap();

        let to_name = ValidatedAgentName::new("recipient").unwrap();
        let err = dispatch(
            &agent_registry,
            &identity_registry,
            orphan_id,
            &to_name,
            "hello",
        )
        .unwrap_err();

        assert!(
            matches!(err, SendError::KeyNotFound { identity_id } if identity_id == orphan_id),
            "expected KeyNotFound for sender not in identity registry, got {err:?}"
        );
    }

    // ── D7: corrupt keypair file → KeypairLoad ────────────────────────────────

    /// D7: When `identity.key` contains bytes that are not a valid 32-byte
    /// ed25519 seed, `dispatch()` returns `Err(SendError::KeypairLoad)`.
    #[test]
    fn dispatch_fails_when_keypair_file_is_corrupt() {
        let tmp = secure_dir();
        let base = tmp.path();

        let (agent_registry, identity_registry, sender_root, _recipient_root, sender_id, _) =
            build_test_registry(base);

        // Overwrite the sender's identity.key with bytes that are neither 32
        // bytes in length nor a valid seed — generate_or_load_keypair rejects
        // files whose length != KEY_SEED_LEN.
        let key_path = sender_root.join("identity.key");
        fs::write(&key_path, b"not-a-valid-seed-this-is-garbage-xyz").unwrap();

        let to_name = ValidatedAgentName::new("recipient").unwrap();
        let err = dispatch(
            &agent_registry,
            &identity_registry,
            sender_id,
            &to_name,
            "hello",
        )
        .unwrap_err();

        assert!(
            matches!(err, SendError::KeypairLoad { .. }),
            "expected KeypairLoad when keypair file is corrupt, got {err:?}"
        );
    }

    // ── D10: body just under limit → post-serialization BodyTooLarge ─────────

    /// D10: A body one byte below `MAX_ENVELOPE_BYTES` passes the pre-check
    /// but its serialized envelope exceeds the cap (base64 encoding adds ~33%
    /// overhead). The post-serialization check at `dispatch()` fires and
    /// returns `SendError::BodyTooLarge` with `len` equal to the serialized
    /// envelope length, which exceeds `MAX_ENVELOPE_BYTES`.
    #[test]
    fn dispatch_body_just_under_limit_hits_post_check_returns_error() {
        let tmp = secure_dir();
        let base = tmp.path();

        let (agent_registry, identity_registry, _sender_root, _recipient_root, sender_id, _) =
            build_test_registry(base);

        let near_limit_body = "x".repeat(MAX_ENVELOPE_BYTES - 1);
        let to_name = ValidatedAgentName::new("recipient").unwrap();

        let err = dispatch(
            &agent_registry,
            &identity_registry,
            sender_id,
            &to_name,
            &near_limit_body,
        )
        .unwrap_err();

        assert!(
            matches!(err, SendError::BodyTooLarge { len } if len > MAX_ENVELOPE_BYTES),
            "expected BodyTooLarge with serialized len > MAX_ENVELOPE_BYTES, got {err:?}"
        );
    }

    // ── D_REVOKED: sender key not Active → KeyNotFound ────────────────────────

    /// `D_REVOKED`: When the sender's single key record has state Revoked,
    /// `dispatch()` returns `Err(SendError::KeyNotFound)` — the Active-key
    /// selection at `dispatch()` line ~299 returns None.
    #[test]
    fn dispatch_fails_when_sender_has_no_active_key() {
        let tmp = secure_dir();
        let base = tmp.path();

        // Build base registry
        let (mut agent_registry, identity_registry, _sender_root, _recipient_root, _sender_id, _) =
            build_test_registry(base);

        // Add a "revoked-sender" agent whose identity registry entry has
        // a Revoked (not Active) key state.
        let revoked_root = base.join("revoked-sender");
        fs::create_dir_all(&revoked_root).unwrap();
        provision_inbox(&revoked_root);
        let revoked_id = IdentityId::new().unwrap();
        let revoked_record = AgentRecord {
            name: ValidatedAgentName::new("revoked-sender").unwrap(),
            identity_id: revoked_id,
            inbox_dir: revoked_root.join("inbox"),
            persona_name: None,
            spawned_at: OffsetDateTime::now_utc(),
            status: AgentStatus::Running,
        };
        agent_registry.register(revoked_record).unwrap();

        // Register the revoked-sender in identity registry with a Revoked key.
        let revoked_keypair = generate_or_load_keypair(&revoked_root.join("identity.key")).unwrap();
        let revoked_public = *revoked_keypair.public();
        let operator_id = IdentityId::new().unwrap();
        let revoked_identity =
            Identity::new_agent("revoked-sender".to_owned(), operator_id).unwrap();
        let revoked_identity = {
            let mut id = revoked_identity;
            id.identity_id = revoked_id;
            id
        };
        let mut revoked_key_record = KeyRecord::new(revoked_id, revoked_public).unwrap();
        // Override state to Revoked so there is no Active key for the sender.
        revoked_key_record.state = KeyState::Revoked {
            revoked_at: OffsetDateTime::now_utc(),
        };
        let revoked_stored = StoredIdentity::new(revoked_identity, revoked_key_record).unwrap();
        identity_registry.write(&revoked_stored).unwrap();

        let to_name = ValidatedAgentName::new("recipient").unwrap();
        let err = dispatch(
            &agent_registry,
            &identity_registry,
            revoked_id,
            &to_name,
            "hello",
        )
        .unwrap_err();

        assert!(
            matches!(err, SendError::KeyNotFound { identity_id } if identity_id == revoked_id),
            "expected KeyNotFound when sender has no Active key, got {err:?}"
        );
    }

    // ── D_SYMLINK: symlinked inbox paths → SymlinkRejected ────────────────────

    /// `D_SYMLINK_TMP`: When inbox/tmp is a symlink, `deposit_envelope` returns
    /// `SendError::SymlinkRejected`.
    #[test]
    #[cfg(unix)]
    fn dispatch_inbox_tmp_is_symlink_returns_error() {
        use std::os::unix::fs::symlink;

        let tmp = secure_dir();
        let base = tmp.path();

        let (agent_registry, identity_registry, _sender_root, recipient_root, sender_id, _) =
            build_test_registry(base);

        // Replace inbox/tmp with a symlink to an outside directory.
        let tmp_dir = recipient_root.join("inbox").join("tmp");
        let outside = secure_dir();
        fs::remove_dir(&tmp_dir).unwrap();
        symlink(outside.path(), &tmp_dir).unwrap();

        let to_name = ValidatedAgentName::new("recipient").unwrap();
        let err = dispatch(
            &agent_registry,
            &identity_registry,
            sender_id,
            &to_name,
            "hello",
        )
        .unwrap_err();

        assert!(
            matches!(err, SendError::SymlinkRejected { .. }),
            "expected SymlinkRejected when inbox/tmp is a symlink, got {err:?}"
        );
    }

    /// `D_SYMLINK_ROOT`: When `inbox_dir` itself is a symlink, `deposit_envelope`
    /// returns `SendError::SymlinkRejected` from the `inbox_root` check.
    #[test]
    #[cfg(unix)]
    fn dispatch_inbox_root_is_symlink_returns_error() {
        use std::os::unix::fs::symlink;

        let tmp = secure_dir();
        let base = tmp.path();

        let (agent_registry, identity_registry, _sender_root, recipient_root, sender_id, _) =
            build_test_registry(base);

        // Replace the entire recipient inbox dir with a symlink to an outside directory.
        let inbox_dir = recipient_root.join("inbox");
        fs::remove_dir_all(&inbox_dir).unwrap();
        let outside = secure_dir();
        // Create inbox/tmp and inbox/new in the outside dir so it looks valid.
        fs::create_dir_all(outside.path().join("tmp")).unwrap();
        fs::create_dir_all(outside.path().join("new")).unwrap();
        symlink(outside.path(), &inbox_dir).unwrap();

        let to_name = ValidatedAgentName::new("recipient").unwrap();
        let err = dispatch(
            &agent_registry,
            &identity_registry,
            sender_id,
            &to_name,
            "hello",
        )
        .unwrap_err();

        assert!(
            matches!(err, SendError::SymlinkRejected { .. }),
            "expected SymlinkRejected when inbox_dir is a symlink, got {err:?}"
        );
    }
}
