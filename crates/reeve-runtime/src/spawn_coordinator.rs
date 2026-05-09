//! Coordinator actor that provisions and starts subordinate agents on request.
//!
//! [`SpawnCoordinator`] handles [`SpawnRequest`] messages. A single request
//! drives the full provisioning sequence: validate the persona config, create
//! the agent directory tree, mint a durable identity, register the agent in
//! both registries, resolve the model adapter, write the spawn snapshot, and
//! start the agent actor under the supervisor tree. The reply arrives on the
//! caller-supplied [`actix::Recipient<SpawnResponse>`] rather than through a
//! synchronous actix request/reply — the same two-message pattern used by
//! tool actors.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use actix::{Actor, Supervised};
use rand_core::{OsRng, RngCore as _};
use reeve_types::{Identity, IdentityId, KeyRecord};
use time::OffsetDateTime;
use tracing::{debug, warn};

use crate::agent::Agent;
use crate::agent_fs::AgentDirs;
use crate::agent_registry::{
    generate_or_load_keypair, AgentRecord, AgentRegistry, AgentRegistryError, AgentStatus,
};
use crate::config::load_persona_config;
use crate::identity_registry::{IdentityRegistry, StoredIdentity};
use crate::inbox::AgentInbox;
use crate::model_resolution::{resolve_model, write_spawn_snapshot};
use crate::supervisor::WatchInbox;
use crate::watcher::Watcher;
use crate::ValidatedAgentName;

// ── Messages ──────────────────────────────────────────────────────────────────

/// Errors returned by [`SpawnRequest::new`] when the caller-supplied fields
/// fail invariant checks before the message is enqueued.
#[derive(Debug)]
pub enum SpawnRequestError {
    /// `persona_name` did not pass [`ValidatedAgentName`] validation.
    InvalidPersonaName(AgentRegistryError),
    /// `display_name` was empty or whitespace-only.
    EmptyDisplayName,
    /// `display_name` exceeded the maximum allowed byte length.
    DisplayNameTooLong {
        /// Maximum allowed byte length.
        max: usize,
        /// Actual byte length of the rejected value.
        actual: usize,
    },
    /// `display_name` contained a control character.
    InvalidDisplayName {
        /// Human-readable description of the violation.
        reason: &'static str,
    },
}

impl fmt::Display for SpawnRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPersonaName(err) => write!(f, "invalid persona name: {err}"),
            Self::EmptyDisplayName => f.write_str("display_name must not be empty"),
            Self::DisplayNameTooLong { max, actual } => {
                write!(
                    f,
                    "display_name exceeds maximum length of {max} bytes (actual: {actual})"
                )
            }
            Self::InvalidDisplayName { reason } => {
                write!(f, "display_name is invalid: {reason}")
            }
        }
    }
}

impl std::error::Error for SpawnRequestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPersonaName(err) => Some(err),
            Self::EmptyDisplayName
            | Self::DisplayNameTooLong { .. }
            | Self::InvalidDisplayName { .. } => None,
        }
    }
}

/// Maximum byte length accepted for `display_name`.
const MAX_DISPLAY_NAME_LEN: usize = 256;

/// Directory name under `data_dir` where persona configs are stored.
const PERSONAS_DIR: &str = "personas";

/// Request to provision and start a new subordinate agent.
///
/// All fields are private; [`SpawnRequest::new`] is the only construction path.
/// Invariants — non-empty `persona_name`, non-empty/not-too-long `display_name`
/// — are checked once at the message boundary so the handler never re-validates.
/// The outcome arrives on `reply_to` as a [`SpawnResponse`].
pub struct SpawnRequest {
    persona_name: ValidatedAgentName,
    display_name: String,
    system_prompt: String,
    sender_id: IdentityId,
    reply_to: actix::Recipient<SpawnResponse>,
}

impl SpawnRequest {
    /// Construct a [`SpawnRequest`], validating `persona_name` and
    /// `display_name` at the message boundary.
    ///
    /// Returns [`SpawnRequestError`] when either field fails its invariant.
    pub fn new(
        persona_name: &str,
        display_name: &str,
        system_prompt: &str,
        sender_id: IdentityId,
        reply_to: actix::Recipient<SpawnResponse>,
    ) -> Result<Self, SpawnRequestError> {
        let persona_name =
            ValidatedAgentName::new(persona_name).map_err(SpawnRequestError::InvalidPersonaName)?;
        let display_name = display_name.trim().to_owned();
        if display_name.is_empty() {
            return Err(SpawnRequestError::EmptyDisplayName);
        }
        if display_name.len() > MAX_DISPLAY_NAME_LEN {
            return Err(SpawnRequestError::DisplayNameTooLong {
                max: MAX_DISPLAY_NAME_LEN,
                actual: display_name.len(),
            });
        }
        if display_name.chars().any(char::is_control) {
            return Err(SpawnRequestError::InvalidDisplayName {
                reason: "display_name must not contain control characters",
            });
        }
        let system_prompt = system_prompt.trim().to_owned();
        Ok(Self {
            persona_name,
            display_name,
            system_prompt,
            sender_id,
            reply_to,
        })
    }

