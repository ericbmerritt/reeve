//! The estate coordinator: operator-tier organizational operations.
//!
//! Estate operations — opening, closing, and reopening engagements — arrive
//! as signed envelopes in the coordinator's maildir inbox, exactly the path
//! any agent message takes (decision A1 in
//! `specs/reeve-organization.ladder.md`: the filesystem is the protocol; no
//! socket, no RPC). The CLI and the TUI slash-command both sign as the
//! operator and deposit to `agents/estate/inbox/new/`; the watcher verifies
//! the signature and delivers [`ProcessInbound`] here like anywhere else.
//!
//! Authority is the operator tier: an envelope whose verified sender is not
//! the enrolled operator is refused, and — like every refusal on this actor —
//! audited as `engagement.op_refused`. There is no reply channel; the
//! durable engagement record is the operation's observable effect, and the
//! audit log is its receipt.
//!
//! The coordinator's identity and inbox are registered in
//! [`crate::system_registry::SystemRegistry`] under the reserved name
//! `estate`, not in `AgentRegistry` — it is not a model-backed agent, has no
//! persona, and never has an incarnation. `estate` stays a reserved *agent*
//! name too (see the `mint-agent`/`retire-agent` refusals below): an
//! operator must not be able to mint a real agent that shadows the
//! coordinator's name in a different registry.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use actix::{Actor, ActorContext as _, AsyncContext as _, Context, Handler, Supervised};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::{info, warn};

use crate::agent::{ControlRoutes, ProcessInbound, Retire};
use crate::agent_registry::{AgentRegistry, AgentStatus};
use crate::audit::{AuditEvent, AuditLog};
use crate::engagement::{EngagementError, EngagementRegistry};
use crate::spawn_coordinator::{SpawnRequest, SpawnResponse};
use crate::team::{MemberDisposition, TeamMemberRecord, TeamRecord, TeamRegistry, TeamState};

/// Reserved agent-registry name for the estate coordinator.
pub const ESTATE_AGENT_NAME: &str = "estate";

/// Runtime collaborators for team and agent operations. Bundled so the
/// engagement-only construction (and its tests) stays untouched; the daemon
/// always wires this.
pub struct TeamOpsDeps {
    /// Spawn path used to mint member agents with requested names.
    pub spawner: actix::Recipient<SpawnRequest>,
    pub teams: TeamRegistry,
    pub control_routes: ControlRoutes,
    pub agent_registry_path: PathBuf,
    /// Data root, for loading team templates.
    pub data_dir: PathBuf,
}

/// How long the coordinator waits for one member mint to complete.
const MINT_TIMEOUT: Duration = Duration::from_secs(30);

/// An estate operation, carried as the JSON payload of a signed envelope.
///
/// The `op` tag values are the operations vocabulary of
/// `specs/reeve-organization.md` § Operations Vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum EstateOp {
    /// Open a top-level engagement. `root` must be an absolute path when
    /// present; the coordinator canonicalizes it before recording.
    OpenEngagement {
        name: String,
        purpose: String,
        #[serde(default)]
        root: Option<PathBuf>,
    },
    /// Close an open engagement.
    CloseEngagement { name: String },
    /// Reopen a closed engagement with its recorded context intact.
    ReopenEngagement { name: String },
    /// Form a standing team from a template, minting its members as durable
    /// named agents.
    FormTeam {
        name: String,
        /// Template file stem under `teams/`; defaults to `name` at the CLI.
        template: String,
    },
    /// Dissolve a standing team with a per-member disposition. Members
    /// absent from the map default to `retired`.
    DissolveTeam {
        name: String,
        #[serde(default)]
        dispositions: BTreeMap<String, MemberDisposition>,
    },
    /// Mint a teamless standing agent.
    MintAgent { name: String, persona: String },
    /// Permanently retire a teamless agent (standing-team members are
    /// retired through dissolution, not individually).
    RetireAgent { name: String },
}

impl EstateOp {
    /// The operation verb as it appears in audit events and payloads.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::OpenEngagement { .. } => "open-engagement",
            Self::CloseEngagement { .. } => "close-engagement",
            Self::ReopenEngagement { .. } => "reopen-engagement",
            Self::FormTeam { .. } => "form-team",
            Self::DissolveTeam { .. } => "dissolve-team",
            Self::MintAgent { .. } => "mint-agent",
            Self::RetireAgent { .. } => "retire-agent",
        }
    }

    /// The engagement/team/agent name the operation targets.
    pub fn name(&self) -> &str {
        match self {
            Self::OpenEngagement { name, .. }
            | Self::CloseEngagement { name }
            | Self::ReopenEngagement { name }
            | Self::FormTeam { name, .. }
            | Self::DissolveTeam { name, .. }
            | Self::MintAgent { name, .. }
            | Self::RetireAgent { name } => name,
        }
    }
}

fn refusal_reason(err: &EngagementError) -> &'static str {
    match err {
        EngagementError::InvalidName { .. } => "invalid_name",
        EngagementError::NameTaken { .. } => "name_taken",
        EngagementError::NotFound { .. } => "not_found",
        EngagementError::WrongState { .. } => "wrong_state",
        EngagementError::RelativeRoot { .. } => "relative_root",
        EngagementError::Io { .. } => "io_error",
        EngagementError::Toml { .. } => "record_corrupt",
    }
}

