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
//! The coordinator occupies the reserved agent name `estate` in the agent
//! registry so name→(identity, inbox) resolution works with the same lookup
//! every other sender uses. It is not a model-backed agent: the daemon's
//! resume pass skips it, and it never makes model calls.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use actix::{Actor, ActorContext as _, AsyncContext as _, Context, Handler, Supervised};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::{info, warn};

use crate::agent::{
    ControlRoutes, PrepareReincarnation, ProcessInbound, ReincarnationReady, Retire,
};
use crate::agent_fs::{AgentDirs, RuntimeLayout};
use crate::agent_registry::{generate_or_load_keypair, AgentRegistry, AgentStatus};
use crate::audit::{AuditEvent, AuditLog};
use crate::capability::load_capability_profile;
use crate::dispatcher::SendMessage;
use crate::engagement::{EngagementError, EngagementRegistry, EngagementState, StaffedUnit};
use crate::identity_registry::IdentityRegistry;
use crate::model_resolution::{write_spawn_snapshot, SpawnSnapshot};
use crate::spawn_coordinator::{launch_incarnation, SpawnRequest, SpawnResponse};
use crate::supervisor::WatchInbox;
use crate::team::{MemberDisposition, TeamMemberRecord, TeamRecord, TeamRegistry, TeamState};
use crate::tool::BlacklistHandle;
use crate::watcher::Watcher;

/// Reserved agent-registry name for the estate coordinator.
pub const ESTATE_AGENT_NAME: &str = "estate";

