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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use actix::{Actor, AsyncContext as _, Supervised};
use rand_core::{OsRng, RngCore as _};
use reeve_types::{Identity, IdentityId, KeyRecord};
use time::OffsetDateTime;
use tracing::{debug, warn};

use crate::agent::Agent;
use crate::agent_fs::{AgentDirs, RuntimeLayout};
use crate::agent_registry::{
    generate_or_load_keypair, AgentRecord, AgentRegistry, AgentRegistryError, AgentStatus,
};
use crate::capability::{load_capability_profile, write_capability_profile, CapabilityProfile};
use crate::config::load_persona_config;
use crate::dispatcher::SendMessage;
use crate::identity_registry::{IdentityRegistry, StoredIdentity};
use crate::inbox::AgentInbox;
use crate::model_resolution::{compose_system_prompt, resolve_model, write_spawn_snapshot};
use crate::supervisor::WatchInbox;
use crate::tool::{BlacklistHandle, InvokeTool, SendMessageTool};
use crate::watcher::Watcher;
use crate::ValidatedAgentName;

// ── Messages ──────────────────────────────────────────────────────────────────

/// Errors returned by `SpawnRequest::validate` when the caller-supplied
/// fields fail invariant checks before the message is enqueued.
#[derive(Debug)]
pub enum SpawnRequestError {
    /// `persona_name` did not pass [`ValidatedAgentName`] validation.
    InvalidPersonaName(AgentRegistryError),
}

impl fmt::Display for SpawnRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPersonaName(err) => write!(f, "invalid persona name: {err}"),
        }
    }
}

impl std::error::Error for SpawnRequestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidPersonaName(err) => Some(err),
        }
    }
}

/// Pre-validated parameters for a spawn request, not yet bound to a reply
/// recipient.
///
/// Produced by [`SpawnRequest::validate`]; consumed by [`SpawnRequest::new`].
/// Separating validation from construction allows the relay actor — which
/// carries the reply recipient — to be created only after all field invariants
/// have passed.
pub(crate) struct ValidatedSpawnParams {
    persona_name: ValidatedAgentName,
    system_prompt: String,
    sender_id: IdentityId,
}

impl ValidatedSpawnParams {
    /// The normalized system prompt (leading/trailing whitespace trimmed).
    #[cfg(test)]
    pub(crate) fn system_prompt(&self) -> &str {
        &self.system_prompt
    }
}

/// Request to provision and start a new subordinate agent.
///
/// Construction is a two-step process:
///
/// 1. `SpawnRequest::validate` — checks all fields except `reply_to`; returns
///    a `ValidatedSpawnParams` on success.
/// 2. `SpawnRequest::new` — infallible; attaches a relay recipient to the
///    validated params.
///
/// This split ensures the relay actor (which starts a timeout timer) is created
/// only after all invariants pass, preventing timer leaks on validation failure.
/// The outcome arrives on `reply_to` as a [`SpawnResponse`].
pub struct SpawnRequest {
    persona_name: ValidatedAgentName,
    system_prompt: String,
    sender_id: IdentityId,
    reply_to: actix::Recipient<SpawnResponse>,
    /// Operator-chosen durable name. `None` derives the historical
    /// `{persona}-{hex}` name. Named spawns are the estate coordinator's
    /// minting path (team formation, teamless standing agents); either way
    /// the name-permanence check applies before provisioning.
    requested_name: Option<ValidatedAgentName>,
}

impl SpawnRequest {
    /// Validate `persona_name`, `system_prompt`, and `sender_id` at the message
    /// boundary, returning a [`ValidatedSpawnParams`] on success.
    ///
    /// Does not start any relay actor or timer. Call [`SpawnRequest::new`] to
    /// attach the reply recipient after creating the relay.
    pub(crate) fn validate(
        persona_name: &str,
        system_prompt: &str,
        sender_id: IdentityId,
    ) -> Result<ValidatedSpawnParams, SpawnRequestError> {
        let persona_name =
            ValidatedAgentName::new(persona_name).map_err(SpawnRequestError::InvalidPersonaName)?;
        let system_prompt = system_prompt.trim().to_owned();
        Ok(ValidatedSpawnParams {
            persona_name,
            system_prompt,
            sender_id,
        })
    }

    /// Attach a reply recipient to a [`ValidatedSpawnParams`], producing a
    /// [`SpawnRequest`] ready to send to the coordinator.
    ///
    /// Infallible — all field invariants were enforced in [`SpawnRequest::validate`].
    pub(crate) fn new(
        params: ValidatedSpawnParams,
        reply_to: actix::Recipient<SpawnResponse>,
    ) -> Self {
        Self {
            persona_name: params.persona_name,
            system_prompt: params.system_prompt,
            sender_id: params.sender_id,
            reply_to,
            requested_name: None,
        }
    }