/// Actor handling estate operations delivered through the signed-envelope
/// transport.
pub struct EstateCoordinator {
    operator_id: reeve_types::IdentityId,
    engagements: EngagementRegistry,
    audit: Arc<AuditLog>,
    /// Collaborators for team/agent operations. `None` only in
    /// engagement-focused tests; the daemon always wires it, and an unwired
    /// team op is refused (and audited) rather than panicking.
    team_ops: Option<Arc<TeamOpsDeps>>,
}

impl EstateCoordinator {
    pub fn new(
        operator_id: reeve_types::IdentityId,
        engagements: EngagementRegistry,
        audit: Arc<AuditLog>,
    ) -> Self {
        Self {
            operator_id,
            engagements,
            audit,
            team_ops: None,
        }
    }

    /// Wire the team/agent operation collaborators.
    #[must_use]
    pub fn with_team_ops(mut self, deps: TeamOpsDeps) -> Self {
        self.team_ops = Some(Arc::new(deps));
        self
    }

    fn audit_event(&self, event: &AuditEvent) {
        if let Err(err) = self.audit.append(event) {
            warn!(err = %err, "estate coordinator failed to append audit event");
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the parameters mirror the fields of the EngagementOpRefused \
                  audit event one-to-one; a bundling struct would duplicate \
                  that event's shape for no reader benefit"
    )]
    fn refuse(
        &self,
        sender_id: Option<reeve_types::IdentityId>,
        op: &str,
        name: Option<String>,
        reason: &str,
        at: OffsetDateTime,
    ) {
        warn!(op, ?name, reason, "estate operation refused");
        self.audit_event(&AuditEvent::EngagementOpRefused {
            sender_id,
            op: op.to_owned(),
            name,
            reason: reason.to_owned(),
            at,
        });
    }

    #[expect(
        clippy::too_many_lines,
        reason = "linear match over the engagement operations; each arm is \
                  the same execute-audit-or-refuse shape"
    )]
    fn execute(&self, op: &EstateOp, sender_id: reeve_types::IdentityId, at: OffsetDateTime) {
        match op {
            EstateOp::OpenEngagement {
                name,
                purpose,
                root,
            } => {
                // Canonicalize so the recorded context — the path the
                // effectors ladder will enforce as the file jail — is the
                // resolved directory, not whatever spelling the operator
                // typed. A root that does not resolve is a refusal, not a
                // best-effort record.
                let canonical_root = match root {
                    Some(raw) => match std::fs::canonicalize(raw) {
                        Ok(resolved) => Some(resolved),
                        Err(err) => {
                            warn!(root = %raw.display(), err = %err, "engagement root does not resolve");
                            self.refuse(
                                Some(sender_id),
                                op.verb(),
                                Some(name.clone()),
                                "root_unresolvable",
                                at,
                            );
                            return;
                        }
                    },
                    None => None,
                };
                match self
                    .engagements
                    .open_engagement(name, purpose, canonical_root, at)
                {
                    Ok(record) => {
                        info!(name, root = ?record.root, "engagement opened");
                        self.audit_event(&AuditEvent::EngagementOpened {
                            sender_id,
                            name: record.name,
                            root: record.root,
                            at,
                        });
                    }
                    Err(err) => self.refuse(
                        Some(sender_id),
                        op.verb(),
                        Some(name.clone()),
                        refusal_reason(&err),
                        at,
                    ),
                }
            }
            EstateOp::CloseEngagement { name } => match self.engagements.close(name) {
                Ok(_) => {
                    info!(name, "engagement closed");
                    self.audit_event(&AuditEvent::EngagementClosed {
                        sender_id,
                        name: name.clone(),
                        at,
                    });
                }
                Err(err) => self.refuse(
                    Some(sender_id),
                    op.verb(),
                    Some(name.clone()),
                    refusal_reason(&err),
                    at,
                ),
            },
            EstateOp::ReopenEngagement { name } => match self.engagements.reopen(name) {
                Ok(_) => {
                    info!(name, "engagement reopened");
                    self.audit_event(&AuditEvent::EngagementReopened {
                        sender_id,
                        name: name.clone(),
                        at,
                    });
                }
                Err(err) => self.refuse(
                    Some(sender_id),
                    op.verb(),
                    Some(name.clone()),
                    refusal_reason(&err),
                    at,
                ),
            },
            EstateOp::FormTeam { .. }
            | EstateOp::DissolveTeam { .. }
            | EstateOp::MintAgent { .. }
            | EstateOp::RetireAgent { .. } => {
                unreachable!("team ops are dispatched to execute_team_op, not execute")
            }
        }
    }
}

impl Actor for EstateCoordinator {
    type Context = Context<Self>;
}

impl Supervised for EstateCoordinator {}

impl Handler<ProcessInbound> for EstateCoordinator {
    type Result = ();