/// Runtime collaborators for the estate coordinator's async-dispatched
/// operations: team formation/dissolution, agent mint/retire, and staffing.
/// Bundled so the engagement-only construction (and its tests) stays
/// untouched; the daemon always wires this.
///
/// Carries its own [`EngagementRegistry`] handle, a second instance from
/// the one `EstateCoordinator` holds privately for the synchronous
/// engagement-op bucket — safe because the store is stateless between
/// calls (every operation reads and writes the record file directly), so
/// two instances rooted at the same path never desync.
#[derive(Clone)]
pub struct EstateOpsDeps {
    /// Spawn path used to mint member agents with requested names.
    pub spawner: actix::Recipient<SpawnRequest>,
    pub teams: TeamRegistry,
    pub engagements: EngagementRegistry,
    pub control_routes: ControlRoutes,
    pub agent_registry_path: PathBuf,
    /// Data root, for loading team templates.
    pub data_dir: PathBuf,
    /// Collaborators below this line exist only for staffing's runtime
    /// reincarnation path (`spawn_coordinator::launch_incarnation`) — team
    /// formation and mint/retire don't need them, since minting goes
    /// through `spawner` instead.
    pub identity_registry: Arc<IdentityRegistry>,
    pub adapters: Vec<Arc<dyn reeve_adapter::Adapter>>,
    pub watcher: Arc<Watcher>,
    pub inbox_starter: actix::Recipient<WatchInbox>,
    pub dispatcher: actix::Recipient<SendMessage>,
    pub blacklist: Option<BlacklistHandle>,
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
    /// Staff a standing team to a top-level engagement: each member winds
    /// down and re-incarnates with the engagement's context.
    StaffTeam { engagement: String, team: String },
    /// Staff a lone teamless agent to a top-level engagement — the
    /// degenerate unit of one.
    StaffAgent { engagement: String, agent: String },
    /// Recall whatever unit is staffed to an engagement: every member winds
    /// down and re-incarnates rootless.
    Unstaff { engagement: String },
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
            Self::StaffTeam { .. } => "staff-team",
            Self::StaffAgent { .. } => "staff-agent",
            Self::Unstaff { .. } => "unstaff",
        }
    }

    /// The engagement/team/agent name the operation targets. For staffing
    /// ops this is the engagement — the record the operation mutates —
    /// matching `OpenEngagement`/`CloseEngagement`/`ReopenEngagement`.
    pub fn name(&self) -> &str {
        match self {
            Self::OpenEngagement { name, .. }
            | Self::CloseEngagement { name }
            | Self::ReopenEngagement { name }
            | Self::FormTeam { name, .. }
            | Self::DissolveTeam { name, .. }
            | Self::MintAgent { name, .. }
            | Self::RetireAgent { name } => name,
            Self::StaffTeam { engagement, .. }
            | Self::StaffAgent { engagement, .. }
            | Self::Unstaff { engagement } => engagement,
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
    team_ops: Option<Arc<EstateOpsDeps>>,
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
    pub fn with_team_ops(mut self, deps: EstateOpsDeps) -> Self {
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
            EstateOp::CloseEngagement { name } => {
                // The engagement outlives any staffing (spec § Engagement):
                // closing a still-staffed engagement would leave a unit
                // pointed at a closed record. Require an explicit unstaff
                // first rather than silently cascading one.
                match self.engagements.get(name) {
                    Ok(record) if record.staffed_unit.is_some() => {
                        self.refuse(
                            Some(sender_id),
                            op.verb(),
                            Some(name.clone()),
                            "staffed",
                            at,
                        );
                        return;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        self.refuse(
                            Some(sender_id),
                            op.verb(),
                            Some(name.clone()),
                            refusal_reason(&err),
                            at,
                        );
                        return;
                    }
                }
                match self.engagements.close(name) {
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
                }
            }
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
            | EstateOp::RetireAgent { .. }
            | EstateOp::StaffTeam { .. }
            | EstateOp::StaffAgent { .. }
            | EstateOp::Unstaff { .. } => {
                unreachable!("team and staffing ops are dispatched to execute_team_op, not execute")
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
            | EstateOp::RetireAgent { .. }
            | EstateOp::StaffTeam { .. }
            | EstateOp::StaffAgent { .. }
            | EstateOp::Unstaff { .. } => {
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
    deps: &EstateOpsDeps,
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
fn retire_identity(deps: &EstateOpsDeps, name: &str) {
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

// ── Staffing ───────────────────────────────────────────────────────────────

/// One-shot capture actor bridging a [`ReincarnationReady`] recipient to an
/// awaitable channel — the same shape as [`MintCapture`], for the
/// wind-down-then-relaunch reincarnation path.
struct WindDownCapture {
    tx: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Actor for WindDownCapture {
    type Context = Context<Self>;
}

impl Handler<ReincarnationReady> for WindDownCapture {
    type Result = ();

    fn handle(&mut self, _msg: ReincarnationReady, ctx: &mut Context<Self>) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(());
        }
        ctx.stop();
    }
}

impl Handler<StopCapture> for WindDownCapture {
    type Result = ();

    fn handle(&mut self, _msg: StopCapture, ctx: &mut Context<Self>) {
        ctx.stop();
    }
}

/// The engagement side of a staff precondition: must exist, be `Open`, and
/// carry no staffed unit yet (strict 1:1-at-a-time). Returns the
/// engagement's root on success.
fn check_engagement_available(
    deps: &EstateOpsDeps,
    engagement: &str,
) -> Result<Option<PathBuf>, &'static str> {
    let record = deps.engagements.get(engagement).map_err(|err| match err {
        EngagementError::NotFound { .. } => "engagement_not_found",
        EngagementError::InvalidName { .. }
        | EngagementError::NameTaken { .. }
        | EngagementError::WrongState { .. }
        | EngagementError::RelativeRoot { .. }
        | EngagementError::Io { .. }
        | EngagementError::Toml { .. } => "engagement_error",
    })?;
    if record.state != EngagementState::Open {
        return Err("engagement_not_open");
    }
    if record.staffed_unit.is_some() {
        return Err("engagement_already_staffed");
    }
    Ok(record.root)
}

/// The unit side of a staff precondition: is this team/agent already
/// staffed to some *other* open engagement? Staffing state lives only on
/// the engagement record (§ `EstateOpsDeps` doc), so this scans rather
/// than following a second pointer that could drift out of sync.
fn unit_already_staffed(deps: &EstateOpsDeps, unit: &StaffedUnit) -> Result<bool, String> {
    let all = deps.engagements.list().map_err(|e| e.to_string())?;
    Ok(all
        .iter()
        .any(|e| e.state == EngagementState::Open && e.staffed_unit.as_ref() == Some(unit)))
}

/// Wind down one member's live incarnation (if any) and restart it with a
/// new snapshot carrying `engagement`/`root` — `None`/`None` to unstaff
/// (rootless). The new snapshot is written to disk *before* the wind-down
/// starts, so a daemon crash mid-reincarnation self-heals on the next
/// boot's resume pass (which reads whatever `agent.toml` currently holds)
/// instead of needing special recovery.
///
/// Unlike `daemon::resume_one_subagent`, this does not verify the on-disk
/// keypair against the identity registry: this identity was live and
/// trusted moments ago, not crossing the disk-could-have-been-tampered
/// boundary a daemon restart crosses.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is a distinct collaborator or datum the wind-down/relaunch/audit \
              sequence needs (deps, audit, sender identity, target agent, new engagement \
              context, timestamp); bundling them into a struct would just move the same count \
              into a constructor"
)]
#[expect(
    clippy::too_many_lines,
    reason = "one linear sequence — snapshot rewrite, wind-down, relaunch, audit — each step \
              depends on the previous one's output, so splitting it would scatter a single \
              causal chain across helper functions the reader has to reassemble"
)]
async fn reincarnate_member(
    deps: &EstateOpsDeps,
    audit: &Arc<AuditLog>,
    sender_id: reeve_types::IdentityId,
    name: &str,
    engagement: Option<&str>,
    root: Option<&std::path::Path>,
    at: OffsetDateTime,
) -> Result<(), String> {
    let dirs =
        AgentDirs::open(&deps.data_dir, name).map_err(|e| format!("open agent dirs: {e}"))?;
    let record = AgentRegistry::open(deps.agent_registry_path.clone())
        .map_err(|e| format!("open agent registry: {e}"))?
        .lookup(name)
        .ok_or_else(|| "agent not found in registry".to_owned())?
        .clone();

    let snapshot_text = std::fs::read_to_string(dirs.agent_toml_path())
        .map_err(|e| format!("read agent.toml: {e}"))?;
    let mut snapshot: SpawnSnapshot =
        toml::from_str(&snapshot_text).map_err(|e| format!("parse agent.toml: {e}"))?;
    snapshot.engagement_name = engagement.map(str::to_owned);
    snapshot.working_root = root.map(std::path::Path::to_path_buf);
    write_spawn_snapshot(&dirs, &snapshot).map_err(|e| format!("write new snapshot: {e}"))?;

    if let Some(addr) = deps.control_routes.unregister(name) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let capture_addr = WindDownCapture { tx: Some(tx) }.start();
        addr.do_send(PrepareReincarnation {
            reply_to: capture_addr.clone().recipient(),
        });
        // Drop the Addr before awaiting the reply: actix::Supervisor gates
        // restart eligibility on mailbox connectivity alone (ADR 005), so a
        // live handle held across this await — even one no longer stored in
        // any table — keeps a terminating agent "connected" and Supervisor
        // restarts it in a tight loop for the full timeout below.
        drop(addr);
        match tokio::time::timeout(MINT_TIMEOUT, rx).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return Err("reincarnation reply channel dropped".to_owned()),
            Err(_) => {
                capture_addr.do_send(StopCapture);
                return Err(format!(
                    "reincarnation wind-down timed out after {MINT_TIMEOUT:?}"
                ));
            }
        }
    }

    // A concurrent RetireAgent can land while the wind-down above was
    // draining: retire_identity's control-route lookup found nothing (this
    // function already unregistered it) and wrote Retired directly to the
    // registry. Retirement is terminal — re-check before relaunching so a
    // race does not resurrect a retired identity. transition_to_stopped
    // carries the matching guard against the reverse ordering (this
    // function's Stopped write landing after a concurrent Retired one).
    let currently_retired = AgentRegistry::open(deps.agent_registry_path.clone())
        .ok()
        .and_then(|reg| reg.lookup(name).map(|r| r.status))
        == Some(AgentStatus::Retired);
    if currently_retired {
        return Err("agent was retired during wind-down; relaunch aborted".to_owned());
    }

    let keypair = generate_or_load_keypair(&dirs.identity_key_path())
        .map_err(|e| format!("load keypair: {e}"))?;
    let adapter = deps
        .adapters
        .iter()
        .find(|a| a.id() == snapshot.adapter_id)
        .ok_or_else(|| {
            format!(
                "no adapter matches snapshot adapter_id '{}'",
                snapshot.adapter_id
            )
        })?;
    let persona_name = record
        .persona_name
        .as_deref()
        .unwrap_or(&snapshot.persona_name);
    let profile = if let Ok(p) = load_capability_profile(&dirs.profile_path()) {
        Some(Arc::new(p))
    } else {
        let persona_profile_path =
            RuntimeLayout::new(&deps.data_dir).persona_profile_path(persona_name);
        load_capability_profile(&persona_profile_path)
            .ok()
            .map(Arc::new)
    };
    let system_prompt = snapshot.system_prompt.clone();

    launch_incarnation(
        Arc::clone(adapter),
        &dirs,
        snapshot,
        system_prompt,
        record.identity_id,
        keypair,
        profile,
        &deps.data_dir,
        name,
        &deps.agent_registry_path,
        &deps.watcher,
        Some(&deps.control_routes),
        Some(deps.spawner.clone()),
        &deps.dispatcher,
        deps.blacklist.as_ref(),
        &deps.inbox_starter,
        audit,
    )?;

    audit_append(
        audit,
        &AuditEvent::Reincarnated {
            sender_id,
            name: name.to_owned(),
            engagement: engagement.map(str::to_owned),
            at,
        },
    );
    Ok(())
}