    pub fn persona_name(&self) -> &ValidatedAgentName {
        &self.persona_name
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    pub fn sender_id(&self) -> IdentityId {
        self.sender_id
    }

    pub fn reply_to(&self) -> &actix::Recipient<SpawnResponse> {
        &self.reply_to
    }
}

impl actix::Message for SpawnRequest {
    type Result = ();
}

/// Outcome of a [`SpawnRequest`].
#[derive(Debug)]
pub enum SpawnResponse {
    /// The spawn sequence completed successfully.
    Success {
        /// Role-name assigned to the new agent (e.g., `"analyst-01943abc"`).
        agent_name: String,
        /// Registered identity ID of the new agent.
        agent_id: IdentityId,
    },
    /// The spawn sequence failed before the agent was started.
    Failure {
        /// Human-readable reason for the failure.
        message: String,
    },
}

impl actix::Message for SpawnResponse {
    type Result = ();
}

// ── SpawnCoordinator ──────────────────────────────────────────────────────────

/// Supervised actor that provisions and starts subordinate agents.
///
/// One coordinator per daemon instance; the lead agent holds a reference to it
/// (wired in the next task). All spawn requests pass through here so that
/// identity registration, directory provisioning, and actor startup happen in a
/// single, auditable sequence.
pub struct SpawnCoordinator {
    /// Root of the Reeve data directory (`~/.local/share/reeve` by default).
    data_dir: PathBuf,
    /// Path to the agent registry TOML file.
    agent_registry_path: PathBuf,
    /// Shared identity registry for writing new agent identities.
    identity_registry: Arc<IdentityRegistry>,
    /// Model adapter used by all spawned agents.
    adapter: Arc<dyn reeve_adapter::Adapter>,
    /// Watcher that owns the routing table; `register_route` is called after
    /// the agent actor starts.
    watcher: Arc<Watcher>,
    /// Recipient that receives [`WatchInbox`] messages to start inbox watching
    /// for each new agent. Typically a [`crate::supervisor::WatcherActor`]
    /// recipient; replaceable in tests with a no-op stub.
    inbox_starter: actix::Recipient<WatchInbox>,
}

impl SpawnCoordinator {
    /// Construct a coordinator.
    #[expect(
        clippy::too_many_arguments,
        reason = "coordinator wires six independent runtime collaborators; \
                  bundling into a config struct trades clarity for indirection"
    )]
    pub fn new(
        data_dir: PathBuf,
        agent_registry_path: PathBuf,
        identity_registry: Arc<IdentityRegistry>,
        adapter: Arc<dyn reeve_adapter::Adapter>,
        watcher: Arc<Watcher>,
        inbox_starter: actix::Recipient<WatchInbox>,
    ) -> Self {
        Self {
            data_dir,
            agent_registry_path,
            identity_registry,
            adapter,
            watcher,
            inbox_starter,
        }
    }
}

impl Actor for SpawnCoordinator {
    type Context = actix::Context<Self>;
}

impl Supervised for SpawnCoordinator {}

// ── SpawnRequest handler ──────────────────────────────────────────────────────

impl actix::Handler<SpawnRequest> for SpawnCoordinator {
    type Result = ();