    fn handle(&mut self, msg: ProcessInbound, ctx: &mut Context<Self>) {
        let at = OffsetDateTime::now_utc();
        let op: EstateOp = match serde_json::from_str(&msg.payload) {
            Ok(op) => op,
            Err(err) => {
                warn!(err = %err, "estate payload is not a valid operation");
                self.refuse(Some(msg.sender_id), "unknown", None, "invalid_payload", at);
                return;
            }
        };
        // The watcher verified the signature; this check is the authority
        // tier: only the enrolled operator commands the estate. Parsing
        // before the tier check lets the refusal audit name the attempted
        // operation.
        if msg.sender_id != self.operator_id {
            self.refuse(
                Some(msg.sender_id),
                op.verb(),
                Some(op.name().to_owned()),
                "not_operator",
                at,
            );
            return;
        }
        match op {
            EstateOp::OpenEngagement { .. }
            | EstateOp::CloseEngagement { .. }
            | EstateOp::ReopenEngagement { .. } => self.execute(&op, msg.sender_id, at),
            EstateOp::FormTeam { .. }
            | EstateOp::DissolveTeam { .. }
            | EstateOp::MintAgent { .. }
            | EstateOp::RetireAgent { .. } => {
                let Some(deps) = self.team_ops.clone() else {
                    audit_estate_refusal(
                        &self.audit,
                        Some(msg.sender_id),
                        op.verb(),
                        Some(op.name().to_owned()),
                        "team_ops_unwired",
                        at,
                    );
                    return;
                };
                let audit = Arc::clone(&self.audit);
                let sender_id = msg.sender_id;
                ctx.spawn(actix::fut::wrap_future(execute_team_op(
                    deps, audit, sender_id, op,
                )));
            }
        }
    }
}

// ── Team / agent operations ───────────────────────────────────────────────────

fn audit_append(audit: &AuditLog, event: &AuditEvent) {
    if let Err(err) = audit.append(event) {
        warn!(err = %err, "estate coordinator failed to append audit event");
    }
}

/// Cap on the `reason` field of an `estate.op_refused` audit event.
///
/// A few call sites build `reason` from arbitrary downstream error text
/// (e.g. a mint failure's message), unlike the short machine-readable
/// tokens used everywhere else. `AuditLog::append` relies on the whole
/// serialized event staying under `PIPE_BUF` (4096 bytes) for `O_APPEND`
/// atomicity; an unbounded `reason` could blow that budget and tear a
/// concurrent writer's line. Truncated well below `PIPE_BUF` to leave room
/// for the event's other fields.
const MAX_REFUSAL_REASON_BYTES: usize = 256;

#[expect(
    clippy::too_many_arguments,
    reason = "parameters mirror the EstateOpRefused audit event one-to-one"
)]
fn audit_estate_refusal(
    audit: &AuditLog,
    sender_id: Option<reeve_types::IdentityId>,
    op: &str,
    name: Option<String>,
    reason: &str,
    at: OffsetDateTime,
) {
    let reason = truncate_at_char_boundary(reason, MAX_REFUSAL_REASON_BYTES);
    warn!(op, ?name, reason, "estate operation refused");
    audit_append(
        audit,
        &AuditEvent::EstateOpRefused {
            sender_id,
            op: op.to_owned(),
            name,
            reason: reason.to_owned(),
            at,
        },
    );
}

fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// One-shot capture actor bridging a [`SpawnResponse`] recipient to an
/// awaitable channel. `Option::take` semantics: the first response
/// delivers, anything after is dropped.
struct MintCapture {
    tx: Option<tokio::sync::oneshot::Sender<SpawnResponse>>,
}

impl Actor for MintCapture {
    type Context = Context<Self>;
}

impl Handler<SpawnResponse> for MintCapture {
    type Result = ();

    fn handle(&mut self, msg: SpawnResponse, ctx: &mut Context<Self>) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(msg);
        }
        ctx.stop();
    }
}

/// Stop a [`MintCapture`] that will never receive a reply — sent when
/// `mint_one` gives up waiting so the actor doesn't linger forever on a
/// `SpawnResponse` that may arrive late or never.
struct StopCapture;

impl actix::Message for StopCapture {
    type Result = ();
}

impl Handler<StopCapture> for MintCapture {
    type Result = ();

    fn handle(&mut self, _msg: StopCapture, ctx: &mut Context<Self>) {
        ctx.stop();
    }
}

/// Mint one durable named agent through the spawn path. The spawn
/// coordinator owns provisioning, identity, tools, and the name-permanence
/// check; this just requests and awaits.
async fn mint_one(
    deps: &TeamOpsDeps,
    operator_id: reeve_types::IdentityId,
    persona: &str,
    requested_name: crate::agent_registry::ValidatedAgentName,
) -> Result<(), String> {
    let params = SpawnRequest::validate(persona, "", operator_id)
        .map_err(|e| format!("invalid mint request: {e}"))?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let capture_addr = MintCapture { tx: Some(tx) }.start();
    deps.spawner.do_send(
        SpawnRequest::new(params, capture_addr.clone().recipient())
            .with_requested_name(requested_name),
    );
    match tokio::time::timeout(MINT_TIMEOUT, rx).await {
        Ok(Ok(SpawnResponse::Success { .. })) => Ok(()),
        Ok(Ok(SpawnResponse::Failure { message })) => Err(message),
        Ok(Err(_)) => Err("mint reply channel dropped".to_owned()),
        Err(_) => {
            // The reply may still arrive after this point (do_send is
            // fire-and-forget and the spawn may complete late); the actor
            // just drops it silently since `tx` is dropped along with this
            // scope. Stop it now instead of waiting for that arrival.
            capture_addr.do_send(StopCapture);
            Err(format!("mint timed out after {MINT_TIMEOUT:?}"))
        }
    }
}