/// Staff a standing team to a top-level engagement: every member winds
/// down and re-incarnates with the engagement's context. Aborts on the
/// first member that fails to reincarnate, matching `form_team`'s
/// stop-on-first-failure posture — members that already reincarnated
/// before the failure keep their new context (not rolled back; the audit
/// trail shows exactly what happened), and the engagement is not marked
/// staffed since the rotation did not complete.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors reincarnate_member's collaborators (deps, audit, sender identity, \
              timestamp) plus the two names that identify the staffing target"
)]
#[expect(
    clippy::too_many_lines,
    reason = "each precondition (roster lookup, state check, double-staffing scan, engagement \
              availability) is a distinct refusal path that must run before the member loop; \
              collapsing them into helpers would hide which check produced which audit reason"
)]
async fn staff_team(
    deps: &EstateOpsDeps,
    audit: &Arc<AuditLog>,
    sender_id: reeve_types::IdentityId,
    engagement: &str,
    team: &str,
    at: OffsetDateTime,
) {
    use crate::team::TeamError;
    let refuse = |reason: &str| {
        audit_estate_refusal(
            audit,
            Some(sender_id),
            "staff-team",
            Some(engagement.to_owned()),
            reason,
            at,
        );
    };
    let team_record = match deps.teams.get(team) {
        Ok(r) => r,
        Err(TeamError::NotFound { .. }) => {
            refuse("team_not_found");
            return;
        }
        Err(err) => {
            warn!(err = %err, "roster lookup failed during staff-team");
            refuse("roster_error");
            return;
        }
    };
    if team_record.state != TeamState::Formed {
        refuse("team_wrong_state");
        return;
    }
    let unit = StaffedUnit::Team {
        name: team.to_owned(),
    };
    match unit_already_staffed(deps, &unit) {
        Ok(true) => {
            refuse("unit_already_staffed");
            return;
        }
        Ok(false) => {}
        Err(err) => {
            warn!(err = %err, "engagement scan failed during staff-team");
            refuse("engagement_scan_error");
            return;
        }
    }
    let root = match check_engagement_available(deps, engagement) {
        Ok(root) => root,
        Err(reason) => {
            refuse(reason);
            return;
        }
    };

    for member in &team_record.members {
        if let Err(err) = reincarnate_member(
            deps,
            audit,
            sender_id,
            &member.agent_name,
            Some(engagement),
            root.as_deref(),
            at,
        )
        .await
        {
            warn!(member = %member.agent_name, err, "member failed to reincarnate during staff-team");
            refuse(&format!("reincarnate_failed: {}: {err}", member.agent_name));
            return;
        }
    }

    if let Err(err) = deps.engagements.set_staffed_unit(engagement, Some(unit)) {
        warn!(err = %err, "failed to write staffed_unit after staff-team");
        refuse("engagement_write_failed");
        return;
    }
    info!(engagement, team, "team staffed");
    audit_append(
        audit,
        &AuditEvent::Staffed {
            sender_id,
            engagement: engagement.to_owned(),
            unit_kind: "team",
            unit_name: team.to_owned(),
            at,
        },
    );
}