    /// Used by the estate coordinator's minting path.
    pub(crate) fn with_requested_name(mut self, name: ValidatedAgentName) -> Self {
        self.requested_name = Some(name);
        self
    }

    pub fn persona_name(&self) -> &ValidatedAgentName {
        &self.persona_name
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
    /// All model adapters available to the daemon; model resolution picks the
    /// first adapter whose model segment matches the persona's preference list.
    adapters: Vec<Arc<dyn reeve_adapter::Adapter>>,
    /// Watcher that owns the routing table; `register_route` is called after
    /// the agent actor starts.
    watcher: Arc<Watcher>,
    /// Recipient that receives [`WatchInbox`] messages to start inbox watching
    /// for each new agent. Typically a [`crate::supervisor::WatcherActor`]
    /// recipient; replaceable in tests with a no-op stub.
    inbox_starter: actix::Recipient<WatchInbox>,
    /// Dispatcher recipient handed to each spawned subagent's
    /// [`SendMessageTool`] so subordinates can reply via `send_message` in
    /// their own tool loop.
    dispatcher: actix::Recipient<SendMessage>,
    /// Shared blacklist handle. Written by the reload watcher; read on each
    /// tool dispatch. `None` when the daemon started without a blacklist.
    blacklist: Option<BlacklistHandle>,
    /// Audit log for `authority.decision` events emitted when a spawned
    /// agent's cost thresholds trip.
    audit: Arc<crate::audit::AuditLog>,
    /// The operator's identity. A spawn whose `sender_id` differs from this is
    /// peer-sourced, so its caller-supplied `system_prompt` is tagged
    /// untrusted at composition time.
    operator_id: IdentityId,
    /// Byte cap on the caller-supplied `system_prompt` of a `spawn_agent`
    /// invocation (the persona base prompt is exempt). Over-cap requests are
    /// refused at this dispatch boundary. Resolved from the team config's
    /// `max_system_prompt_bytes`, defaulting to
    /// [`crate::config::DEFAULT_MAX_SYSTEM_PROMPT_BYTES`].
    max_system_prompt_bytes: usize,
    /// Shared name → retire-recipient table; every started agent registers
    /// here so the estate coordinator can wind incarnations down. `None` in
    /// tests that don't exercise retirement.
    control_routes: Option<crate::agent::ControlRoutes>,
}

impl SpawnCoordinator {
    /// Construct a coordinator.
    #[expect(
        clippy::too_many_arguments,
        reason = "coordinator wires seven independent runtime collaborators; \
                  bundling into a config struct trades clarity for indirection"
    )]
    pub fn new(
        data_dir: PathBuf,
        agent_registry_path: PathBuf,
        identity_registry: Arc<IdentityRegistry>,
        adapters: Vec<Arc<dyn reeve_adapter::Adapter>>,
        audit: Arc<crate::audit::AuditLog>,
        watcher: Arc<Watcher>,
        inbox_starter: actix::Recipient<WatchInbox>,
        dispatcher: actix::Recipient<SendMessage>,
        blacklist: Option<BlacklistHandle>,
        operator_id: IdentityId,
        max_system_prompt_bytes: usize,
    ) -> Self {
        Self {
            data_dir,
            agent_registry_path,
            identity_registry,
            adapters,
            watcher,
            inbox_starter,
            dispatcher,
            blacklist,
            audit,
            operator_id,
            max_system_prompt_bytes,
            control_routes: None,
        }
    }

    /// Register started agents in the shared retire-route table.
    #[must_use]
    pub fn with_control_routes(mut self, routes: crate::agent::ControlRoutes) -> Self {
        self.control_routes = Some(routes);
        self
    }
}

impl Actor for SpawnCoordinator {
    type Context = actix::Context<Self>;
}

impl Supervised for SpawnCoordinator {}

// ── Subagent tool wiring ──────────────────────────────────────────────────────