/// Wind down a running incarnation via its control route, or mark the
/// registry record `Retired` directly when no incarnation is live. When a
/// route exists the agent itself lands the record on `Retired` at the end
/// of its drain.
fn retire_identity(deps: &TeamOpsDeps, name: &str) {
    let route = deps.control_routes.unregister(name);
    if let Some(route) = route {
        route.do_send(Retire);
        return;
    }
    match AgentRegistry::open(deps.agent_registry_path.clone()) {
        Ok(mut registry) => {
            if let Err(err) = registry.update_status(name, AgentStatus::Retired) {
                warn!(name, err = %err, "failed to mark agent retired in registry");
            }
        }
        Err(err) => warn!(err = %err, "failed to open agent registry for retirement"),
    }
}

fn member_agent_name(team: &str, role: &str, index: u32, count: u32) -> String {
    if count == 1 {
        format!("{team}-{role}")
    } else {
        format!("{team}-{role}-{}", index + 1)
    }
}

async fn execute_team_op(
    deps: Arc<TeamOpsDeps>,
    audit: Arc<AuditLog>,
    sender_id: reeve_types::IdentityId,
    op: EstateOp,
) {
    let at = OffsetDateTime::now_utc();
    match &op {
        EstateOp::FormTeam { name, template } => {
            form_team(&deps, &audit, sender_id, name, template, at).await;
        }
        EstateOp::DissolveTeam { name, dispositions } => {
            dissolve_team(&deps, &audit, sender_id, name, dispositions, at);
        }
        EstateOp::MintAgent { name, persona } => {
            let Ok(validated) = crate::agent_registry::ValidatedAgentName::new(name) else {
                audit_estate_refusal(
                    &audit,
                    Some(sender_id),
                    op.verb(),
                    Some(name.clone()),
                    "invalid_name",
                    at,
                );
                return;
            };
            match mint_one(&deps, sender_id, persona, validated).await {
                Ok(()) => {
                    info!(name, persona, "agent minted");
                    audit_append(
                        &audit,
                        &AuditEvent::AgentMinted {
                            sender_id,
                            name: name.clone(),
                            persona_name: persona.clone(),
                            team: None,
                            at,
                        },
                    );
                }
                Err(message) => audit_estate_refusal(
                    &audit,
                    Some(sender_id),
                    op.verb(),
                    Some(name.clone()),
                    &format!("mint_failed: {message}"),
                    at,
                ),
            }
        }
        EstateOp::RetireAgent { name } => retire_teamless_agent(&deps, &audit, sender_id, name, at),
        EstateOp::OpenEngagement { .. }
        | EstateOp::CloseEngagement { .. }
        | EstateOp::ReopenEngagement { .. } => {
            unreachable!("engagement ops are handled synchronously")
        }
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "parameters mirror the operation payload plus the shared audit \
              handles; a bundling struct would add indirection at one call site"
)]
#[expect(
    clippy::too_many_lines,
    reason = "linear guard-then-mint sequence; splitting on line count would \
              fragment the partial-failure posture"
)]
pub(crate) async fn form_team(
    deps: &TeamOpsDeps,
    audit: &AuditLog,
    sender_id: reeve_types::IdentityId,
    name: &str,
    template: &str,
    at: OffsetDateTime,
) {
    use crate::team::TeamError;
    let refuse = |reason: &str| {
        audit_estate_refusal(
            audit,
            Some(sender_id),
            "form-team",
            Some(name.to_owned()),
            reason,
            at,
        );
    };
    match deps.teams.get(name) {
        Ok(_) => {
            refuse("name_taken");
            return;
        }
        Err(TeamError::NotFound { .. }) => {}
        Err(err) => {
            warn!(err = %err, "roster lookup failed during form-team");
            refuse("roster_error");
            return;
        }
    }
    if crate::agent_fs::validate_agent_name(template).is_err() {
        refuse("invalid_template");
        return;
    }
    let layout = crate::agent_fs::RuntimeLayout::new(&deps.data_dir);
    let template_cfg = match crate::config::load_team_config(&layout.team_config_path(template)) {
        Ok(cfg) => cfg,
        Err(err) => {
            warn!(err = %err, template, "failed to load team template");
            refuse("template_not_found");
            return;
        }
    };
    let mut members = Vec::new();
    for member in &template_cfg.members {
        for index in 0..member.count {
            let agent_name = member_agent_name(name, &member.role_label, index, member.count);
            let Ok(validated) = crate::agent_registry::ValidatedAgentName::new(&agent_name) else {
                refuse(&format!("invalid_member_name: {agent_name}"));
                return;
            };
            if let Err(message) = mint_one(deps, sender_id, &member.persona_name, validated).await {
                // Members minted before the failure remain as teamless
                // standing agents — minting is not transactional; the
                // audit trail shows exactly what exists.
                refuse(&format!("mint_failed: {agent_name}: {message}"));
                return;
            }
            audit_append(
                audit,
                &AuditEvent::AgentMinted {
                    sender_id,
                    name: agent_name.clone(),
                    persona_name: member.persona_name.clone(),
                    team: Some(name.to_owned()),
                    at,
                },
            );
            members.push(TeamMemberRecord {
                agent_name,
                role_label: member.role_label.clone(),
                persona_name: member.persona_name.clone(),
            });
        }
    }
    let member_names: Vec<String> = members.iter().map(|m| m.agent_name.clone()).collect();
    let record = TeamRecord {
        name: name.to_owned(),
        template_name: template.to_owned(),
        lead_role: template_cfg.lead_role.clone(),
        members,
        state: TeamState::Formed,
        formed_at: at,
        dispositions: BTreeMap::new(),
    };
    if let Err(err) = deps.teams.form(&record) {
        warn!(err = %err, "failed to write roster after minting members");
        refuse("roster_write_failed");
        return;
    }
    info!(name, members = ?member_names, "team formed");
    audit_append(
        audit,
        &AuditEvent::TeamFormed {
            sender_id,
            name: name.to_owned(),
            members: member_names,
            at,
        },
    );
}