/// Staff a lone teamless agent to a top-level engagement — the degenerate
/// unit of one.
#[expect(
    clippy::too_many_arguments,
    reason = "mirrors staff_team's collaborators (deps, audit, sender identity, timestamp) \
              plus the two names that identify the staffing target"
)]
#[expect(
    clippy::too_many_lines,
    reason = "each precondition (reserved name, team-membership scan, registry lookup, \
              double-staffing scan, engagement availability) is a distinct refusal path that \
              must run before reincarnation; collapsing them into helpers would hide which \
              check produced which audit reason"
)]
async fn staff_agent(
    deps: &EstateOpsDeps,
    audit: &Arc<AuditLog>,
    sender_id: reeve_types::IdentityId,
    engagement: &str,
    agent: &str,
    at: OffsetDateTime,
) {
    let refuse = |reason: &str| {
        audit_estate_refusal(
            audit,
            Some(sender_id),
            "staff-agent",
            Some(engagement.to_owned()),
            reason,
            at,
        );
    };
    if agent == ESTATE_AGENT_NAME {
        refuse("reserved");
        return;
    }
    // A team member is never a top-level unit on its own (spec § Engagement,
    // "Staffing authority follows the tree") — staff the team instead.
    match deps.teams.list() {
        Ok(rosters) => {
            let serving = rosters.iter().any(|r| {
                r.state == TeamState::Formed && r.members.iter().any(|m| m.agent_name == agent)
            });
            if serving {
                refuse("team_member");
                return;
            }
        }
        Err(err) => {
            warn!(err = %err, "roster scan failed during staff-agent");
            refuse("roster_error");
            return;
        }
    }
    match AgentRegistry::open(deps.agent_registry_path.clone()) {
        Ok(registry) => match registry.lookup(agent) {
            None => {
                refuse("agent_not_found");
                return;
            }
            Some(record) if matches!(record.status, AgentStatus::Retired) => {
                refuse("agent_retired");
                return;
            }
            Some(_) => {}
        },
        Err(err) => {
            warn!(err = %err, "agent registry open failed during staff-agent");
            refuse("registry_error");
            return;
        }
    }
    let unit = StaffedUnit::Agent {
        name: agent.to_owned(),
    };
    match unit_already_staffed(deps, &unit) {
        Ok(true) => {
            refuse("unit_already_staffed");
            return;
        }
        Ok(false) => {}
        Err(err) => {
            warn!(err = %err, "engagement scan failed during staff-agent");
            refuse("engagement_scan_error");
            return;
        }
    }
    let root = match check_engagement_available(deps, engagement) {
        Ok(root) => root,
        Err(reason) => {
            refuse(reason);
            return;
        }
    };

    if let Err(err) = reincarnate_member(
        deps,
        audit,
        sender_id,
        agent,
        Some(engagement),
        root.as_deref(),
        at,
    )
    .await
    {
        warn!(agent, err, "agent failed to reincarnate during staff-agent");
        refuse(&format!("reincarnate_failed: {err}"));
        return;
    }

    if let Err(err) = deps.engagements.set_staffed_unit(engagement, Some(unit)) {
        warn!(err = %err, "failed to write staffed_unit after staff-agent");
        refuse("engagement_write_failed");
        return;
    }
    info!(engagement, agent, "agent staffed");
    audit_append(
        audit,
        &AuditEvent::Staffed {
            sender_id,
            engagement: engagement.to_owned(),
            unit_kind: "agent",
            unit_name: agent.to_owned(),
            at,
        },
    );
}