    /// Execute the full spawn sequence and reply with a [`SpawnResponse`].
    ///
    /// The sequence is linear and fails fast: each step that errors sends an
    /// error reply and returns. Directories provisioned before a later failure
    /// are left in place — `AgentDirs::provision` is idempotent and the agent
    /// registry will not contain a record for the partial provisioning, so a
    /// subsequent spawn with a fresh name is safe.
    #[expect(
        clippy::too_many_lines,
        reason = "splitting would obscure the linear dependency chain across the spawn steps"
    )]
    fn handle(&mut self, msg: SpawnRequest, _ctx: &mut actix::Context<Self>) {
        let SpawnRequest {
            persona_name,
            display_name,
            system_prompt,
            sender_id,
            reply_to,
        } = msg;
        let persona_name_str = persona_name.as_str();

        let mut suffix_bytes = [0u8; 4];
        OsRng.fill_bytes(&mut suffix_bytes);
        let suffix = suffix_bytes
            .iter()
            .fold(String::with_capacity(8), |mut acc, b| {
                use std::fmt::Write as _;
                let _ = write!(acc, "{b:02x}");
                acc
            });
        let agent_name_str = format!("{persona_name_str}-{suffix}");
        let validated_name = match ValidatedAgentName::new(&agent_name_str) {
            Ok(n) => n,
            Err(err) => {
                error_reply(
                    &reply_to,
                    format!("derived agent name '{agent_name_str}' is invalid: {err}"),
                );
                return;
            }
        };

        let persona_path = self
            .data_dir
            .join(PERSONAS_DIR)
            .join(persona_name_str)
            .join("config.toml");

        let persona_config = match load_persona_config(&persona_path) {
            Ok(cfg) => cfg,
            Err(crate::config::ConfigError::Io { ref source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                error_reply(
                    &reply_to,
                    format!("persona not found: '{persona_name_str}'"),
                );
                return;
            }
            Err(err) => {
                warn!(%err, "failed to load persona config");
                error_reply(&reply_to, String::from("failed to load persona config"));
                return;
            }
        };

        let dirs = match AgentDirs::provision(&self.data_dir, &agent_name_str) {
            Ok(d) => d,
            Err(err) => {
                error_reply(
                    &reply_to,
                    format!("failed to provision agent directories: {err}"),
                );
                return;
            }
        };

        let keypair = match generate_or_load_keypair(&dirs.identity_key_path()) {
            Ok(kp) => kp,
            Err(err) => {
                error_reply(
                    &reply_to,
                    format!("failed to generate agent keypair: {err}"),
                );
                return;
            }
        };

        let identity = match Identity::new_agent(display_name, sender_id) {
            Ok(id) => id,
            Err(err) => {
                error_reply(&reply_to, format!("failed to create agent identity: {err}"));
                return;
            }
        };
        let agent_id = identity.identity_id;

        let key_record = match KeyRecord::new(agent_id, *keypair.public()) {
            Ok(kr) => kr,
            Err(err) => {
                error_reply(&reply_to, format!("failed to create key record: {err}"));
                return;
            }
        };
        let stored = match StoredIdentity::new(identity, key_record) {
            Ok(s) => s,
            Err(err) => {
                error_reply(&reply_to, format!("failed to build stored identity: {err}"));
                return;
            }
        };
        if let Err(err) = self.identity_registry.write(&stored) {
            error_reply(
                &reply_to,
                format!("failed to write identity to registry: {err}"),
            );
            return;
        }

        let record = AgentRecord {
            name: validated_name.clone(),
            identity_id: agent_id,
            inbox_dir: dirs.inbox_root(),
            persona_name: Some(persona_name.as_str().to_owned()),
            spawned_at: OffsetDateTime::now_utc(),
            status: AgentStatus::Running,
        };
        if let Err(err) = AgentRegistry::open(self.agent_registry_path.clone())
            .and_then(|mut reg| reg.register(record))
        {
            error_reply(&reply_to, format!("failed to register agent: {err}"));
            return;
        }

        let snapshot = match resolve_model(&persona_config, &[self.adapter.as_ref()], agent_id) {
            Ok(s) => s,
            Err(err) => {
                error_reply(&reply_to, format!("failed to resolve model adapter: {err}"));
                return;
            }
        };

        if let Err(err) = write_spawn_snapshot(&dirs, &snapshot) {
            error_reply(&reply_to, format!("failed to write spawn snapshot: {err}"));
            return;
        }

        debug!(
            agent_name = %validated_name,
            %agent_id,
            adapter_id = %snapshot.adapter_id,
            "spawn sequence complete; starting agent actor",
        );

        let final_system_prompt = if system_prompt.is_empty() {
            persona_config.system_prompt.clone()
        } else {
            format!("{}\n\n{}", persona_config.system_prompt, system_prompt)
        };

        let new_agent = match Agent::new(
            Arc::clone(&self.adapter),
            &dirs,
            snapshot,
            final_system_prompt,
            agent_id,
            keypair,
            vec![],
        ) {
            Ok(a) => a,
            Err(err) => {
                error_reply(&reply_to, format!("failed to construct agent: {err}"));
                return;
            }
        };

        let agent_addr = actix::Supervisor::start(move |_| new_agent);

        self.watcher
            .register_route(agent_id, agent_addr.clone().recipient());

        let inbox = AgentInbox::from_path(dirs.inbox_root());
        self.inbox_starter.do_send(WatchInbox {
            agent_id,
            inbox,
            on_quarantine: None,
            recipient: agent_addr.recipient(),
        });

        let response = SpawnResponse::Success {
            agent_name: validated_name.to_string(),
            agent_id,
        };
        if let Err(err) = reply_to.try_send(response) {
            warn!(err = %err, %agent_id, "failed to deliver SpawnResponse to caller");
        }
    }
}