#[expect(
    clippy::too_many_arguments,
    reason = "parameters mirror the operation payload plus the shared audit \
              handles; a bundling struct would add indirection at one call site"
)]
fn dissolve_team(
    deps: &TeamOpsDeps,
    audit: &AuditLog,
    sender_id: reeve_types::IdentityId,
    name: &str,
    dispositions: &BTreeMap<String, MemberDisposition>,
    at: OffsetDateTime,
) {
    use crate::team::TeamError;
    let refuse = |reason: &str| {
        audit_estate_refusal(
            audit,
            Some(sender_id),
            "dissolve-team",
            Some(name.to_owned()),
            reason,
            at,
        );
    };
    let record = match deps.teams.get(name) {
        Ok(r) => r,
        Err(TeamError::NotFound { .. }) => {
            refuse("not_found");
            return;
        }
        Err(err) => {
            warn!(err = %err, "roster lookup failed during dissolve-team");
            refuse("roster_error");
            return;
        }
    };
    if record.state != TeamState::Formed {
        refuse("wrong_state");
        return;
    }
    let mut effective = BTreeMap::new();
    for member in &record.members {
        let disposition = dispositions
            .get(&member.agent_name)
            .copied()
            .unwrap_or(MemberDisposition::Retired);
        match disposition {
            MemberDisposition::Retired => {
                retire_identity(deps, &member.agent_name);
                audit_append(
                    audit,
                    &AuditEvent::AgentRetired {
                        sender_id,
                        name: member.agent_name.clone(),
                        at,
                    },
                );
            }
            MemberDisposition::Released => {
                audit_append(
                    audit,
                    &AuditEvent::AgentReleased {
                        sender_id,
                        name: member.agent_name.clone(),
                        team: name.to_owned(),
                        at,
                    },
                );
            }
        }
        effective.insert(member.agent_name.clone(), disposition);
    }
    if let Err(err) = deps.teams.dissolve(name, effective) {
        warn!(err = %err, "failed to write dissolved roster");
        refuse("roster_write_failed");
        return;
    }
    info!(name, "team dissolved");
    audit_append(
        audit,
        &AuditEvent::TeamDissolved {
            sender_id,
            name: name.to_owned(),
            at,
        },
    );
}