/// Recall whatever unit is staffed to an engagement: every member winds
/// down and re-incarnates rootless (per spec § Constraints, an unstaffed
/// agent's snapshot carries no root — no daemon-cwd fallback anywhere).
/// The unit stays alive, just idle; unstaffing is not retirement.
async fn unstaff(
    deps: &EstateOpsDeps,
    audit: &Arc<AuditLog>,
    sender_id: reeve_types::IdentityId,
    engagement: &str,
    at: OffsetDateTime,
) {
    let refuse = |reason: &str| {
        audit_estate_refusal(
            audit,
            Some(sender_id),
            "unstaff",
            Some(engagement.to_owned()),
            reason,
            at,
        );
    };
    let record = match deps.engagements.get(engagement) {
        Ok(r) => r,
        Err(EngagementError::NotFound { .. }) => {
            refuse("engagement_not_found");
            return;
        }
        Err(err) => {
            warn!(err = %err, "engagement lookup failed during unstaff");
            refuse("engagement_error");
            return;
        }
    };
    let Some(unit) = record.staffed_unit.clone() else {
        refuse("not_staffed");
        return;
    };

    let members: Vec<String> = match &unit {
        StaffedUnit::Team { name } => match deps.teams.get(name) {
            Ok(team_record) => team_record
                .members
                .iter()
                .map(|m| m.agent_name.clone())
                .collect(),
            Err(err) => {
                warn!(err = %err, "roster lookup failed during unstaff");
                refuse("roster_error");
                return;
            }
        },
        StaffedUnit::Agent { name } => vec![name.clone()],
    };

    for member in &members {
        if let Err(err) = reincarnate_member(deps, audit, sender_id, member, None, None, at).await {
            warn!(
                member,
                err, "member failed to reincarnate rootless during unstaff"
            );
            refuse(&format!("reincarnate_failed: {member}: {err}"));
            return;
        }
    }

    if let Err(err) = deps.engagements.set_staffed_unit(engagement, None) {
        warn!(err = %err, "failed to clear staffed_unit after unstaff");
        refuse("engagement_write_failed");
        return;
    }
    let (unit_kind, unit_name) = match &unit {
        StaffedUnit::Team { name } => ("team", name.clone()),
        StaffedUnit::Agent { name } => ("agent", name.clone()),
    };
    info!(engagement, unit_kind, unit_name, "unit unstaffed");
    audit_append(
        audit,
        &AuditEvent::Unstaffed {
            sender_id,
            engagement: engagement.to_owned(),
            unit_kind,
            unit_name,
            at,
        },
    );
}

