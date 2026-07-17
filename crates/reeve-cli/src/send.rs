//! Business logic for `reeve send --to <name> --body <text>`.
//!
//! Resolves a recipient **name** (e.g. `lead`, `worker-abc12345`, `estate`)
//! to an `identity_id` and inbox, builds a signed envelope using the
//! operator's keychain seed, and deposits the envelope JSON into the
//! recipient's `inbox/new/` via the runtime's atomic
//! [`reeve_runtime::deposit_envelope`] helper — the same helper the
//! in-process agent dispatcher uses, so a CLI-sent message and an
//! agent-sent message reach the watcher through identical filesystem
//! and signing paths.
//!
//! Name resolution checks [`AgentRegistry`] first (the common case), then
//! falls back to [`SystemRegistry`] — non-agent runtime actors like the
//! estate coordinator live there instead, see
//! `reeve_runtime::system_registry`. A name present in neither is
//! `RecipientNotFound`.
//!
//! On success, writes `sent: <message_id>` to `out`.
//!
//! Distinct from `reeve envelope sign`: that command dumps a signed
//! envelope to stdout for debugging; `reeve send` actually delivers.

use std::io::Write;
use std::path::{Path, PathBuf};

use reeve_runtime::{
    deposit_envelope, AgentRegistry, AgentRegistryError, DepositError, IdentityRegistry,
    OperatorKeyStore, SystemRegistry, SystemRegistryError,
};
use reeve_types::IdentityId;

use crate::envelope::{build_signed_envelope, EnvelopeCliError};

/// Errors surfaced by `reeve send`.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum SendCliError {
    /// `--to <name>` does not match any record in the agent registry or the
    /// system registry. The command writes no file when this fires; the
    /// inbox is untouched.
    RecipientNotFound { name: String },
    /// The agent registry file could not be opened or parsed. Distinct from
    /// `RecipientNotFound`: that variant means the registry opened cleanly and
    /// the name is absent; this means the registry itself is unreadable.
    AgentRegistryOpen(AgentRegistryError),
    /// The system registry file could not be opened or parsed.
    SystemRegistryOpen(SystemRegistryError),
    /// Envelope construction or signing failed.
    Envelope(EnvelopeCliError),
    /// Serializing the signed envelope to JSON failed.
    Serialize(serde_json::Error),
    /// Depositing the envelope into the recipient inbox failed (symlink,
    /// IO, etc.).
    Deposit(DepositError),
    /// Writing the success line to the output handle failed.
    Io(std::io::Error),
}

impl std::fmt::Display for SendCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RecipientNotFound { name } => {
                write!(f, "'{name}' not found in the agent or system registry")
            }
            Self::AgentRegistryOpen(err) => write!(f, "open agent registry: {err}"),
            Self::SystemRegistryOpen(err) => write!(f, "open system registry: {err}"),
            Self::Envelope(err) => write!(f, "{err}"),
            Self::Serialize(err) => write!(f, "serialize envelope: {err}"),
            Self::Deposit(err) => write!(f, "deposit envelope: {err}"),
            Self::Io(err) => write!(f, "io: {err}"),
        }
    }
}

impl std::error::Error for SendCliError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::AgentRegistryOpen(err) => Some(err),
            Self::SystemRegistryOpen(err) => Some(err),
            Self::Envelope(err) => Some(err),
            Self::Serialize(err) => Some(err),
            Self::Deposit(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::RecipientNotFound { .. } => None,
        }
    }
}

/// Resolve `to_name` to `(identity_id, inbox_dir)`: agent registry first,
/// system registry second. Re-opens both per call so newly spawned agents
/// or system actors are visible without restart — same pattern the runtime
/// dispatcher uses.
fn resolve_recipient(
    agent_registry_path: &Path,
    system_registry_path: &Path,
    to_name: &str,
) -> Result<(IdentityId, PathBuf), SendCliError> {
    let agent_registry = AgentRegistry::open(agent_registry_path.to_path_buf())
        .map_err(SendCliError::AgentRegistryOpen)?;
    if let Some(record) = agent_registry.lookup(to_name) {
        return Ok((record.identity_id, record.inbox_dir.clone()));
    }

    let system_registry = SystemRegistry::open(system_registry_path.to_path_buf())
        .map_err(SendCliError::SystemRegistryOpen)?;
    if let Some(record) = system_registry.lookup(to_name) {
        return Ok((record.identity_id, record.inbox_dir.clone()));
    }

    Err(SendCliError::RecipientNotFound {
        name: to_name.to_owned(),
    })
}