fn retire_teamless_agent(
    deps: &TeamOpsDeps,
    audit: &AuditLog,
    sender_id: reeve_types::IdentityId,
    name: &str,
    at: OffsetDateTime,
) {
    let refuse = |reason: &str| {
        audit_estate_refusal(
            audit,
            Some(sender_id),
            "retire-agent",
            Some(name.to_owned()),
            reason,
            at,
        );
    };
    if name == ESTATE_AGENT_NAME {
        refuse("reserved");
        return;
    }
    // Standing-team members are retired through dissolution so the roster
    // and the registry never disagree about who serves.
    match deps.teams.list() {
        Ok(rosters) => {
            let serving = rosters.iter().any(|r| {
                r.state == TeamState::Formed && r.members.iter().any(|m| m.agent_name == name)
            });
            if serving {
                refuse("team_member");
                return;
            }
        }
        Err(err) => {
            warn!(err = %err, "roster scan failed during retire-agent");
            refuse("roster_error");
            return;
        }
    }
    match AgentRegistry::open(deps.agent_registry_path.clone()) {
        Ok(registry) => match registry.lookup(name) {
            None => {
                refuse("not_found");
                return;
            }
            Some(record) if matches!(record.status, AgentStatus::Retired) => {
                refuse("wrong_state");
                return;
            }
            Some(_) => {}
        },
        Err(err) => {
            warn!(err = %err, "agent registry open failed during retire-agent");
            refuse("registry_error");
            return;
        }
    }
    retire_identity(deps, name);
    info!(name, "agent retired");
    audit_append(
        audit,
        &AuditEvent::AgentRetired {
            sender_id,
            name: name.to_owned(),
            at,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use crate::engagement::EngagementState;
    use crate::test_support::secure_dir;

    fn read_audit_kinds(data_dir: &Path) -> Vec<(String, serde_json::Value)> {
        let path = crate::audit::audit_log_path(data_dir);
        let body = fs::read_to_string(path).unwrap_or_default();
        body.lines()
            .map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                (v["kind"].as_str().unwrap().to_owned(), v)
            })
            .collect()
    }

    struct TestEstate {
        data_dir: tempfile::TempDir,
        operator_id: reeve_types::IdentityId,
        addr: actix::Addr<EstateCoordinator>,
    }

    fn start_estate() -> TestEstate {
        let data_dir = secure_dir();
        let operator_id = reeve_types::IdentityId::new().unwrap();
        let engagements = EngagementRegistry::open(data_dir.path().join("engagements")).unwrap();
        let audit = Arc::new(AuditLog::open(data_dir.path().to_path_buf()).unwrap());
        let addr = EstateCoordinator::new(operator_id, engagements, audit).start();
        TestEstate {
            data_dir,
            operator_id,
            addr,
        }
    }

    fn inbound(sender_id: reeve_types::IdentityId, op: &EstateOp) -> ProcessInbound {
        ProcessInbound {
            payload: serde_json::to_string(op).unwrap(),
            message_id: "test-message".to_owned(),
            sender_id,
        }
    }

    async fn send(estate: &TestEstate, sender: reeve_types::IdentityId, op: &EstateOp) {
        tokio::time::timeout(
            Duration::from_secs(5),
            estate.addr.send(inbound(sender, op)),
        )
        .await
        .expect("estate handler timed out")
        .expect("estate mailbox closed");
    }

    #[test]
    fn operator_open_close_reopen_round_trip_with_audit() {
        actix::System::new().block_on(async {
            let estate = start_estate();
            let root = estate.data_dir.path().join("work");
            fs::create_dir_all(&root).unwrap();
            let open = EstateOp::OpenEngagement {
                name: "reconciler".to_owned(),
                purpose: "modernize".to_owned(),
                root: Some(root.clone()),
            };
            send(&estate, estate.operator_id, &open).await;
            send(
                &estate,
                estate.operator_id,
                &EstateOp::CloseEngagement {
                    name: "reconciler".to_owned(),
                },
            )
            .await;
            send(
                &estate,
                estate.operator_id,
                &EstateOp::ReopenEngagement {
                    name: "reconciler".to_owned(),
                },
            )
            .await;

            let registry =
                EngagementRegistry::open(estate.data_dir.path().join("engagements")).unwrap();
            let record = registry.get("reconciler").unwrap();
            assert_eq!(record.state, EngagementState::Open);
            assert_eq!(record.root, Some(fs::canonicalize(&root).unwrap()));

            let kinds: Vec<String> = read_audit_kinds(estate.data_dir.path())
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            assert_eq!(
                kinds,
                vec![
                    "engagement.opened",
                    "engagement.closed",
                    "engagement.reopened"
                ],
            );
        });
    }

    #[test]
    fn non_operator_sender_is_refused_and_audited() {
        actix::System::new().block_on(async {
            let estate = start_estate();
            let stranger = reeve_types::IdentityId::new().unwrap();
            let op = EstateOp::OpenEngagement {
                name: "sneaky".to_owned(),
                purpose: "p".to_owned(),
                root: None,
            };
            send(&estate, stranger, &op).await;

            let registry =
                EngagementRegistry::open(estate.data_dir.path().join("engagements")).unwrap();
            assert!(matches!(
                registry.get("sneaky").unwrap_err(),
                EngagementError::NotFound { .. }
            ));
            let events = read_audit_kinds(estate.data_dir.path());
            assert_eq!(events.len(), 1);
            let (kind, v) = &events[0];
            assert_eq!(kind, "engagement.op_refused");
            assert_eq!(v["reason"], "not_operator");
            assert_eq!(v["op"], "open-engagement");
            assert_eq!(v["name"], "sneaky");
        });
    }

    #[test]
    fn invalid_payload_is_refused_and_audited() {
        actix::System::new().block_on(async {
            let estate = start_estate();
            tokio::time::timeout(
                Duration::from_secs(5),
                estate.addr.send(ProcessInbound {
                    payload: "not json at all".to_owned(),
                    message_id: "m".to_owned(),
                    sender_id: estate.operator_id,
                }),
            )
            .await
            .expect("estate handler timed out")
            .expect("estate mailbox closed");

            let events = read_audit_kinds(estate.data_dir.path());
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].0, "engagement.op_refused");
            assert_eq!(events[0].1["reason"], "invalid_payload");
        });
    }

    #[test]
    fn reused_name_is_refused_with_name_taken() {
        actix::System::new().block_on(async {
            let estate = start_estate();
            let open = |name: &str| EstateOp::OpenEngagement {
                name: name.to_owned(),
                purpose: "p".to_owned(),
                root: None,
            };
            send(&estate, estate.operator_id, &open("once")).await;
            send(
                &estate,
                estate.operator_id,
                &EstateOp::CloseEngagement {
                    name: "once".to_owned(),
                },
            )
            .await;
            send(&estate, estate.operator_id, &open("once")).await;

            let events = read_audit_kinds(estate.data_dir.path());
            let last = events.last().unwrap();
            assert_eq!(last.0, "engagement.op_refused");
            assert_eq!(last.1["reason"], "name_taken");
        });
    }

    #[test]
    fn unresolvable_root_is_refused() {
        actix::System::new().block_on(async {
            let estate = start_estate();
            let op = EstateOp::OpenEngagement {
                name: "ghost".to_owned(),
                purpose: "p".to_owned(),
                root: Some(estate.data_dir.path().join("does-not-exist")),
            };
            send(&estate, estate.operator_id, &op).await;

            let events = read_audit_kinds(estate.data_dir.path());
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].1["reason"], "root_unresolvable");
        });
    }

    // ── Team / agent operation tests ─────────────────────────────────────────

    use crate::config::DEFAULT_MAX_SYSTEM_PROMPT_BYTES;
    use crate::spawn_coordinator::SpawnCoordinator;
    use crate::test_support::{
        build_registries, write_full_access_persona_profile, write_persona_config, MockAdapter,
        NullDispatcher, NullInboxStarter,
    };

    const TEST_TEMPLATE: &str = r#"
