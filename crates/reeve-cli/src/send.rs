//! Business logic for `reeve send --to <name> --body <text>`.
//!
//! Resolves an agent **name** (e.g. `lead`, `worker-abc12345`) to an
//! `identity_id` via the [`AgentRegistry`], builds a signed envelope
//! using the operator's keychain seed, and deposits the envelope JSON
//! into the recipient's `inbox/new/` via the runtime's atomic
//! [`reeve_runtime::deposit_envelope`] helper — the same helper the
//! in-process agent dispatcher uses, so a CLI-sent message and an
//! agent-sent message reach the watcher through identical filesystem
//! and signing paths.
//!
//! On success, writes `sent: <message_id>` to `out`.
//!
//! Distinct from `reeve envelope sign`: that command dumps a signed
//! envelope to stdout for debugging; `reeve send` actually delivers.

use std::io::Write;

use reeve_runtime::{
    deposit_envelope, AgentRegistry, AgentRegistryError, DepositError, IdentityRegistry,
    OperatorKeyStore,
};

use crate::envelope::{build_signed_envelope, EnvelopeCliError};

/// Errors surfaced by `reeve send`.
#[derive(Debug)]
#[non_exhaustive]
pub(crate) enum SendCliError {
    /// `--to <name>` does not match any record in the agent registry. The
    /// command writes no file when this fires; the inbox is untouched.
    AgentNotFound { name: String },
    /// The agent registry file could not be opened or parsed. Distinct from
    /// `AgentNotFound`: that variant means the registry opened cleanly and
    /// the name is absent; this means the registry itself is unreadable.
    AgentRegistryOpen(AgentRegistryError),
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
            Self::AgentNotFound { name } => {
                write!(f, "agent '{name}' not found in agent registry")
            }
            Self::AgentRegistryOpen(err) => write!(f, "open agent registry: {err}"),
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
            Self::Envelope(err) => Some(err),
            Self::Serialize(err) => Some(err),
            Self::Deposit(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::AgentNotFound { .. } => None,
        }
    }
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

/// Resolve `to_name` in the agent registry at `agent_registry_path`, build
/// and sign an envelope from the operator to that agent with `body` as the
/// payload, and deposit the JSON in the recipient's `inbox/new/`.
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
    reason = "send orchestrates four independent collaborators (two registries, \
              keychain, output sink) plus two value inputs (to_name, body); \
              bundling them into a context struct would trade clarity for \
              indirection at the only non-test call site (cmd_send in main.rs)."
)]
pub(crate) fn send(
    identity_registry: &IdentityRegistry,
    agent_registry_path: &std::path::Path,
    keychain: &dyn OperatorKeyStore,
    to_name: &str,
    body: &[u8],
    out: &mut impl Write,
) -> Result<(), SendCliError> {
    // Re-open the agent registry per call so newly spawned subagents are
    // visible without restart — same pattern the runtime dispatcher uses.
    let agent_registry = AgentRegistry::open(agent_registry_path.to_path_buf())
        .map_err(SendCliError::AgentRegistryOpen)?;

    let record = agent_registry
        .lookup(to_name)
        .ok_or_else(|| SendCliError::AgentNotFound {
            name: to_name.to_owned(),
        })?;

    let envelope = build_signed_envelope(identity_registry, keychain, record.identity_id, body)?;

    let json = serde_json::to_vec(&envelope).map_err(SendCliError::Serialize)?;
    let message_id = envelope.message_id;
    deposit_envelope(&record.inbox_dir, message_id, &json)?;

    writeln!(out, "sent: {message_id}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

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
        identity_dir: std::path::PathBuf,
        agent_registry_path: std::path::PathBuf,
        root: std::path::PathBuf,
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

        TestEnv {
            _dir: dir,
            identity_dir,
            agent_registry_path,
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

    // S2: unknown name surfaces as AgentNotFound with no file written and
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
            &keychain,
            "ghost",
            b"body",
            &mut out,
        )
        .unwrap_err();

        assert!(
            matches!(err, SendCliError::AgentNotFound { ref name } if name == "ghost"),
            "expected AgentNotFound, got {err}"
        );
        assert!(out.is_empty(), "no stdout expected, got {out:?}");
    }

    // S3: a malformed agent registry file surfaces as AgentRegistryOpen
    // (distinct from AgentNotFound, which means "registry parsed fine, name
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
}