async fn execute_team_op(
    deps: Arc<EstateOpsDeps>,
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
        EstateOp::StaffTeam { engagement, team } => {
            staff_team(&deps, &audit, sender_id, engagement, team, at).await;
        }
        EstateOp::StaffAgent { engagement, agent } => {
            staff_agent(&deps, &audit, sender_id, engagement, agent, at).await;
        }
        EstateOp::Unstaff { engagement } => {
            unstaff(&deps, &audit, sender_id, engagement, at).await;
        }
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
    deps: &EstateOpsDeps,
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
    let layout = RuntimeLayout::new(&deps.data_dir);
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
    deps: &EstateOpsDeps,
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
    deps: &EstateOpsDeps,
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
        /// Same collaborators the running `EstateCoordinator` holds — a
        /// second handle, not a separate instance (every field is either
        /// `Arc`-backed or reopens the same on-disk path). Lets tests call
        /// functions like `reincarnate_member` directly against the live
        /// system instead of only through the async `EstateOp` dispatch.
        deps: EstateOpsDeps,
        audit: Arc<AuditLog>,
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
        let watcher_for_deps = Arc::clone(&watcher);
        let coordinator = SpawnCoordinator::new(
            root.to_path_buf(),
            registry_path.clone(),
            Arc::clone(&identity_registry),
            adapters.clone(),
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
        let deps = EstateOpsDeps {
            spawner: coord_addr.recipient(),
            teams,
            engagements: engagements.clone(),
            control_routes,
            agent_registry_path: registry_path.clone(),
            data_dir: root.to_path_buf(),
            identity_registry,
            adapters,
            watcher: watcher_for_deps,
            inbox_starter: NullInboxStarter.start().recipient(),
            dispatcher: NullDispatcher.start().recipient(),
            blacklist: None,
        };
        let addr = EstateCoordinator::new(operator_id, engagements, Arc::clone(&audit))
            .with_team_ops(deps.clone())
            .start();
        TeamBed {
            data_dir,
            operator_id,
            registry_path,
            addr,
            deps,
            audit,
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

    // ── Staffing operation tests ─────────────────────────────────────────────

    fn read_snapshot(data_dir: &Path, name: &str) -> SpawnSnapshot {
        let dirs = AgentDirs::open(data_dir, name).unwrap();
        let text = fs::read_to_string(dirs.agent_toml_path()).unwrap();
        toml::from_str(&text).unwrap()
    }

    async fn staff_team_op(bed: &TeamBed, engagement: &str, team: &str) {
        send_op(
            bed,
            &EstateOp::StaffTeam {
                engagement: engagement.to_owned(),
                team: team.to_owned(),
            },
        )
        .await;
    }

    async fn open_engagement(bed: &TeamBed, name: &str, root: Option<PathBuf>) {
        send_op(
            bed,
            &EstateOp::OpenEngagement {
                name: name.to_owned(),
                purpose: "test".to_owned(),
                root,
            },
        )
        .await;
        let engagements =
            EngagementRegistry::open(bed.data_dir.path().join("engagements")).unwrap();
        wait_until("engagement to open", || engagements.get(name).is_ok()).await;
    }

    #[test]
    fn staff_agent_writes_context_and_audits_staffed_and_reincarnated() {
        actix::System::new().block_on(async {
            let bed = start_team_bed();
            let root = bed.data_dir.path().join("work");
            fs::create_dir_all(&root).unwrap();
            let canonical_root = fs::canonicalize(&root).unwrap();

            send_op(
                &bed,
                &EstateOp::MintAgent {
                    name: "librarian".to_owned(),
                    persona: "worker".to_owned(),
                },
            )
            .await;
            wait_until("librarian running", || {
                agent_status(&bed.registry_path, "librarian") == Some(AgentStatus::Running)
            })
            .await;
            // Unstaffed at mint: no root, no engagement — no daemon-cwd
            // fallback anywhere.
            let minted = read_snapshot(bed.data_dir.path(), "librarian");
            assert_eq!(minted.engagement_name, None);
            assert_eq!(minted.working_root, None);

            open_engagement(&bed, "billing", Some(root.clone())).await;
            send_op(
                &bed,
                &EstateOp::StaffAgent {
                    engagement: "billing".to_owned(),
                    agent: "librarian".to_owned(),
                },
            )
            .await;

            wait_until("librarian staffed snapshot", || {
                read_snapshot(bed.data_dir.path(), "librarian")
                    .engagement_name
                    .as_deref()
                    == Some("billing")
            })
            .await;
            let staffed = read_snapshot(bed.data_dir.path(), "librarian");
            assert_eq!(staffed.working_root, Some(canonical_root));

            let engagements =
                EngagementRegistry::open(bed.data_dir.path().join("engagements")).unwrap();
            let record = engagements.get("billing").unwrap();
            assert_eq!(
                record.staffed_unit,
                Some(StaffedUnit::Agent {
                    name: "librarian".to_owned()
                })
            );

            assert!(audit_has(bed.data_dir.path(), "staffing.staffed", |v| {
                v["engagement"] == "billing"
                    && v["unit_kind"] == "agent"
                    && v["unit_name"] == "librarian"
            }));
            assert!(audit_has(
                bed.data_dir.path(),
                "staffing.reincarnated",
                |v| { v["name"] == "librarian" && v["engagement"] == "billing" }
            ));
        });
    }

    #[test]
    fn staff_team_writes_context_to_every_member() {
        actix::System::new().block_on(async {
            let bed = start_team_bed();
            let root = bed.data_dir.path().join("work");
            fs::create_dir_all(&root).unwrap();
            let canonical_root = fs::canonicalize(&root).unwrap();

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

            open_engagement(&bed, "billing", Some(root)).await;
            staff_team_op(&bed, "billing", "crew").await;

            for member in ["crew-lead", "crew-helper-1", "crew-helper-2"] {
                wait_until(&format!("{member} staffed snapshot"), || {
                    read_snapshot(bed.data_dir.path(), member)
                        .engagement_name
                        .as_deref()
                        == Some("billing")
                })
                .await;
                assert_eq!(
                    read_snapshot(bed.data_dir.path(), member).working_root,
                    Some(canonical_root.clone())
                );
            }

            let engagements =
                EngagementRegistry::open(bed.data_dir.path().join("engagements")).unwrap();
            assert_eq!(
                engagements.get("billing").unwrap().staffed_unit,
                Some(StaffedUnit::Team {
                    name: "crew".to_owned()
                })
            );
            assert!(audit_has(bed.data_dir.path(), "staffing.staffed", |v| {
                v["unit_kind"] == "team" && v["unit_name"] == "crew"
            }));
        });
    }

    #[test]
    fn unstaff_clears_context_and_engagement_state() {
        actix::System::new().block_on(async {
            let bed = start_team_bed();
            let root = bed.data_dir.path().join("work");
            fs::create_dir_all(&root).unwrap();

            send_op(
                &bed,
                &EstateOp::MintAgent {
                    name: "librarian".to_owned(),
                    persona: "worker".to_owned(),
                },
            )
            .await;
            wait_until("librarian running", || {
                agent_status(&bed.registry_path, "librarian") == Some(AgentStatus::Running)
            })
            .await;
            open_engagement(&bed, "billing", Some(root)).await;
            send_op(
                &bed,
                &EstateOp::StaffAgent {
                    engagement: "billing".to_owned(),
                    agent: "librarian".to_owned(),
                },
            )
            .await;
            wait_until("librarian staffed", || {
                read_snapshot(bed.data_dir.path(), "librarian")
                    .engagement_name
                    .is_some()
            })
            .await;

            send_op(
                &bed,
                &EstateOp::Unstaff {
                    engagement: "billing".to_owned(),
                },
            )
            .await;

            wait_until("librarian rootless again", || {
                read_snapshot(bed.data_dir.path(), "librarian")
                    .engagement_name
                    .is_none()
            })
            .await;
            let rootless = read_snapshot(bed.data_dir.path(), "librarian");
            assert_eq!(rootless.working_root, None);

            let engagements =
                EngagementRegistry::open(bed.data_dir.path().join("engagements")).unwrap();
            assert_eq!(engagements.get("billing").unwrap().staffed_unit, None);
            assert!(audit_has(bed.data_dir.path(), "staffing.unstaffed", |v| {
                v["engagement"] == "billing" && v["unit_name"] == "librarian"
            }));
        });
    }

    #[test]
    fn restaffing_after_unstaff_moves_the_team_to_the_new_root() {
        actix::System::new().block_on(async {
            let bed = start_team_bed();
            let root_a = bed.data_dir.path().join("a");
            let root_b = bed.data_dir.path().join("b");
            fs::create_dir_all(&root_a).unwrap();
            fs::create_dir_all(&root_b).unwrap();
            let canonical_b = fs::canonicalize(&root_b).unwrap();

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

            open_engagement(&bed, "engagement-a", Some(root_a)).await;
            staff_team_op(&bed, "engagement-a", "crew").await;
            wait_until("crew-lead staffed to A", || {
                read_snapshot(bed.data_dir.path(), "crew-lead")
                    .engagement_name
                    .as_deref()
                    == Some("engagement-a")
            })
            .await;

            // A team already serving an engagement cannot be staffed directly
            // to a second one — unstaff first, matching the strict
            // 1:1-at-a-time contract.
            open_engagement(&bed, "engagement-b", Some(root_b)).await;
            staff_team_op(&bed, "engagement-b", "crew").await;
            wait_until("double-staff refusal", || {
                audit_has(bed.data_dir.path(), "estate.op_refused", |v| {
                    v["op"] == "staff-team" && v["reason"] == "unit_already_staffed"
                })
            })
            .await;

            send_op(
                &bed,
                &EstateOp::Unstaff {
                    engagement: "engagement-a".to_owned(),
                },
            )
            .await;
            wait_until("crew-lead rootless", || {
                read_snapshot(bed.data_dir.path(), "crew-lead")
                    .engagement_name
                    .is_none()
            })
            .await;

            staff_team_op(&bed, "engagement-b", "crew").await;
            wait_until("crew-lead staffed to B", || {
                read_snapshot(bed.data_dir.path(), "crew-lead")
                    .engagement_name
                    .as_deref()
                    == Some("engagement-b")
            })
            .await;
            assert_eq!(
                read_snapshot(bed.data_dir.path(), "crew-lead").working_root,
                Some(canonical_b)
            );
        });
    }

    #[test]
    fn staffing_refuses_second_unit_to_an_already_staffed_engagement() {
        actix::System::new().block_on(async {
            let bed = start_team_bed();
            let root = bed.data_dir.path().join("work");
            fs::create_dir_all(&root).unwrap();

            send_op(
                &bed,
                &EstateOp::MintAgent {
                    name: "first".to_owned(),
                    persona: "worker".to_owned(),
                },
            )
            .await;
            send_op(
                &bed,
                &EstateOp::MintAgent {
                    name: "second".to_owned(),
                    persona: "worker".to_owned(),
                },
            )
            .await;
            wait_until("both minted", || {
                agent_status(&bed.registry_path, "first") == Some(AgentStatus::Running)
                    && agent_status(&bed.registry_path, "second") == Some(AgentStatus::Running)
            })
            .await;

            open_engagement(&bed, "billing", Some(root)).await;
            send_op(
                &bed,
                &EstateOp::StaffAgent {
                    engagement: "billing".to_owned(),
                    agent: "first".to_owned(),
                },
            )
            .await;
            wait_until("first staffed", || {
                read_snapshot(bed.data_dir.path(), "first")
                    .engagement_name
                    .is_some()
            })
            .await;

            send_op(
                &bed,
                &EstateOp::StaffAgent {
                    engagement: "billing".to_owned(),
                    agent: "second".to_owned(),
                },
            )
            .await;
            wait_until("second staffing refused", || {
                audit_has(bed.data_dir.path(), "estate.op_refused", |v| {
                    v["op"] == "staff-agent" && v["reason"] == "engagement_already_staffed"
                })
            })
            .await;
            assert_eq!(
                read_snapshot(bed.data_dir.path(), "second").engagement_name,
                None,
                "the refused agent's snapshot must not have been touched"
            );
        });
    }

    #[test]
    fn staff_agent_refuses_a_team_member_offered_as_a_top_level_unit() {
        actix::System::new().block_on(async {
            let bed = start_team_bed();
            let root = bed.data_dir.path().join("work");
            fs::create_dir_all(&root).unwrap();

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

            open_engagement(&bed, "billing", Some(root)).await;
            send_op(
                &bed,
                &EstateOp::StaffAgent {
                    engagement: "billing".to_owned(),
                    agent: "crew-helper-1".to_owned(),
                },
            )
            .await;

            wait_until("team-member staffing refused", || {
                audit_has(bed.data_dir.path(), "estate.op_refused", |v| {
                    v["op"] == "staff-agent" && v["reason"] == "team_member"
                })
            })
            .await;
        });
    }

    // Regression (Copilot review, PR staffing-moves-teams): a concurrent
    // RetireAgent can land while a staffing wind-down is draining —
    // retire_identity's control-route lookup finds nothing (the
    // reincarnation already unregistered it) and writes Retired directly
    // to the registry. reincarnate_member's entry-point registry read
    // doesn't gate on status, so without the post-wind-down check it would
    // relaunch straight over an already-retired identity. Calling it
    // directly against a fully-retired agent exercises the same code path
    // a mid-call race would: the entry read still finds the (now stale)
    // record, and the new check is what actually refuses.
    #[test]
    fn reincarnate_member_refuses_to_relaunch_an_already_retired_agent() {
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
            wait_until("librarian running", || {
                agent_status(&bed.registry_path, "librarian") == Some(AgentStatus::Running)
            })
            .await;

            send_op(
                &bed,
                &EstateOp::RetireAgent {
                    name: "librarian".to_owned(),
                },
            )
            .await;
            wait_until("librarian retired", || {
                agent_status(&bed.registry_path, "librarian") == Some(AgentStatus::Retired)
            })
            .await;

            let result = reincarnate_member(
                &bed.deps,
                &bed.audit,
                bed.operator_id,
                "librarian",
                Some("billing"),
                None,
                OffsetDateTime::now_utc(),
            )
            .await;

            assert!(
                result.is_err(),
                "must refuse to relaunch a retired identity, got: {result:?}"
            );
            assert_eq!(
                agent_status(&bed.registry_path, "librarian"),
                Some(AgentStatus::Retired),
                "retirement must survive the aborted relaunch attempt"
            );
        });
    }
}