impl From<EnvelopeCliError> for SendCliError {
    fn from(err: EnvelopeCliError) -> Self {
        Self::Envelope(err)
    }
}

impl From<DepositError> for SendCliError {
    fn from(err: DepositError) -> Self {
        Self::Deposit(err)
    }
}

impl From<std::io::Error> for SendCliError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

/// Resolve `to_name` (agent registry, then system registry, at their
/// respective `*_registry_path`s), build and sign an envelope from the
/// operator to that recipient with `body` as the payload, and deposit the
/// JSON in the recipient's `inbox/new/`.
///
/// Writes `sent: <message_id>` to `out` on success. The "no file written"
/// guarantee holds only for failures *before* the deposit step (name
/// lookup, envelope build/sign): once the envelope has landed in
/// `inbox/new/`, a subsequent `out` write failure (e.g. `BrokenPipe`)
/// propagates as `SendCliError::Io` even though the message has already
/// been delivered to the watcher. Callers that care about the
/// at-most-once distinction can inspect the error variant.
#[expect(
    clippy::too_many_arguments,
    reason = "send orchestrates five independent collaborators (three registries, \
              keychain, output sink) plus two value inputs (to_name, body); \
              bundling them into a context struct would trade clarity for \
              indirection at the only non-test call site (cmd_send in main.rs)."
)]
pub(crate) fn send(
    identity_registry: &IdentityRegistry,
    agent_registry_path: &Path,
    system_registry_path: &Path,
    keychain: &dyn OperatorKeyStore,
    to_name: &str,
    body: &[u8],
    out: &mut impl Write,
) -> Result<(), SendCliError> {
    let (recipient_id, inbox_dir) =
        resolve_recipient(agent_registry_path, system_registry_path, to_name)?;

    let envelope = build_signed_envelope(identity_registry, keychain, recipient_id, body)?;

    let json = serde_json::to_vec(&envelope).map_err(SendCliError::Serialize)?;
    let message_id = envelope.message_id;
    deposit_envelope(&inbox_dir, message_id, &json)?;

    writeln!(out, "sent: {message_id}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use reeve_runtime::keychain::memory::MemoryKeyStore;
    use reeve_runtime::{
        AgentRecord, AgentRegistry, AgentStatus, IdentityRegistry, ValidatedAgentName,
    };
    use reeve_types::{Identity, IdentityId, KeyRecord, Keypair};
    use tempfile::tempdir;
    use time::OffsetDateTime;

    /// Set the test directory to 0o700 on Unix so registry opens (which
    /// enforce the mode) succeed.
    #[cfg(unix)]
    fn secure_dir(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0o700 in tests");
    }

    #[cfg(not(unix))]
    fn secure_dir(_path: &Path) {}

    fn open_identity_registry(dir: &Path) -> IdentityRegistry {
        secure_dir(dir);
        IdentityRegistry::open(dir.to_path_buf()).unwrap()
    }

    /// Create an agent record + paired inbox directory tree under `data_dir`
    /// and write it to the agent registry at `registry_path`. Returns the
    /// record's `identity_id`.
    fn register_agent(registry_path: &Path, data_dir: &Path, name: &str) -> IdentityId {
        let id = IdentityId::new().unwrap();
        let inbox = data_dir.join("agents").join(name).join("inbox");
        for sub in ["tmp", "new"] {
            std::fs::create_dir_all(inbox.join(sub)).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(inbox.join(sub), std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&inbox, std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        let mut registry = AgentRegistry::open(registry_path.to_path_buf()).unwrap();
        registry
            .register(AgentRecord {
                name: ValidatedAgentName::new(name).unwrap(),
                identity_id: id,
                inbox_dir: inbox,
                persona_name: Some(name.to_owned()),
                spawned_at: OffsetDateTime::now_utc(),
                status: AgentStatus::Running,
                stopped_reason: None,
            })
            .unwrap();
        id
    }

    /// Build a test environment laid out like production:
    /// - `<root>/identities/` for the identity registry (so `<uuid>.toml`
    ///   files are isolated from the agent registry TOML).
    /// - `<root>/agents/registry.toml` for the agent registry.
    /// - `<root>/agents/<name>/inbox/{tmp,new}` for the recipient inbox.
    ///
    /// Returns the identity registry path, agent registry path, and root.
    struct TestEnv {
        _dir: tempfile::TempDir,
        identity_dir: PathBuf,
        agent_registry_path: PathBuf,
        system_registry_path: PathBuf,
        root: PathBuf,
    }

    fn make_env() -> TestEnv {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        secure_dir(&root);

        let identity_dir = root.join("identities");
        std::fs::create_dir_all(&identity_dir).unwrap();
        secure_dir(&identity_dir);

        let agents_dir = root.join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        secure_dir(&agents_dir);
        let agent_registry_path = agents_dir.join("registry.toml");
        let system_registry_path = root.join("system").join("registry.toml");

        TestEnv {
            _dir: dir,
            identity_dir,
            agent_registry_path,
            system_registry_path,
            root,
        }
    }

    /// Enroll the operator and register `agent_name` as a recipient agent,
    /// mirroring the identity into both registries (the envelope-sign path
    /// requires the recipient identity in the identity registry too).
    fn enroll_operator_and_register_recipient(
        env: &TestEnv,
        keychain: &MemoryKeyStore,
        agent_name: &str,
    ) -> (IdentityId, IdentityId) {
        let registry = open_identity_registry(&env.identity_dir);
        let stored = crate::identity::enroll(&registry, keychain, "Operator").unwrap();
        let operator_id = stored.identity().identity_id;

        let recipient_id = register_agent(&env.agent_registry_path, &env.root, agent_name);

        let keypair = Keypair::generate();
        let (_, public) = keypair.into_parts();
        let recipient_identity = Identity::new_agent(agent_name.to_owned(), operator_id).unwrap();
        let recipient_identity = Identity {
            identity_id: recipient_id,
            ..recipient_identity
        };
        let key_record = KeyRecord::new(recipient_id, public).unwrap();
        let stored = reeve_runtime::StoredIdentity::new(recipient_identity, key_record).unwrap();
        registry.write(&stored).unwrap();

        (operator_id, recipient_id)
    }

    // S1: happy path — bytes land in the recipient's inbox/new/ and stdout
    // gets `sent: <message_id>`.
    #[test]
    fn send_happy_path_deposits_envelope_and_reports_message_id() {
        let env = make_env();
        let keychain = MemoryKeyStore::new();
        let (_op, recipient_id) =
            enroll_operator_and_register_recipient(&env, &keychain, "worker-abc12345");

        let id_registry = IdentityRegistry::open(env.identity_dir.clone()).unwrap();
        let mut out = Vec::new();
        send(
            &id_registry,
            &env.agent_registry_path,
            &env.system_registry_path,
            &keychain,
            "worker-abc12345",
            b"hello, worker",
            &mut out,
        )
        .unwrap();

        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.starts_with("sent: "), "stdout was {stdout:?}");
        let message_id = stdout.trim_start_matches("sent: ").trim_end();

        let inbox_new = env
            .root
            .join("agents")
            .join("worker-abc12345")
            .join("inbox")
            .join("new");
        let entries: Vec<_> = std::fs::read_dir(&inbox_new).unwrap().collect();
        assert_eq!(entries.len(), 1, "expected one file in inbox/new/");
        let landed = entries.into_iter().next().unwrap().unwrap().path();
        assert_eq!(landed.file_name().unwrap().to_string_lossy(), message_id);

        let bytes = std::fs::read(&landed).unwrap();
        let envelope: reeve_types::Envelope = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(envelope.recipient_id, recipient_id);
        assert_eq!(envelope.body, b"hello, worker");
        assert_eq!(envelope.message_id.to_string(), message_id);
    }

    // S2: unknown name surfaces as RecipientNotFound with no file written and
    // no stdout output.
    #[test]
    fn send_unknown_name_returns_agent_not_found_and_writes_no_file() {
        let env = make_env();
        let keychain = MemoryKeyStore::new();
        let _ = enroll_operator_and_register_recipient(&env, &keychain, "worker-abc12345");

        let id_registry = IdentityRegistry::open(env.identity_dir.clone()).unwrap();
        let mut out = Vec::new();
        let err = send(
            &id_registry,
            &env.agent_registry_path,
            &env.system_registry_path,
            &keychain,
            "ghost",
            b"body",
            &mut out,
        )
        .unwrap_err();

        assert!(
            matches!(err, SendCliError::RecipientNotFound { ref name } if name == "ghost"),
            "expected RecipientNotFound, got {err}"
        );
        assert!(out.is_empty(), "no stdout expected, got {out:?}");
    }

    // S3: a malformed agent registry file surfaces as AgentRegistryOpen
    // (distinct from RecipientNotFound, which means "registry parsed fine, name
    // is absent").
    #[test]
    fn send_unparseable_registry_returns_agent_registry_open() {
        let env = make_env();
        let keychain = MemoryKeyStore::new();
        let id_registry = open_identity_registry(&env.identity_dir);
        let _ = crate::identity::enroll(&id_registry, &keychain, "Operator").unwrap();

        // Write garbage at the registry path so AgentRegistry::open's TOML
        // parse fails.
        std::fs::write(&env.agent_registry_path, b"this is not toml = = =").unwrap();

        let mut out = Vec::new();
        let err = send(
            &id_registry,
            &env.agent_registry_path,
            &env.system_registry_path,
            &keychain,
            "anyone",
            b"body",
            &mut out,
        )
        .unwrap_err();

        assert!(
            matches!(err, SendCliError::AgentRegistryOpen(_)),
            "expected AgentRegistryOpen, got {err}"
        );
    }

    // S4: a name absent from the agent registry but present in the system
    // registry (e.g. `estate`) resolves and delivers — the fallback that
    // fixes the crash where chat-submit against `estate` assumed every
    // registered name was a model-backed agent.
    #[test]
    fn send_resolves_system_actor_when_absent_from_agent_registry() {
        let env = make_env();
        let keychain = MemoryKeyStore::new();
        let id_registry = open_identity_registry(&env.identity_dir);
        let operator_stored = crate::identity::enroll(&id_registry, &keychain, "Operator").unwrap();
        let operator_id = operator_stored.identity().identity_id;

        let system_inbox = env.root.join("system").join("estate").join("inbox");
        for sub in ["tmp", "new"] {
            std::fs::create_dir_all(system_inbox.join(sub)).unwrap();
        }
        // `create_dir_all` leaves the `system/` parent at the platform
        // default mode; `SystemRegistry::open` enforces 0o700 on it.
        secure_dir(&env.root.join("system"));
        let estate_identity_id = IdentityId::new().unwrap();
        let estate_identity = Identity::new_system("estate".to_owned(), operator_id).unwrap();
        let estate_identity = Identity {
            identity_id: estate_identity_id,
            ..estate_identity
        };
        let estate_keypair = Keypair::generate();
        let key_record = KeyRecord::new(estate_identity_id, *estate_keypair.public()).unwrap();
        let stored = reeve_runtime::StoredIdentity::new(estate_identity, key_record).unwrap();
        id_registry.write(&stored).unwrap();

        let mut system_registry = SystemRegistry::open(env.system_registry_path.clone()).unwrap();
        system_registry
            .register(reeve_runtime::SystemActorRecord {
                name: "estate".to_owned(),
                identity_id: estate_identity_id,
                inbox_dir: system_inbox.clone(),
            })
            .unwrap();

        let mut out = Vec::new();
        send(
            &id_registry,
            &env.agent_registry_path,
            &env.system_registry_path,
            &keychain,
            "estate",
            b"open-engagement",
            &mut out,
        )
        .unwrap();

        let stdout = String::from_utf8(out).unwrap();
        assert!(stdout.starts_with("sent: "), "stdout was {stdout:?}");
        let entries: Vec<_> = std::fs::read_dir(system_inbox.join("new"))
            .unwrap()
            .collect();
        assert_eq!(entries.len(), 1, "expected one file in estate's inbox/new/");
    }
}