name = "default"
version = 1
lead_role = "lead"

[[members]]
persona_name = "worker"
persona_version = 1
count = 1
role_label = "lead"

[[members]]
persona_name = "worker"
persona_version = 1
count = 2
role_label = "helper"
"#;

    struct TeamBed {
        data_dir: tempfile::TempDir,
        operator_id: reeve_types::IdentityId,
        registry_path: PathBuf,
        addr: actix::Addr<EstateCoordinator>,
    }

    /// Full stack: real spawn coordinator (mock adapter), real registries,
    /// estate coordinator with team ops wired. Must run inside an actix
    /// System.
    fn start_team_bed() -> TeamBed {
        let data_dir = secure_dir();
        let root = data_dir.path();
        write_persona_config(root, "worker", "claude-opus-4-7");
        write_full_access_persona_profile(root, "worker");
        fs::create_dir_all(root.join("teams")).unwrap();
        fs::write(root.join("teams").join("default.toml"), TEST_TEMPLATE).unwrap();

        let (identity_registry, watcher, registry_path) = build_registries(root);
        let operator_id = reeve_types::IdentityId::new().unwrap();
        let audit = Arc::new(AuditLog::open(root.to_path_buf()).unwrap());
        let control_routes = ControlRoutes::default();

        let adapters: Vec<Arc<dyn reeve_adapter::Adapter>> = vec![Arc::new(MockAdapter::new(
            "claude-opus-4-7@anthropic-direct",
        ))];
        let coordinator = SpawnCoordinator::new(
            root.to_path_buf(),
            registry_path.clone(),
            identity_registry,
            adapters,
            Arc::clone(&audit),
            watcher,
            NullInboxStarter.start().recipient(),
            NullDispatcher.start().recipient(),
            None,
            operator_id,
            DEFAULT_MAX_SYSTEM_PROMPT_BYTES,
        )
        .with_control_routes(control_routes.clone());
        let coord_addr = coordinator.start();

        let engagements = EngagementRegistry::open(root.join("engagements")).unwrap();
        let teams = TeamRegistry::open(root.join("rosters")).unwrap();
        let deps = TeamOpsDeps {
            spawner: coord_addr.recipient(),
            teams,
            control_routes,
            agent_registry_path: registry_path.clone(),
            data_dir: root.to_path_buf(),
        };
        let addr = EstateCoordinator::new(operator_id, engagements, Arc::clone(&audit))
            .with_team_ops(deps)
            .start();
        TeamBed {
            data_dir,
            operator_id,
            registry_path,
            addr,
        }
    }

    /// Poll `condition` until it holds or a 10-second deadline passes.
    /// Team ops resolve asynchronously (ctx.spawn inside the coordinator),
    /// so effects are awaited through the durable stores, never assumed.
    async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn agent_status(registry_path: &Path, name: &str) -> Option<AgentStatus> {
        AgentRegistry::open(registry_path.to_path_buf())
            .ok()
            .and_then(|r| r.lookup(name).map(|rec| rec.status))
    }

    fn audit_has(data_dir: &Path, kind: &str, pred: impl Fn(&serde_json::Value) -> bool) -> bool {
        read_audit_kinds(data_dir)
            .iter()
            .any(|(k, v)| k == kind && pred(v))
    }

    async fn send_op(bed: &TeamBed, op: &EstateOp) {
        tokio::time::timeout(
            Duration::from_secs(5),
            bed.addr.send(ProcessInbound {
                payload: serde_json::to_string(op).unwrap(),
                message_id: "team-test".to_owned(),
                sender_id: bed.operator_id,
            }),
        )
        .await
        .expect("estate handler timed out")
        .expect("estate mailbox closed");
    }

    #[test]
    fn form_team_mints_members_and_writes_roster() {
        actix::System::new().block_on(async {
            let bed = start_team_bed();
            send_op(
                &bed,
                &EstateOp::FormTeam {
                    name: "default".to_owned(),
                    template: "default".to_owned(),
                },
            )
            .await;

            let teams = TeamRegistry::open(bed.data_dir.path().join("rosters")).unwrap();
            wait_until("roster to be written", || teams.get("default").is_ok()).await;

            let roster = teams.get("default").unwrap();
            assert_eq!(roster.state, TeamState::Formed);
            let names: Vec<&str> = roster
                .members
                .iter()
                .map(|m| m.agent_name.as_str())
                .collect();
            assert_eq!(
                names,
                vec!["default-lead", "default-helper-1", "default-helper-2"],
                "one member per (persona, count) with deterministic names"
            );
            assert_eq!(roster.lead_member_name(), Some("default-lead"));

            for name in &names {
                assert_eq!(
                    agent_status(&bed.registry_path, name),
                    Some(AgentStatus::Running),
                    "{name} must be a running durable agent"
                );
            }
            assert!(audit_has(bed.data_dir.path(), "team.formed", |v| {
                v["name"] == "default"
            }));
            assert!(audit_has(bed.data_dir.path(), "agent.minted", |v| {
                v["name"] == "default-lead" && v["team"] == "default"
            }));

            // Names are forever: re-forming the same team name is refused.
            send_op(
                &bed,
                &EstateOp::FormTeam {
                    name: "default".to_owned(),
                    template: "default".to_owned(),
                },
            )
            .await;
            wait_until("re-form refusal audit", || {
                audit_has(bed.data_dir.path(), "estate.op_refused", |v| {
                    v["op"] == "form-team" && v["reason"] == "name_taken"
                })
            })
            .await;
        });
    }

    #[test]
    fn mint_retire_round_trip_and_name_permanence() {
        actix::System::new().block_on(async {
            let bed = start_team_bed();
            send_op(
                &bed,
                &EstateOp::MintAgent {
                    name: "librarian".to_owned(),
                    persona: "worker".to_owned(),
                },
            )
            .await;
            wait_until("librarian to be running", || {
                agent_status(&bed.registry_path, "librarian") == Some(AgentStatus::Running)
            })
            .await;
            assert!(audit_has(bed.data_dir.path(), "agent.minted", |v| {
                v["name"] == "librarian" && v.get("team").is_none_or(serde_json::Value::is_null)
            }));

            send_op(
                &bed,
                &EstateOp::RetireAgent {
                    name: "librarian".to_owned(),
                },
            )
            .await;
            wait_until("librarian to be retired", || {
                agent_status(&bed.registry_path, "librarian") == Some(AgentStatus::Retired)
            })
            .await;
            assert!(audit_has(bed.data_dir.path(), "agent.retired", |v| {
                v["name"] == "librarian"
            }));

            // The name is permanently unavailable, even after retirement.
            send_op(
                &bed,
                &EstateOp::MintAgent {
                    name: "librarian".to_owned(),
                    persona: "worker".to_owned(),
                },
            )
            .await;
            wait_until("re-mint refusal audit", || {
                audit_has(bed.data_dir.path(), "estate.op_refused", |v| {
                    v["op"] == "mint-agent"
                        && v["reason"]
                            .as_str()
                            .is_some_and(|r| r.starts_with("mint_failed"))
                })
            })
            .await;
        });
    }

    #[test]
    fn retire_agent_refuses_team_member_and_reserved_identity() {
        actix::System::new().block_on(async {
            let bed = start_team_bed();
            send_op(
                &bed,
                &EstateOp::FormTeam {
                    name: "crew".to_owned(),
                    template: "default".to_owned(),
                },
            )
            .await;
            let teams = TeamRegistry::open(bed.data_dir.path().join("rosters")).unwrap();
            wait_until("crew roster", || teams.get("crew").is_ok()).await;

            // Guard: a standing-team member cannot be retired individually.
            send_op(
                &bed,
                &EstateOp::RetireAgent {
                    name: "crew-helper-1".to_owned(),
                },
            )
            .await;
            wait_until("team-member retire refusal", || {
                audit_has(bed.data_dir.path(), "estate.op_refused", |v| {
                    v["op"] == "retire-agent" && v["reason"] == "team_member"
                })
            })
            .await;

            // Guard: the estate coordinator itself is reserved.
            send_op(
                &bed,
                &EstateOp::RetireAgent {
                    name: ESTATE_AGENT_NAME.to_owned(),
                },
            )
            .await;
            wait_until("reserved retire refusal", || {
                audit_has(bed.data_dir.path(), "estate.op_refused", |v| {
                    v["op"] == "retire-agent" && v["reason"] == "reserved"
                })
            })
            .await;
        });
    }

    #[test]
    fn dissolve_applies_mixed_dispositions_and_retire_guards_hold() {
        actix::System::new().block_on(async {
            let bed = start_team_bed();
            send_op(
                &bed,
                &EstateOp::FormTeam {
                    name: "crew".to_owned(),
                    template: "default".to_owned(),
                },
            )
            .await;
            let teams = TeamRegistry::open(bed.data_dir.path().join("rosters")).unwrap();
            wait_until("crew roster", || teams.get("crew").is_ok()).await;

            let mut dispositions = BTreeMap::new();
            dispositions.insert("crew-helper-1".to_owned(), MemberDisposition::Released);
            send_op(
                &bed,
                &EstateOp::DissolveTeam {
                    name: "crew".to_owned(),
                    dispositions,
                },
            )
            .await;

            wait_until("roster dissolved", || {
                teams
                    .get("crew")
                    .is_ok_and(|r| r.state == TeamState::Dissolved)
            })
            .await;
            // Unspecified members default to retired; the released member
            // keeps running as a teamless standing agent.
            wait_until("crew-lead retired", || {
                agent_status(&bed.registry_path, "crew-lead") == Some(AgentStatus::Retired)
            })
            .await;
            wait_until("crew-helper-2 retired", || {
                agent_status(&bed.registry_path, "crew-helper-2") == Some(AgentStatus::Retired)
            })
            .await;
            assert_eq!(
                agent_status(&bed.registry_path, "crew-helper-1"),
                Some(AgentStatus::Running),
                "released member keeps running"
            );
            assert!(audit_has(bed.data_dir.path(), "agent.released", |v| {
                v["name"] == "crew-helper-1" && v["team"] == "crew"
            }));
            assert!(audit_has(bed.data_dir.path(), "team.dissolved", |v| {
                v["name"] == "crew"
            }));

            // Released agent is now teamless and retirable.
            send_op(
                &bed,
                &EstateOp::RetireAgent {
                    name: "crew-helper-1".to_owned(),
                },
            )
            .await;
            wait_until("released member retired", || {
                agent_status(&bed.registry_path, "crew-helper-1") == Some(AgentStatus::Retired)
            })
            .await;
        });
    }
}