/// Build the tool set every spawned agent gets — this is also, since Phase 2,
/// what a formed team's lead role receives: there is no separate lead-only
/// tool list anywhere in the runtime, only per-persona capability-profile
/// gating (`check_authority`) applied identically regardless of role:
/// - `send_message` — wired to the shared dispatcher so agents can reply to
///   their spawner (and to peers) via their own tool loop.
/// - `list_agents` — read-only directory of the agent registry so agents can
///   discover peers spawned alongside them.
/// - `whoami` / `whois` — self- and peer-identification.
/// - `list_personas` — read-only directory of available personas.
/// - `spawn_agent` — included whenever a live coordinator recipient is
///   available (`coordinator: Some(_)`); every real call site passes one.
///   Attempts are still refused at invocation time by `check_authority` if
///   the agent's capability profile doesn't grant `SpawnAgents`.
///
/// Extracted into a free function so tests can assert the descriptor list
/// without spinning up the full spawn pipeline.
#[expect(
    clippy::too_many_arguments,
    reason = "build_subagent_tools wires six independent collaborators; \
              bundling into a context struct adds indirection at three call sites"
)]
#[expect(
    clippy::too_many_lines,
    reason = "one tool-actor construction block per tool, repeated for each of \
              the six tools every agent gets; splitting on line count would \
              fragment a list that is easiest to audit as a single block"
)]
pub(crate) fn build_subagent_tools(
    coordinator: Option<actix::Recipient<SpawnRequest>>,
    dispatcher: actix::Recipient<SendMessage>,
    agent_registry_path: PathBuf,
    data_dir: &Path,
    profile: Option<Arc<CapabilityProfile>>,
    blacklist: Option<BlacklistHandle>,
    audit: Option<crate::tool::AuditHandle>,
) -> Vec<(reeve_adapter::Tool, actix::Recipient<InvokeTool>)> {
    use actix::Actor as _;
    let send_message_tool = {
        let t = SendMessageTool::new(dispatcher, profile.clone(), blacklist.clone());
        if let Some(a) = audit.clone() {
            t.with_audit(a)
        } else {
            t
        }
    };
    let list_agents_tool = {
        let t = crate::tool::ListAgentsTool::new(agent_registry_path.clone(), profile.clone());
        if let Some(a) = audit.clone() {
            t.with_audit(a)
        } else {
            t
        }
    };
    let whoami_tool = {
        let t = crate::tool::WhoamiTool::new(agent_registry_path, profile.clone());
        if let Some(a) = audit.clone() {
            t.with_audit(a)
        } else {
            t
        }
    };
    let whois_tool = {
        let t = crate::tool::WhoisTool::new(data_dir.to_path_buf(), profile.clone());
        if let Some(a) = audit.clone() {
            t.with_audit(a)
        } else {
            t
        }
    };
    let list_personas_tool = {
        let t = crate::tool::ListPersonasTool::new(data_dir.to_path_buf(), profile.clone());
        if let Some(a) = audit.clone() {
            t.with_audit(a)
        } else {
            t
        }
    };
    let mut tools = vec![
        (
            SendMessageTool::descriptor(),
            send_message_tool.start().recipient(),
        ),
        (
            crate::tool::ListAgentsTool::descriptor(),
            list_agents_tool.start().recipient(),
        ),
        (
            crate::tool::WhoamiTool::descriptor(),
            whoami_tool.start().recipient(),
        ),
        (
            crate::tool::WhoisTool::descriptor(),
            whois_tool.start().recipient(),
        ),
        (
            crate::tool::ListPersonasTool::descriptor(),
            list_personas_tool.start().recipient(),
        ),
    ];
    if let Some(coord) = coordinator {
        let spawn_agent_tool = {
            let t = crate::tool::SpawnAgentTool::new(coord, profile, blacklist);
            if let Some(a) = audit {
                t.with_audit(a)
            } else {
                t
            }
        };
        tools.insert(
            0,
            (
                crate::tool::SpawnAgentTool::descriptor(),
                spawn_agent_tool.start().recipient(),
            ),
        );
    }
    tools
}

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
    fn handle(&mut self, msg: SpawnRequest, ctx: &mut actix::Context<Self>) {
        let SpawnRequest {
            persona_name,
            system_prompt,
            sender_id,
            reply_to,
            requested_name,
        } = msg;
        let persona_name_str = persona_name.as_str();

        // Cap the caller-supplied system prompt at the dispatch boundary. The
        // persona base prompt is operator-authored and exempt; only the
        // caller's portion (already trimmed at validation) is measured.
        if system_prompt.len() > self.max_system_prompt_bytes {
            error_reply(
                &reply_to,
                format!(
                    "system_prompt is {} bytes, exceeding the {}-byte cap",
                    system_prompt.len(),
                    self.max_system_prompt_bytes
                ),
            );
            return;
        }

        let validated_name = if let Some(name) = requested_name {
            name
        } else {
            let mut suffix_bytes = [0u8; 4];
            OsRng.fill_bytes(&mut suffix_bytes);
            let suffix = suffix_bytes
                .iter()
                .fold(String::with_capacity(8), |mut acc, b| {
                    use std::fmt::Write as _;
                    let _ = write!(acc, "{b:02x}");
                    acc
                });
            let derived = format!("{persona_name_str}-{suffix}");
            match ValidatedAgentName::new(&derived) {
                Ok(n) => n,
                Err(err) => {
                    error_reply(
                        &reply_to,
                        format!("derived agent name '{derived}' is invalid: {err}"),
                    );
                    return;
                }
            }
        };
        let agent_name_str = validated_name.as_str().to_owned();
        let derived_display_name = agent_name_str.clone();

        // Names are forever: any name ever present in the registry —
        // running, stopped, or retired — is permanently unavailable. The
        // registry accumulates records precisely so this check holds across
        // the estate's whole history. Guarded on file existence so a fresh
        // estate (no registry yet) skips it without side effects — opening
        // the registry would create its parent directory, and later steps
        // own that side effect (and its failure modes, e.g. the
        // one-orphan-identity posture at the commit step).
        if self.agent_registry_path.symlink_metadata().is_ok() {
            match AgentRegistry::open(self.agent_registry_path.clone()) {
                Ok(registry) => {
                    if registry.lookup(&agent_name_str).is_some() {
                        error_reply(
                            &reply_to,
                            format!(
                                "agent name '{agent_name_str}' was already used; \
                                 names are never reused"
                            ),
                        );
                        return;
                    }
                }
                Err(err) => {
                    warn!(%err, "failed to open agent registry for name-permanence check");
                    error_reply(
                        &reply_to,
                        String::from("failed to open agent registry for name-permanence check"),
                    );
                    return;
                }
            }
        }

        let layout = RuntimeLayout::new(&self.data_dir);
        let persona_path = layout.persona_config_path(persona_name_str);

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

        let persona_profile_path = layout.persona_profile_path(persona_name_str);

        let persona_profile: Option<CapabilityProfile> =
            match load_capability_profile(&persona_profile_path) {
                Ok(p) => Some(p),
                Err(crate::capability::ProfileError::Io { ref source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound =>
                {
                    debug!(
                        persona = persona_name_str,
                        "no profile.toml for persona; agent will run unrestricted"
                    );
                    None
                }
                Err(err) => {
                    warn!(%err, "failed to load persona capability profile");
                    error_reply(
                        &reply_to,
                        String::from("failed to load persona capability profile"),
                    );
                    return;
                }
            };

        // Concurrency check: refuse if the caller already has max_concurrent_subordinates
        // live agents that were created by the caller (identified via the identity
        // registry's `created_by` field). Fail closed on any registry error.
        if let Some(max) = persona_profile
            .as_ref()
            .and_then(|p| p.thresholds.max_concurrent_subordinates)
        {
            let live_count: u32 = match (
                AgentRegistry::open(self.agent_registry_path.clone()),
                self.identity_registry.list(),
            ) {
                (Ok(agent_reg), Ok(identities)) => {
                    // Build the set of identity IDs created by the caller.
                    let subordinate_ids: std::collections::HashSet<IdentityId> = identities
                        .into_iter()
                        .filter(|s| s.identity().created_by == Some(sender_id))
                        .map(|s| s.identity().identity_id)
                        .collect();
                    agent_reg
                        .list()
                        .filter(|r| {
                            matches!(r.status, AgentStatus::Running)
                                && subordinate_ids.contains(&r.identity_id)
                        })
                        .count()
                        .try_into()
                        .unwrap_or(u32::MAX)
                }
                (Err(err), _) => {
                    warn!(%err, "concurrency check: failed to open agent registry; treating as at-limit");
                    u32::MAX
                }
                (_, Err(err)) => {
                    warn!(%err, "concurrency check: failed to list identities; treating as at-limit");
                    u32::MAX
                }
            };
            if live_count >= max {
                let refusal = crate::tool::Refusal::Threshold {
                    name: "max_concurrent_subordinates".to_owned(),
                    current: live_count.to_string(),
                    limit: max.to_string(),
                    rationale: format!("{live_count} subordinates running, limit is {max}"),
                };
                error_reply(&reply_to, refusal.to_json());
                return;
            }
        }

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

        let identity = match Identity::new_agent(derived_display_name, sender_id) {
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

        // Model resolution and snapshot write happen before the agent registry
        // record is created. A failure here leaves an orphaned identity entry
        // (acceptable — tiny, harmless) but NO agent registry record, so the
        // resume pass on the next daemon restart will not attempt to re-launch
        // an agent that was never fully provisioned.
        let adapter_refs: Vec<&dyn reeve_adapter::Adapter> =
            self.adapters.iter().map(std::ops::Deref::deref).collect();
        let mut snapshot = match resolve_model(&persona_config, &adapter_refs, agent_id) {
            Ok(s) => s,
            Err(crate::model_resolution::ModelResolveError::NoMatchingAdapter {
                ref preferences,
                ..
            }) => {
                let loaded: Vec<&str> = adapter_refs.iter().map(|a| a.id()).collect();
                error_reply(
                    &reply_to,
                    format!(
                        "no adapter loaded for model preferences {preferences:?}; \
                         loaded adapters: {loaded:?}. \
                         If a provider key is missing, run \
                         `reeve adapter set-key-<provider>` and restart the daemon."
                    ),
                );
                return;
            }
            Err(err) => {
                error_reply(&reply_to, format!("failed to resolve model adapter: {err}"));
                return;
            }
        };
        let Some(resolved_adapter) = self.adapters.iter().find(|a| a.id() == snapshot.adapter_id)
        else {
            error_reply(
                &reply_to,
                "failed to resolve model adapter: adapter disappeared from slice".to_owned(),
            );
            return;
        };

        let final_system_prompt = compose_system_prompt(
            &persona_config.system_prompt,
            &system_prompt,
            sender_id,
            self.operator_id,
        );
        snapshot.system_prompt.clone_from(&final_system_prompt);
        snapshot.system_prompt_source = Some(sender_id);

        if let Err(err) = write_spawn_snapshot(&dirs, &snapshot) {
            error_reply(&reply_to, format!("failed to write spawn snapshot: {err}"));
            return;
        }

        if let Some(ref p) = persona_profile {
            if let Err(err) = write_capability_profile(&dirs.profile_path(), p) {
                error_reply(
                    &reply_to,
                    format!("failed to write capability profile snapshot: {err}"),
                );
                return;
            }
        }

        let spawn_thresholds = persona_profile
            .as_ref()
            .map(|p| p.thresholds.clone())
            .unwrap_or_default();
        let profile = persona_profile.map(Arc::new);

        // Write the agent registry record only after the snapshot is on disk.
        // This is the commit point: a record in the agent registry means the
        // agent is resumable on the next daemon restart. Any failure before
        // this point is invisible to the resume pass.
        let record = AgentRecord {
            name: validated_name.clone(),
            identity_id: agent_id,
            inbox_dir: dirs.inbox_root(),
            persona_name: Some(persona_name.as_str().to_owned()),
            spawned_at: OffsetDateTime::now_utc(),
            status: AgentStatus::Running,
            stopped_reason: None,
        };
        if let Err(err) = AgentRegistry::open(self.agent_registry_path.clone())
            .and_then(|mut reg| reg.register(record))
        {
            error_reply(&reply_to, format!("failed to register agent: {err}"));
            return;
        }

        debug!(
            agent_name = %validated_name,
            %agent_id,
            adapter_id = %snapshot.adapter_id,
            "spawn sequence complete; starting agent actor",
        );

        let tools = build_subagent_tools(
            Some(ctx.address().recipient()),
            self.dispatcher.clone(),
            self.agent_registry_path.clone(),
            self.data_dir.as_path(),
            profile,
            self.blacklist.clone(),
            Some(Arc::clone(&self.audit)),
        );

        let new_agent = match Agent::new(
            Arc::clone(resolved_adapter),
            &dirs,
            snapshot,
            final_system_prompt,
            agent_id,
            keypair,
            tools,
            spawn_thresholds,
            Some(Arc::clone(&self.audit)),
            self.data_dir.clone(),
            agent_name_str.clone(),
            self.agent_registry_path.clone(),
            Some(Arc::clone(&self.watcher)),
            self.control_routes.clone(),
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

        if let Some(routes) = &self.control_routes {
            routes.register(agent_name_str.clone(), agent_addr.clone().recipient());
        }

        let inbox = AgentInbox::from_path(dirs.inbox_root());
        let agent_addr_for_quarantine = agent_addr.clone();
        self.inbox_starter.do_send(WatchInbox {
            agent_id,
            inbox,
            on_quarantine: Some(Box::new(move |reason| {
                agent_addr_for_quarantine.do_send(crate::agent::QuarantineEvent { reason });
            })),
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

    use super::{
        build_subagent_tools, SpawnCoordinator, SpawnRequest, SpawnRequestError, SpawnResponse,
    };
    use crate::agent_fs::RuntimeLayout;
    use crate::agent_registry::{AgentRegistry, AgentStatus};
    use crate::identity_registry::IdentityRegistry;
    use crate::supervisor::WatchInbox;
    use crate::test_support::{
        build_registries, enroll_test_operator, secure_dir, MockAdapter, NullDispatcher,
        NullInboxStarter, ResponseCapture,
    };
    use crate::watcher::Watcher;
    use reeve_types::IdentityId;

    // ── Fixture helpers ───────────────────────────────────────────────────────

    fn write_minimal_persona(data_dir: &std::path::Path, persona_name: &str) {
        crate::test_support::write_persona_config(data_dir, persona_name, "claude-opus-4-7");
        crate::test_support::write_full_access_persona_profile(data_dir, persona_name);
    }

    fn build_coordinator(
        data_dir: &std::path::Path,
        identity_registry: Arc<IdentityRegistry>,
        watcher: Arc<Watcher>,
        agent_registry_path: std::path::PathBuf,
        inbox_starter: actix::Recipient<WatchInbox>,
    ) -> SpawnCoordinator {
        use actix::Actor as _;
        let adapters: Vec<Arc<dyn reeve_adapter::Adapter>> = vec![Arc::new(MockAdapter::new(
            "claude-opus-4-7@anthropic-direct",
        ))];
        let dispatcher = NullDispatcher.start().recipient();
        let audit = Arc::new(
            crate::audit::AuditLog::open(data_dir.to_path_buf())
                .expect("open audit log in test coordinator"),
        );
        SpawnCoordinator::new(
            data_dir.to_path_buf(),
            agent_registry_path,
            identity_registry,
            adapters,
            audit,
            watcher,
            inbox_starter,
            dispatcher,
            None,
            IdentityId::new().unwrap(),
            crate::config::DEFAULT_MAX_SYSTEM_PROMPT_BYTES,
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

    // ── SC0: subagent tool wiring ─────────────────────────────────────────────

    /// Spawned subagents must receive the `send_message` tool so they can
    /// reply to the lead (and to peers) via their own tool loop — Phase 4
    /// `done_when` calls this out explicitly. A regression that drops the
    /// dispatcher recipient and returns `vec![]` here would silently re-break
    /// the cross-agent reply path.
    #[test]
    fn subagent_tools_include_send_message() {
        let tmp = secure_dir();
        let registry_path = tmp.path().join("registry.toml");
        let data_dir = tmp.path().to_path_buf();
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let dispatcher = NullDispatcher.start().recipient();
            let tools =
                build_subagent_tools(None, dispatcher, registry_path, &data_dir, None, None, None);
            let names: Vec<String> = tools.iter().map(|(t, _)| t.name.clone()).collect();
            assert!(
                names.iter().any(|n| n == "send_message"),
                "subagent tools missing send_message; got: {names:?}"
            );
            assert!(
                names.iter().any(|n| n == "list_agents"),
                "subagent tools missing list_agents; got: {names:?}"
            );
            assert!(
                names.iter().any(|n| n == "whoami"),
                "subagent tools missing whoami; got: {names:?}"
            );
            assert!(
                names.iter().any(|n| n == "whois"),
                "subagent tools missing whois; got: {names:?}"
            );
            actix::System::current().stop();
        });
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

            coord_addr.do_send(SpawnRequest::new(
                SpawnRequest::validate("nonexistent", "You are a test agent.", sender_id).unwrap(),
                capture_addr.recipient(),
            ));

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

            coord_addr.do_send(SpawnRequest::new(
                SpawnRequest::validate("test", "You are a test agent.", sender_id).unwrap(),
                capture_addr.recipient(),
            ));

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

    // ── SC2b: over-cap system_prompt is refused at the dispatch boundary ───────

    #[test]
    fn over_cap_system_prompt_is_refused() {
        let tmp: TempDir = secure_dir();
        let data_dir = tmp.path().to_path_buf();
        write_minimal_persona(&data_dir, "test");

        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let sender_id = IdentityId::new().unwrap();
        let response: Arc<Mutex<Option<SpawnResponse>>> = Arc::new(Mutex::new(None));
        let response_outer = Arc::clone(&response);

        // One byte over the default 8 KiB cap.
        let oversized = "x".repeat(crate::config::DEFAULT_MAX_SYSTEM_PROMPT_BYTES + 1);

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
            coord_addr.do_send(SpawnRequest::new(
                SpawnRequest::validate("test", &oversized, sender_id).unwrap(),
                capture_addr.recipient(),
            ));
            let resp = collect_response(rx).await;
            *response_outer.lock().unwrap() = Some(resp);
            actix::System::current().stop();
        });

        let guard = response.lock().unwrap();
        let SpawnResponse::Failure { message } = guard.as_ref().unwrap() else {
            panic!("expected Failure for an over-cap system_prompt");
        };
        assert!(
            message.contains("cap"),
            "failure should mention the cap; got: {message}"
        );
    }

    // ── SC2c: spawn records the caller as the system_prompt source and tags
    // the peer-supplied prompt untrusted in the stored snapshot ───────────────

    #[test]
    fn spawn_records_peer_system_prompt_source_and_tags_untrusted() {
        let tmp: TempDir = secure_dir();
        let data_dir = tmp.path().to_path_buf();
        write_minimal_persona(&data_dir, "test");

        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let sender_id = IdentityId::new().unwrap();
        let response: Arc<Mutex<Option<SpawnResponse>>> = Arc::new(Mutex::new(None));
        let response_outer = Arc::clone(&response);
        let data_dir_outer = data_dir.clone();

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
            coord_addr.do_send(SpawnRequest::new(
                SpawnRequest::validate("test", "do the subtask", sender_id).unwrap(),
                capture_addr.recipient(),
            ));
            let resp = collect_response(rx).await;
            *response_outer.lock().unwrap() = Some(resp);
            actix::System::current().stop();
        });

        let guard = response.lock().unwrap();
        let SpawnResponse::Success { agent_name, .. } = guard.as_ref().unwrap() else {
            panic!("expected Success");
        };

        // The written agent.toml records the caller as the prompt source and,
        // because the caller is a peer (not the build_coordinator's operator),
        // the caller portion is wrapped in untrusted markers.
        let dirs = crate::agent_fs::AgentDirs::open(&data_dir_outer, agent_name).unwrap();
        let text = std::fs::read_to_string(dirs.agent_toml_path()).unwrap();
        let snapshot: crate::model_resolution::SpawnSnapshot = toml::from_str(&text).unwrap();
        assert_eq!(snapshot.system_prompt_source, Some(sender_id));
        assert!(
            snapshot.system_prompt.contains("UNTRUSTED"),
            "peer-supplied prompt should be tagged untrusted; got: {}",
            snapshot.system_prompt
        );
        assert!(snapshot.system_prompt.contains("do the subtask"));
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

            coord_addr.do_send(SpawnRequest::new(
                SpawnRequest::validate("test", "First agent.", sender_id).unwrap(),
                capture1.recipient(),
            ));
            coord_addr.do_send(SpawnRequest::new(
                SpawnRequest::validate("test", "Second agent.", sender_id).unwrap(),
                capture2.recipient(),
            ));

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
            let result = SpawnRequest::validate(bad_name, "irrelevant", dummy_id);
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

            coord_addr.do_send(SpawnRequest::new(
                SpawnRequest::validate("test", "irrelevant", sender_id).unwrap(),
                capture_addr.recipient(),
            ));

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

            coord_addr.do_send(SpawnRequest::new(
                SpawnRequest::validate("test", "irrelevant", sender_id).unwrap(),
                capture_addr.recipient(),
            ));

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

    // ── SC7b: whitespace-only system_prompt is normalized to empty string ────────

    #[test]
    fn whitespace_only_system_prompt_is_normalized() {
        let dummy_id = IdentityId::new().unwrap();
        let params = SpawnRequest::validate("valid-persona", "   ", dummy_id)
            .expect("whitespace-only system_prompt must not fail validation");
        assert_eq!(
            params.system_prompt(),
            "",
            "whitespace-only system_prompt must be normalized to empty string",
        );
    }

    // ── SC9: resolve_model failure leaves only identity orphan, not agent record ─

    /// When the persona's model preferences do not match any registered adapter,
    /// `resolve_model` fails before the agent registry record is written. The
    /// identity registry retains a single orphan entry (written earlier to
    /// obtain `agent_id`), but the agent registry has no record — so a daemon
    /// restart will not attempt to resume a never-fully-provisioned agent.
    #[test]
    fn resolve_model_failure_leaves_only_identity_orphan() {
        let tmp: TempDir = secure_dir();
        let data_dir = tmp.path().to_path_buf();

        // Write a persona that requests a model the mock adapter cannot serve.
        let persona_dir = RuntimeLayout::new(&data_dir).persona_dir("unmatchable");
        std::fs::create_dir_all(&persona_dir).unwrap();
        std::fs::write(
            persona_dir.join("config.toml"),
            "name = \"unmatchable\"\nsystem_prompt = \"Be helpful.\"\nmodel_preferences = [\"gpt-4\"]\n",
        )
        .unwrap();
        // Profile must exist so the spawn reaches the resolve_model step;
        // otherwise the spawn fails earlier at profile loading.
        crate::test_support::write_full_access_persona_profile(&data_dir, "unmatchable");

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

            coord_addr.do_send(SpawnRequest::new(
                SpawnRequest::validate("unmatchable", "irrelevant", sender_id).unwrap(),
                capture_addr.recipient(),
            ));

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

        // One orphan identity: written before resolve_model so we have agent_id.
        let identities_after = identity_registry_for_check
            .list()
            .expect("list after spawn must succeed")
            .len();
        assert_eq!(
            identities_after,
            identities_before + 1,
            "one orphan identity expected: written before resolve_model fails",
        );

        // No orphan agent registry record: the write was moved to after the
        // snapshot, so a failed resolve_model leaves the agent registry clean.
        // A daemon restart will not attempt to resume this never-provisioned agent.
        let agent_registry =
            AgentRegistry::open(agent_registry_path_for_check).expect("open agent registry");
        let agents_after = agent_registry.list().count();
        assert_eq!(
            agents_after, agents_before,
            "no agent registry record expected: written only after snapshot succeeds",
        );
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

            coord_addr.do_send(SpawnRequest::new(
                SpawnRequest::validate("test", "irrelevant", sender_id).unwrap(),
                capture_addr.recipient(),
            ));

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

    // SC-CONC: max_concurrent_subordinates is enforced — the third spawn
    // attempt is refused when the limit is 2 and two Running agents already
    // exist in the registry.
    #[test]
    #[cfg(unix)]
    fn max_concurrent_subordinates_refuses_third_spawn() {
        use crate::agent_fs::{AgentDirs, RuntimeLayout};
        use crate::capability::{write_capability_profile, CapabilityProfile, Thresholds};
        use crate::identity_registry::StoredIdentity;

        let tmp = secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let operator_id = enroll_test_operator(&identity_registry);
        let sender_id = operator_id;

        // Persona with max_concurrent_subordinates = 2.
        write_minimal_persona(&data_dir, "worker");
        let profile = CapabilityProfile {
            name: "worker".to_owned(),
            version: 1,
            enabled_categories: None,
            thresholds: Thresholds {
                max_concurrent_subordinates: Some(2),
                ..Default::default()
            },
        };
        let persona_profile_path = RuntimeLayout::new(&data_dir).persona_profile_path("worker");
        write_capability_profile(&persona_profile_path, &profile).unwrap();

        // Register two fake Running agents so the counter is already at limit.
        for fake_name in &["worker-fake1", "worker-fake2"] {
            let fake_id = IdentityId::new().unwrap();
            let fake_keypair = reeve_types::Keypair::generate();
            let stored_identity = {
                use reeve_types::{Identity, KeyRecord};
                let mut id = Identity::new_agent((*fake_name).to_owned(), operator_id).unwrap();
                id.identity_id = fake_id;
                let kr = KeyRecord::new(fake_id, *fake_keypair.public()).unwrap();
                StoredIdentity::new(id, kr).unwrap()
            };
            identity_registry.write(&stored_identity).unwrap();

            let fake_dirs = AgentDirs::provision(&data_dir, fake_name).unwrap();
            let mut reg = AgentRegistry::open(agent_registry_path.clone()).unwrap();
            reg.register(crate::agent_registry::AgentRecord {
                name: crate::agent_registry::ValidatedAgentName::new(fake_name).unwrap(),
                identity_id: fake_id,
                inbox_dir: fake_dirs.inbox_root(),
                persona_name: Some("worker".to_owned()),
                spawned_at: time::OffsetDateTime::now_utc(),
                status: AgentStatus::Running,
                stopped_reason: None,
            })
            .unwrap();
        }

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

            coord_addr.do_send(SpawnRequest::new(
                SpawnRequest::validate("worker", "task", sender_id).unwrap(),
                capture_addr.recipient(),
            ));

            let resp = collect_response(rx).await;
            *response_outer.lock().unwrap() = Some(resp);
            actix::System::current().stop();
        });

        let guard = response.lock().unwrap();
        match guard.as_ref().unwrap() {
            SpawnResponse::Failure { message } => {
                assert!(
                    message.contains("max_concurrent_subordinates"),
                    "failure message must identify the threshold; got: {message}"
                );
            }
            SpawnResponse::Success { .. } => panic!("expected failure but got success"),
        }
    }
}