fn error_reply(reply_to: &actix::Recipient<SpawnResponse>, message: String) {
    let response = SpawnResponse::Failure { message };
    if let Err(send_err) = reply_to.try_send(response) {
        warn!(err = %send_err, "failed to deliver error SpawnResponse to caller");
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use actix::Actor;
    use tempfile::TempDir;
    use tokio::sync::oneshot;

    use super::{SpawnCoordinator, SpawnRequest, SpawnRequestError, SpawnResponse};
    use crate::agent_registry::{AgentRegistry, AgentStatus};
    use crate::identity_registry::IdentityRegistry;
    use crate::supervisor::WatchInbox;
    use crate::test_support::{build_registries, secure_dir, MockAdapter};
    use crate::watcher::Watcher;
    use reeve_types::IdentityId;

    // ── Stub actors ───────────────────────────────────────────────────────────

    /// No-op inbox-starter stub that prevents the tokio runtime from blocking on shutdown.
    struct NullInboxStarter;

    impl Actor for NullInboxStarter {
        type Context = actix::Context<Self>;
    }

    impl actix::Handler<WatchInbox> for NullInboxStarter {
        type Result = ();

        fn handle(&mut self, _msg: WatchInbox, _ctx: &mut actix::Context<Self>) {}
    }

    // ── Fixture helpers ───────────────────────────────────────────────────────

    fn write_minimal_persona(data_dir: &std::path::Path, persona_name: &str) {
        let persona_dir = data_dir.join("personas").join(persona_name);
        std::fs::create_dir_all(&persona_dir).unwrap();
        let config = format!(
            "name = \"{persona_name}\"\nsystem_prompt = \"Be helpful.\"\nmodel_preferences = [\"claude-opus-4-7\"]\n"
        );
        std::fs::write(persona_dir.join("config.toml"), config).unwrap();
    }

    fn build_coordinator(
        data_dir: &std::path::Path,
        identity_registry: Arc<IdentityRegistry>,
        watcher: Arc<Watcher>,
        agent_registry_path: std::path::PathBuf,
        inbox_starter: actix::Recipient<WatchInbox>,
    ) -> SpawnCoordinator {
        let adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(MockAdapter::new("claude-opus-4-7@anthropic-direct"));
        SpawnCoordinator::new(
            data_dir.to_path_buf(),
            agent_registry_path,
            identity_registry,
            adapter,
            watcher,
            inbox_starter,
        )
    }

    /// Collect a single `SpawnResponse` from the channel with a 5-second timeout.
    ///
    /// Panics if the response does not arrive in time.
    async fn collect_response(rx: oneshot::Receiver<SpawnResponse>) -> SpawnResponse {
        const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
        tokio::time::timeout(RESPONSE_TIMEOUT, rx)
            .await
            .expect("SpawnResponse did not arrive within 5 seconds")
            .expect("SpawnResponse sender dropped without sending")
    }

    struct ResponseCapture {
        tx: Option<oneshot::Sender<SpawnResponse>>,
    }

    impl Actor for ResponseCapture {
        type Context = actix::Context<Self>;
    }

    impl actix::Handler<SpawnResponse> for ResponseCapture {
        type Result = ();

        fn handle(&mut self, msg: SpawnResponse, _ctx: &mut actix::Context<Self>) {
            if let Some(tx) = self.tx.take() {
                let _ = tx.send(msg);
            }
        }
    }

    // ── SC1: persona not found ────────────────────────────────────────────────

    /// No agent directories should be created under `data_dir/agents/`.
    #[test]
    fn persona_not_found_returns_error() {
        let tmp: TempDir = secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let sender_id = IdentityId::new().unwrap();

        let response: Arc<Mutex<Option<SpawnResponse>>> = Arc::new(Mutex::new(None));
        let response_outer = Arc::clone(&response);

        actix::System::new().block_on(async move {
            let inbox_starter = NullInboxStarter.start().recipient();

            let (tx, rx) = oneshot::channel();
            let capture_addr = ResponseCapture { tx: Some(tx) }.start();

            let coordinator = build_coordinator(
                &data_dir,
                identity_registry,
                watcher,
                agent_registry_path,
                inbox_starter,
            );
            let coord_addr = coordinator.start();

            coord_addr.do_send(
                SpawnRequest::new(
                    "nonexistent",
                    "Test Agent",
                    "You are a test agent.",
                    sender_id,
                    capture_addr.recipient(),
                )
                .unwrap(),
            );

            let resp = collect_response(rx).await;
            *response_outer.lock().unwrap() = Some(resp);
            actix::System::current().stop();
        });

        let guard = response.lock().unwrap();
        let resp = guard.as_ref().expect("response must have arrived");
        let SpawnResponse::Failure { message } = resp else {
            panic!("expected SpawnResponse::Failure for missing persona");
        };
        let msg_lower = message.to_lowercase();
        assert!(
            msg_lower.contains("persona") || msg_lower.contains("not found"),
            "message should mention 'persona' or 'not found', got: '{message}'",
        );

        let agents_dir = tmp.path().join("agents");
        assert!(
            !agents_dir.exists(),
            "no agent directory should be created for a failed persona lookup",
        );
    }

    // ── SC2: success path ─────────────────────────────────────────────────────

    #[test]
    fn success_path_registers_agent_in_both_registries() {
        let tmp: TempDir = secure_dir();
        let data_dir = tmp.path().to_path_buf();
        write_minimal_persona(&data_dir, "test");

        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let identity_registry_outer = Arc::clone(&identity_registry);
        let agent_registry_path_outer = agent_registry_path.clone();
        let watcher_outer = Arc::clone(&watcher);
        let sender_id = IdentityId::new().unwrap();

        let response: Arc<Mutex<Option<SpawnResponse>>> = Arc::new(Mutex::new(None));
        let response_outer = Arc::clone(&response);

        actix::System::new().block_on(async move {
            let inbox_starter = NullInboxStarter.start().recipient();

            let (tx, rx) = oneshot::channel();
            let capture_addr = ResponseCapture { tx: Some(tx) }.start();

            let coordinator = build_coordinator(
                &data_dir,
                identity_registry,
                watcher,
                agent_registry_path,
                inbox_starter,
            );
            let coord_addr = coordinator.start();

            coord_addr.do_send(
                SpawnRequest::new(
                    "test",
                    "Test Agent",
                    "You are a test agent.",
                    sender_id,
                    capture_addr.recipient(),
                )
                .unwrap(),
            );

            let resp = collect_response(rx).await;
            *response_outer.lock().unwrap() = Some(resp);
            actix::System::current().stop();
        });

        let guard = response.lock().unwrap();
        let resp = guard.as_ref().expect("response must have arrived");
        let SpawnResponse::Success {
            agent_name,
            agent_id,
        } = resp
        else {
            panic!("expected SpawnResponse::Success");
        };

        assert!(
            !agent_name.is_empty(),
            "agent_name must be non-empty on success"
        );

        // Agent appears in the agent registry.
        let agent_registry =
            AgentRegistry::open(agent_registry_path_outer).expect("open agent registry");
        let record = agent_registry
            .lookup(agent_name.as_str())
            .expect("agent record must be present in registry");
        assert_eq!(record.identity_id, *agent_id);
        assert_eq!(record.status, AgentStatus::Running);
        assert_eq!(record.persona_name.as_deref(), Some("test"));

        // Identity appears in the identity registry.
        let stored = identity_registry_outer
            .lookup(*agent_id)
            .expect("identity registry lookup must not fail")
            .expect("identity must be registered");
        assert_eq!(stored.identity().identity_id, *agent_id);

        // Routing is registered in the watcher.
        assert!(
            watcher_outer.has_route(*agent_id),
            "spawned agent must be routable",
        );
    }

    // ── SC3: two spawns with same persona produce distinct names ──────────────

    #[test]
    fn two_spawns_same_persona_produce_distinct_names() {
        let tmp: TempDir = secure_dir();
        let data_dir = tmp.path().to_path_buf();
        write_minimal_persona(&data_dir, "test");

        let sender_id = IdentityId::new().unwrap();
        let responses: Arc<Mutex<Vec<SpawnResponse>>> = Arc::new(Mutex::new(Vec::new()));
        let responses_outer = Arc::clone(&responses);

        actix::System::new().block_on(async move {
            let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
            let inbox_starter = NullInboxStarter.start().recipient();

            let (tx1, rx1) = oneshot::channel();
            let (tx2, rx2) = oneshot::channel();
            let capture1 = ResponseCapture { tx: Some(tx1) }.start();
            let capture2 = ResponseCapture { tx: Some(tx2) }.start();

            let coordinator = build_coordinator(
                &data_dir,
                identity_registry,
                watcher,
                agent_registry_path,
                inbox_starter,
            );
            let coord_addr = coordinator.start();

            coord_addr.do_send(
                SpawnRequest::new(
                    "test",
                    "Agent One",
                    "First agent.",
                    sender_id,
                    capture1.recipient(),
                )
                .unwrap(),
            );
            coord_addr.do_send(
                SpawnRequest::new(
                    "test",
                    "Agent Two",
                    "Second agent.",
                    sender_id,
                    capture2.recipient(),
                )
                .unwrap(),
            );

            let r1 = collect_response(rx1).await;
            let r2 = collect_response(rx2).await;
            responses_outer.lock().unwrap().push(r1);
            responses_outer.lock().unwrap().push(r2);

            actix::System::current().stop();
        });

        let guard = responses.lock().unwrap();
        assert_eq!(guard.len(), 2);

        let (name1, id1) = match &guard[0] {
            SpawnResponse::Success {
                agent_name,
                agent_id,
            } => (agent_name.clone(), *agent_id),
            SpawnResponse::Failure { message } => panic!("first spawn failed: '{message}'"),
        };
        let (name2, id2) = match &guard[1] {
            SpawnResponse::Success {
                agent_name,
                agent_id,
            } => (agent_name.clone(), *agent_id),
            SpawnResponse::Failure { message } => panic!("second spawn failed: '{message}'"),
        };

        assert_ne!(
            name1, name2,
            "two spawns of the same persona must produce distinct agent names",
        );
        assert_ne!(
            id1, id2,
            "two spawns of the same persona must produce distinct identity IDs",
        );
    }

    // ── SC6: invalid persona_name returns failure ─────────────────────────────

    #[test]
    fn invalid_persona_name_returns_failure() {
        // Test path-traversal attempt and empty string — both are rejected by
        // ValidatedAgentName at construction time.
        let invalid_names: &[&str] = &["../escape", "", "name/with/slash", ".."];
        for &bad_name in invalid_names {
            let dummy_id = IdentityId::new().unwrap();
            let result = actix::System::new().block_on(async {
                let (tx, _rx) = oneshot::channel::<SpawnResponse>();
                let capture = ResponseCapture { tx: Some(tx) }.start();
                SpawnRequest::new(
                    bad_name,
                    "Test Agent",
                    "irrelevant",
                    dummy_id,
                    capture.recipient(),
                )
            });
            assert!(
                matches!(result, Err(SpawnRequestError::InvalidPersonaName(_))),
                "expected Err(InvalidPersonaName) for invalid persona_name '{bad_name}'",
            );
        }
    }

    // ── SC5: registry open failure leaves one orphan identity ────────────────

    #[test]
    fn registry_open_failure_leaves_one_orphan_identity() {
        let tmp: TempDir = secure_dir();
        let data_dir = tmp.path().to_path_buf();
        write_minimal_persona(&data_dir, "test");

        let (identity_registry, watcher, _real_registry_path) = build_registries(&data_dir);
        let identity_registry_for_check = Arc::clone(&identity_registry);
        let sender_id = IdentityId::new().unwrap();

        // AgentRegistry::open fails because its parent directory is a regular file;
        // identity written before this step is a known orphan.
        let blocked_parent = data_dir.join("blocked-reg");
        std::fs::write(&blocked_parent, b"not a directory").unwrap();
        let blocked_registry_path = blocked_parent.join("registry.toml");

        let identities_before = identity_registry_for_check
            .list()
            .expect("list before spawn must succeed")
            .len();

        let response: Arc<Mutex<Option<SpawnResponse>>> = Arc::new(Mutex::new(None));
        let response_outer = Arc::clone(&response);

        actix::System::new().block_on(async move {
            let inbox_starter = NullInboxStarter.start().recipient();

            let (tx, rx) = oneshot::channel();
            let capture_addr = ResponseCapture { tx: Some(tx) }.start();

            let coordinator = build_coordinator(
                &data_dir,
                identity_registry,
                watcher,
                blocked_registry_path,
                inbox_starter,
            );
            let coord_addr = coordinator.start();

            coord_addr.do_send(
                SpawnRequest::new(
                    "test",
                    "Orphan Test",
                    "irrelevant",
                    sender_id,
                    capture_addr.recipient(),
                )
                .unwrap(),
            );

            let resp = collect_response(rx).await;
            *response_outer.lock().unwrap() = Some(resp);
            actix::System::current().stop();
        });

        let guard = response.lock().unwrap();
        let resp = guard.as_ref().expect("response must have arrived");
        assert!(
            matches!(resp, SpawnResponse::Failure { .. }),
            "expected SpawnResponse::Failure when agent registry is blocked",
        );

        // One orphan identity is written before AgentRegistry::open fails — documented in the handler.
        let identities_after = identity_registry_for_check
            .list()
            .expect("list after spawn must succeed")
            .len();
        assert_eq!(
            identities_after,
            identities_before + 1,
            "one orphan identity is expected: written before registry open fails",
        );
    }

    // ── SC10: provision failure returns SpawnResponse::Failure ───────────────

    #[test]
    fn provision_failure_returns_error() {
        let tmp: TempDir = secure_dir();
        let data_dir = tmp.path().to_path_buf();
        write_minimal_persona(&data_dir, "test");

        // Block the agents directory by placing a regular file at that path;
        // AgentDirs::provision cannot create subdirectories inside a file.
        let agents_path = data_dir.join("agents");
        std::fs::write(&agents_path, b"not a directory").unwrap();

        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let sender_id = IdentityId::new().unwrap();

        let response: Arc<Mutex<Option<SpawnResponse>>> = Arc::new(Mutex::new(None));
        let response_outer = Arc::clone(&response);

        actix::System::new().block_on(async move {
            let inbox_starter = NullInboxStarter.start().recipient();

            let (tx, rx) = oneshot::channel();
            let capture_addr = ResponseCapture { tx: Some(tx) }.start();

            let coordinator = build_coordinator(
                &data_dir,
                identity_registry,
                watcher,
                agent_registry_path,
                inbox_starter,
            );
            let coord_addr = coordinator.start();

            coord_addr.do_send(
                SpawnRequest::new(
                    "test",
                    "Test Agent",
                    "irrelevant",
                    sender_id,
                    capture_addr.recipient(),
                )
                .unwrap(),
            );

            let resp = collect_response(rx).await;
            *response_outer.lock().unwrap() = Some(resp);
            actix::System::current().stop();
        });

        let guard = response.lock().unwrap();
        assert!(
            matches!(guard.as_ref().unwrap(), SpawnResponse::Failure { .. }),
            "expected Failure when agent directory provision fails",
        );
    }

    // ── SC7: whitespace-only display_name returns failure ─────────────────────

    #[test]
    fn empty_display_name_returns_failure() {
        let dummy_id = IdentityId::new().unwrap();
        let result = actix::System::new().block_on(async {
            let (tx, _rx) = oneshot::channel::<SpawnResponse>();
            let capture = ResponseCapture { tx: Some(tx) }.start();
            SpawnRequest::new(
                "valid-persona",
                "   ",
                "irrelevant",
                dummy_id,
                capture.recipient(),
            )
        });
        assert!(
            matches!(result, Err(SpawnRequestError::EmptyDisplayName)),
            "expected Err(EmptyDisplayName) for whitespace-only display_name",
        );
    }

    // ── SC7b: whitespace-only system_prompt is normalized to empty string ────────

    #[test]
    fn whitespace_only_system_prompt_is_normalized() {
        let dummy_id = IdentityId::new().unwrap();
        let result = actix::System::new().block_on(async {
            let (tx, _rx) = oneshot::channel::<SpawnResponse>();
            let capture = ResponseCapture { tx: Some(tx) }.start();
            SpawnRequest::new(
                "valid-persona",
                "Test Agent",
                "   ",
                dummy_id,
                capture.recipient(),
            )
        });
        let req = result.expect("whitespace-only system_prompt must not fail validation");
        assert_eq!(
            req.system_prompt(),
            "",
            "whitespace-only system_prompt must be normalized to empty string",
        );
    }

    // ── SC8: display_name byte-length boundary ────────────────────────────────

    #[test]
    fn display_name_at_max_len_is_accepted() {
        let dummy_id = IdentityId::new().unwrap();
        let name_256 = "a".repeat(256);
        assert_eq!(name_256.len(), 256);
        let result = actix::System::new().block_on(async {
            let (tx, _rx) = oneshot::channel::<SpawnResponse>();
            let capture = ResponseCapture { tx: Some(tx) }.start();
            SpawnRequest::new(
                "valid-persona",
                &name_256,
                "irrelevant",
                dummy_id,
                capture.recipient(),
            )
        });
        assert!(
            result.is_ok(),
            "display_name of exactly 256 bytes must be accepted",
        );
    }

    #[test]
    fn display_name_one_over_max_len_is_rejected() {
        let dummy_id = IdentityId::new().unwrap();
        let name_257 = "a".repeat(257);
        assert_eq!(name_257.len(), 257);
        let result = actix::System::new().block_on(async {
            let (tx, _rx) = oneshot::channel::<SpawnResponse>();
            let capture = ResponseCapture { tx: Some(tx) }.start();
            SpawnRequest::new(
                "valid-persona",
                &name_257,
                "irrelevant",
                dummy_id,
                capture.recipient(),
            )
        });
        assert!(
            matches!(
                result,
                Err(SpawnRequestError::DisplayNameTooLong {
                    max: 256,
                    actual: 257
                })
            ),
            "display_name of 257 bytes must be rejected with DisplayNameTooLong {{ max: 256, actual: 257 }}",
        );
    }

    #[test]
    fn display_name_multibyte_boundary() {
        // "é" is 2 bytes in UTF-8.
        // 254 ASCII bytes + "é" (2 bytes) = 256 bytes total — should pass.
        let name_256_multibyte = "a".repeat(254) + "é";
        assert_eq!(name_256_multibyte.len(), 256, "fixture must be 256 bytes");

        // 255 ASCII bytes + "é" (2 bytes) = 257 bytes total — should fail.
        let name_257_multibyte = "a".repeat(255) + "é";
        assert_eq!(name_257_multibyte.len(), 257, "fixture must be 257 bytes");

        let dummy_id = IdentityId::new().unwrap();
        let ok = actix::System::new().block_on(async {
            let (tx, _rx) = oneshot::channel::<SpawnResponse>();
            let capture = ResponseCapture { tx: Some(tx) }.start();
            SpawnRequest::new(
                "valid-persona",
                &name_256_multibyte,
                "irrelevant",
                dummy_id,
                capture.recipient(),
            )
        });
        assert!(
            ok.is_ok(),
            "256-byte multibyte display_name must be accepted"
        );

        let dummy_id2 = IdentityId::new().unwrap();
        let err = actix::System::new().block_on(async {
            let (tx, _rx) = oneshot::channel::<SpawnResponse>();
            let capture = ResponseCapture { tx: Some(tx) }.start();
            SpawnRequest::new(
                "valid-persona",
                &name_257_multibyte,
                "irrelevant",
                dummy_id2,
                capture.recipient(),
            )
        });
        assert!(
            matches!(
                err,
                Err(SpawnRequestError::DisplayNameTooLong {
                    max: 256,
                    actual: 257
                })
            ),
            "257-byte multibyte display_name must be rejected",
        );
    }

    // ── SC9: resolve_model failure leaves double-orphan ───────────────────────

    /// When the persona's model preferences do not match any registered adapter,
    /// `resolve_model` fails after identity and agent registry records have
    /// already been written. Both registries retain their entries (double-orphan)
    /// even though the spawn ultimately fails.
    #[test]
    fn resolve_model_failure_leaves_double_orphan() {
        let tmp: TempDir = secure_dir();
        let data_dir = tmp.path().to_path_buf();

        // Write a persona that requests a model the mock adapter cannot serve.
        let persona_dir = data_dir.join("personas").join("unmatchable");
        std::fs::create_dir_all(&persona_dir).unwrap();
        std::fs::write(
            persona_dir.join("config.toml"),
            "name = \"unmatchable\"\nsystem_prompt = \"Be helpful.\"\nmodel_preferences = [\"gpt-4\"]\n",
        )
        .unwrap();

        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let identity_registry_for_check = Arc::clone(&identity_registry);
        let agent_registry_path_for_check = agent_registry_path.clone();
        let sender_id = IdentityId::new().unwrap();

        let identities_before = identity_registry_for_check
            .list()
            .expect("list before spawn must succeed")
            .len();

        let agents_before = AgentRegistry::open(agent_registry_path.clone())
            .expect("open agent registry before spawn")
            .list()
            .count();

        let response: Arc<Mutex<Option<SpawnResponse>>> = Arc::new(Mutex::new(None));
        let response_outer = Arc::clone(&response);

        actix::System::new().block_on(async move {
            let inbox_starter = NullInboxStarter.start().recipient();

            let (tx, rx) = oneshot::channel();
            let capture_addr = ResponseCapture { tx: Some(tx) }.start();

            let coordinator = build_coordinator(
                &data_dir,
                identity_registry,
                watcher,
                agent_registry_path,
                inbox_starter,
            );
            let coord_addr = coordinator.start();

            coord_addr.do_send(
                SpawnRequest::new(
                    "unmatchable",
                    "Unmatchable Agent",
                    "irrelevant",
                    sender_id,
                    capture_addr.recipient(),
                )
                .unwrap(),
            );

            let resp = collect_response(rx).await;
            *response_outer.lock().unwrap() = Some(resp);
            actix::System::current().stop();
        });

        let guard = response.lock().unwrap();
        let resp = guard.as_ref().expect("response must have arrived");
        assert!(
            matches!(resp, SpawnResponse::Failure { .. }),
            "expected SpawnResponse::Failure when model resolution fails",
        );

        // Identity written at step 8 before resolve_model is called — one orphan identity (see agent registry check below for the second orphan).
        let identities_after = identity_registry_for_check
            .list()
            .expect("list after spawn must succeed")
            .len();
        assert_eq!(
            identities_after,
            identities_before + 1,
            "one orphan identity expected: written before resolve_model fails",
        );

        // Agent registry entry also written before resolve_model fails — double-orphan: one identity + one agent registry entry.
        let agent_registry =
            AgentRegistry::open(agent_registry_path_for_check).expect("open agent registry");
        let orphan_count = agent_registry.list().count();
        assert_eq!(
            orphan_count,
            agents_before + 1,
            "one orphan agent registry record expected: written before resolve_model fails",
        );
    }

    // ── SC11: display_name control char is rejected at the message boundary ────

    #[test]
    fn spawn_display_name_rejects_control_char() {
        let dummy_id = IdentityId::new().unwrap();
        let control_char_cases: &[(&str, &str)] = &[
            ("newline", "Agent\nName"),
            ("carriage return", "Agent\rName"),
            ("escape", "Agent\x1bName"),
        ];
        for &(label, bad_name) in control_char_cases {
            let result = actix::System::new().block_on(async {
                let (tx, _rx) = oneshot::channel::<SpawnResponse>();
                let capture = ResponseCapture { tx: Some(tx) }.start();
                SpawnRequest::new(
                    "valid-persona",
                    bad_name,
                    "irrelevant",
                    dummy_id,
                    capture.recipient(),
                )
            });
            assert!(
                matches!(result, Err(SpawnRequestError::InvalidDisplayName { .. })),
                "expected Err(InvalidDisplayName) for display_name with {label}",
            );
        }
    }

    // ── SC12: identity_registry write failure returns SpawnResponse::Failure ───

    #[cfg(unix)]
    #[test]
    fn spawn_identity_registry_write_failure() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt as _;

        let tmp: TempDir = secure_dir();
        let data_dir = tmp.path().to_path_buf();
        write_minimal_persona(&data_dir, "test");

        // Use a dedicated directory for the identity registry so chmod does
        // not interfere with AgentDirs::provision or the agent registry.
        let id_reg_dir = tmp.path().join("id-registry");
        fs::create_dir_all(&id_reg_dir).unwrap();
        fs::set_permissions(&id_reg_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let identity_registry = Arc::new(IdentityRegistry::open(id_reg_dir.clone()).unwrap());

        let (_, watcher, agent_registry_path) = build_registries(&data_dir);
        let sender_id = IdentityId::new().unwrap();

        // Make the identity registry directory read-only after open succeeds so
        // that the write at step 8 of the spawn sequence fails.
        fs::set_permissions(&id_reg_dir, fs::Permissions::from_mode(0o500)).unwrap();

        let response: Arc<Mutex<Option<SpawnResponse>>> = Arc::new(Mutex::new(None));
        let response_outer = Arc::clone(&response);

        actix::System::new().block_on(async move {
            let inbox_starter = NullInboxStarter.start().recipient();

            let (tx, rx) = oneshot::channel();
            let capture_addr = ResponseCapture { tx: Some(tx) }.start();

            let coordinator = build_coordinator(
                &data_dir,
                identity_registry,
                watcher,
                agent_registry_path,
                inbox_starter,
            );
            let coord_addr = coordinator.start();

            coord_addr.do_send(
                SpawnRequest::new(
                    "test",
                    "Test Agent",
                    "irrelevant",
                    sender_id,
                    capture_addr.recipient(),
                )
                .unwrap(),
            );

            let resp = collect_response(rx).await;
            *response_outer.lock().unwrap() = Some(resp);
            actix::System::current().stop();
        });

        // Restore permissions so TempDir cleanup can remove the directory.
        fs::set_permissions(&id_reg_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let guard = response.lock().unwrap();
        assert!(
            matches!(guard.as_ref().unwrap(), SpawnResponse::Failure { .. }),
            "expected SpawnResponse::Failure when identity registry write fails",
        );
    }
}
