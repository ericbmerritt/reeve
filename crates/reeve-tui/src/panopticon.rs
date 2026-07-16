//! Panopticon screen view-model and snapshot reader.
//!
//! The panopticon shows every agent in the runtime — running and stopped —
//! along with a merged recent-events stream and queue counts. This module
//! holds:
//!
//! - The typed view-model the renderer reads ([`PanopticonSnapshot`] and its
//!   parts).
//! - A pure builder ([`build_snapshot`]) that maps pre-read per-agent data
//!   into a snapshot. Pure so the merge-and-rank logic can be tested without
//!   filesystem fixtures.
//! - An IO orchestrator ([`read_snapshot`]) that opens the agent registry,
//!   reads each agent's on-disk state via [`crate::reader`] helpers, and
//!   hands the result to [`build_snapshot`].
//!
//! On-disk format is defined by `reeve-runtime::agent_fs`. Errors are absorbed
//! into safe defaults so the TUI always has something renderable.
//!
//! Pending decisions surface the most recent authority *refusals*, read from
//! the audit log's `authority.decision` entries. The log records the issuing
//! agent by identity, not role name; [`build_snapshot`] joins each decision's
//! `agent_id` against the registry-derived inputs to recover a display name.
//!
//! Sort and attribution choices come from the Phase 6 design review
//! (Saskia's TUI panel): within the running group, agents rank by status
//! priority (Working → Idle → Unknown), then by spawn-age descending; the
//! lead is pinned at the top regardless. Operator-sent events are attributed
//! to the synthetic source name `"you"` so the panopticon's events stream
//! stays the operator's anchor identity instead of fanning a single message
//! out across every recipient's row.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use time::OffsetDateTime;

use reeve_runtime::{
    audit_log_path, AgentDirs, AgentRegistry, EngagementRegistry, EngagementState, RuntimeLayout,
    StaffedUnit,
};
use reeve_types::IdentityId;

use crate::reader::{
    read_authority_decisions_tail, read_conversation_tail, read_cost, read_status,
};
use crate::state::{AgentStatus, AuthorityDecision, Disposition, EntryKind};

/// Display label rendered for [`Source::Operator`] in the events stream.
/// Pinned here so renderer and tests share one literal.
pub const OPERATOR_LABEL: &str = "you";

/// Maximum number of recent events surfaced in a panopticon snapshot. Larger
/// than the wireframe target (6 lines) so the renderer can scroll without
/// re-reading.
const MAX_RECENT_EVENTS: usize = 32;

/// Per-agent tail size used when assembling the merged events stream.
/// Capped so a chatty agent does not dominate the merge — the final list is
/// sorted across all agents and truncated to [`MAX_RECENT_EVENTS`].
const PER_AGENT_TAIL: usize = 16;

/// Bytes of `conversation.jsonl` to tail-read per agent during a snapshot.
/// At ~500 bytes per JSONL line that's ~16 entries — enough to fill
/// [`PER_AGENT_TAIL`] without reading more. The size of an N-agent
/// snapshot is therefore `O(N × CONVERSATION_TAIL_BYTES)` regardless of
/// how long any individual conversation has grown.
const CONVERSATION_TAIL_BYTES: u64 = 8 * 1024;

/// Bytes of the audit log to tail-read per snapshot. 64 KiB holds on the
/// order of 500 `authority.decision` entries — enough recent history for both
/// the pending-decisions panel and the per-agent Decisions tab while keeping
/// per-tick IO bounded as the log grows without limit. Shared with the
/// inspect reload (`crate::app`) so both audit reads window the log
/// identically.
pub(crate) const AUDIT_TAIL_BYTES: u64 = 64 * 1024;

/// Maximum number of refusal rows rendered in the pending-decisions panel.
/// The panel is a glanceable "what needs attention" strip, not a log; the
/// full history lives in each agent's Decisions tab. The `▲ N` title count
/// still reports the *total* refusals in the tail, not this cap.
const MAX_PENDING_DECISIONS: usize = 5;

// ── View-model types ──────────────────────────────────────────────────────────

/// One row in the panopticon's agent table.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRow {
    /// Role name from the agent registry (`"lead"`, `"worker-abc12345"`, …).
    pub name: String,
    /// Persona the agent was spawned under (`Some` after Phase 3+; legacy
    /// records may carry `None`).
    pub persona_name: Option<String>,
    /// Live status read from the agent's `status` file.
    pub status: AgentStatus,
    /// `false` once the registry marks the agent as stopped. The renderer
    /// uses this to pick between the running sigils (`○`/`●`/`!`/`?`) and
    /// the stopped sigils (`✓` clean, `✗` crash).
    pub is_running: bool,
    /// `true` when the agent has no `agent.toml` on disk — the spawn sequence
    /// never completed. Ghost agents have a registry record but no running
    /// actor; messages sent to them land in an unwatched inbox.
    pub is_ghost: bool,
    /// Cumulative cost from the agent's `cost` file (USD).
    pub cost_usd: f64,
    /// Time since the agent was spawned, evaluated at snapshot time. The
    /// renderer formats this as `Hh MMm` / `Mm` / `Ss`.
    pub elapsed: time::Duration,
    /// Approximate time the agent last transitioned status, read from the
    /// `status` file's mtime. `None` when the file is absent or unreadable.
    /// The renderer uses this to render time-in-state alongside the working
    /// sigil (`● 0:12`); a stand-in for a future durable `state_changed_at`
    /// journaled at the transition.
    pub state_changed_at: Option<OffsetDateTime>,
}

/// Origin of an event in the merged recent-events stream. Renderers turn
/// this into a label + colour pair; the data layer doesn't know which colour
/// the lead is rendered in, only that the operator is a distinct kind of
/// source from any agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The operator originated the entry (`sender_id` matched
    /// `operator_id` at snapshot time). Renders as the label in
    /// [`OPERATOR_LABEL`].
    Operator,
    /// An agent originated the entry; `name` is the role name from the
    /// agent registry (`"lead"`, `"worker-abc12345"`, …).
    Agent(String),
}

impl Source {
    /// Display label for this source: the agent's name or
    /// [`OPERATOR_LABEL`] for the operator. Used by the renderer for the
    /// events-stream source column and by tests for assertions.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Operator => OPERATOR_LABEL,
            Self::Agent(name) => name.as_str(),
        }
    }
}

/// Categorisation of a single recent-events row. Phase 6 ships two variants
/// (msg, system); future phases that grow richer journals (tool, flag,
/// model, exit, spawn) extend this enum rather than regex-sniffing system
/// bodies. The renderer's exhaustive match prevents a new variant from
/// silently dropping into an "unknown" rendering path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// An `Inbound` or `Outbound` conversation entry — a message between
    /// the operator and agents, or between agents.
    Msg,
    /// A `System` conversation entry — startup, shutdown, tool dispatch,
    /// or other non-message annotations the runtime journaled.
    System,
}

impl EventKind {
    /// Short token used as the kind column's text in the events stream.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Msg => "msg",
            Self::System => "system",
        }
    }
}

/// One row in the merged recent-events stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentEvent {
    /// Time the event was journaled.
    pub timestamp: OffsetDateTime,
    /// Origin of the event.
    pub source: Source,
    /// Category of the event (msg, system, …).
    pub kind: EventKind,
    /// One-line summary of the event body. Long entries are truncated by
    /// the renderer.
    pub summary: String,
}

/// Counts on each non-blocking review pile shown in the queues row.
///
/// `memory`, `config`, and `cost_ok` are placeholders that always read zero
/// in this phase — the corresponding review piles do not yet exist. They are
/// present so the renderer can claim the full queue row layout now and have
/// somewhere to wire counts later without a struct shape change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueCounts {
    pub memory: usize,
    pub config: usize,
    /// Total files across every agent's `quarantine/` directory.
    pub quarantine: usize,
    /// Whether the session cost ceiling is OK (`true`) or has tripped
    /// (`false`). Always `true` in this phase.
    pub cost_ok: bool,
}

/// One row in the pending-decisions panel — a single authority refusal,
/// resolved for display. Distinct from [`crate::state::AuthorityDecision`]
/// (the raw parsed entry): the agent is named, allows are excluded, and only
/// the fields the panel renders are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDecision {
    /// Time the decision was recorded.
    pub timestamp: OffsetDateTime,
    /// Display name of the refused agent: the role name when the `agent_id`
    /// resolves against the registry, otherwise the persona name as a
    /// fallback so a decision from an unregistered/stopped agent still reads.
    pub agent_name: String,
    /// The refused action descriptor (`Tool(specifier)` form).
    pub action: String,
    /// Operator-facing reason for the refusal.
    pub rationale: String,
}

/// One row in the panopticon's engagement table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngagementRow {
    /// Unique-per-estate engagement name.
    pub name: String,
    /// Lifecycle state (open/closed).
    pub state: EngagementState,
    /// Working root, when the engagement is repo- or directory-bound.
    pub root: Option<PathBuf>,
    /// The team or lone agent currently staffed here, if any.
    pub staffed_unit: Option<StaffedUnit>,
}

/// Everything the panopticon renderer needs in a single snapshot.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PanopticonSnapshot {
    pub agents: Vec<AgentRow>,
    pub engagements: Vec<EngagementRow>,
    pub recent_events: Vec<RecentEvent>,
    pub queue_counts: QueueCounts,
    /// Sum of every agent's `cost_usd`. Surfaced in the title bar.
    pub total_cost_usd: f64,
    /// Time since the oldest agent was spawned. Surfaced in the title bar as
    /// the session-elapsed indicator.
    pub session_elapsed: Option<time::Duration>,
    /// Most recent refusals, newest first, capped at a small fixed count for
    /// the pending-decisions panel.
    pub pending_decisions: Vec<PendingDecision>,
    /// Total refusals in the audit-log tail — the `▲ N` panel count. May
    /// exceed `pending_decisions.len()` when more than the cap were refused.
    pub refusal_count: usize,
}

// ── Pure builder ──────────────────────────────────────────────────────────────

/// Per-agent inputs the builder needs from disk. Surfaced as a separate type
/// so [`build_snapshot`] stays pure.
#[derive(Debug, Clone)]
pub struct AgentInputs {
    pub name: String,
    /// Identity of the agent, used to join audit-log authority decisions
    /// (which key on identity, not role name) back to this row.
    pub identity_id: IdentityId,
    pub persona_name: Option<String>,
    pub status: AgentStatus,
    pub is_running: bool,
    /// `true` when `agent.toml` is absent — the spawn sequence never finished.
    /// The agent has a registry record but no running actor.
    pub is_ghost: bool,
    pub cost_usd: f64,
    pub spawned_at: OffsetDateTime,
    /// Best-effort time-in-state proxy: the `status` file's mtime, read by
    /// [`read_snapshot`]. `None` when unavailable.
    pub state_changed_at: Option<OffsetDateTime>,
    /// Already-parsed conversation entries (output of
    /// [`crate::reader::read_conversation_tail`]) — the builder maps these
    /// into [`RecentEvent`] values for the merged stream. Bounded in size
    /// per agent so an N-agent snapshot stays O(N) in IO regardless of
    /// individual conversation length.
    pub conversation_tail: Vec<crate::state::ConversationEntry>,
}

/// Build a panopticon snapshot from pre-read per-agent data.
///
/// Pure: every input is materialised, no filesystem access happens here.
/// Useful both for unit tests of the merge/rank logic and for callers that
/// want to assemble a snapshot from a non-filesystem source (e.g., a future
/// in-memory test harness or a network panel).
///
/// `operator_id` is consulted only by the events-stream builder: entries
/// whose `sender_id` matches it surface as [`Source::Operator`] instead of
/// [`Source::Agent`] with the recipient's name. Pass `None` when the
/// operator is not yet known (builder falls back to per-agent attribution).
/// `decisions` are the audit log's parsed authority decisions (allows and
/// refuses); the builder keeps only refusals for the pending panel and joins
/// each one's `agent_id` to a display name via `inputs`.
#[expect(
    clippy::too_many_arguments,
    reason = "each argument is an independently-sourced input the builder merges into one \
              snapshot (agents, engagements, decisions, quarantine count, operator identity, \
              now); bundling them into a struct would just move the same count into a \
              constructor callers still have to fill in field by field"
)]
#[must_use]
pub fn build_snapshot(
    inputs: &[AgentInputs],
    engagements: &[EngagementRow],
    decisions: &[AuthorityDecision],
    quarantine_count: usize,
    operator_id: Option<IdentityId>,
    now: OffsetDateTime,
) -> PanopticonSnapshot {
    let mut agents: Vec<AgentRow> = inputs
        .iter()
        .map(|a| AgentRow {
            name: a.name.clone(),
            persona_name: a.persona_name.clone(),
            status: a.status.clone(),
            is_running: a.is_running,
            is_ghost: a.is_ghost,
            cost_usd: a.cost_usd,
            elapsed: now - a.spawned_at,
            state_changed_at: a.state_changed_at,
        })
        .collect();
    // Sort key (matches Saskia's "attention is the scarce resource" rule):
    //   1. lead pinned at the top
    //   2. running before stopped
    //   3. within running, by status priority (Working > Idle > Crashed >
    //      Unknown) so the most demanding agents rise
    //   4. within each status bucket, oldest first — the stale agent is
    //      usually the stuck one
    agents.sort_by_key(sort_key);

    let total_cost_usd: f64 = agents.iter().map(|a| a.cost_usd).sum();
    let session_elapsed = agents.iter().map(|a| a.elapsed).max();

    let mut events: Vec<RecentEvent> = inputs
        .iter()
        .flat_map(|a| {
            a.conversation_tail
                .iter()
                .rev()
                .take(PER_AGENT_TAIL)
                .filter_map(|entry| build_event(&a.name, entry, operator_id))
        })
        .collect();
    events.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
    events.truncate(MAX_RECENT_EVENTS);

    // Join each decision's identity to a role name. The audit log keys on
    // identity (role names are not stable across re-registration); the
    // registry-derived inputs are the only place that mapping lives.
    let name_by_id: std::collections::HashMap<IdentityId, &str> = inputs
        .iter()
        .map(|a| (a.identity_id, a.name.as_str()))
        .collect();
    let refusal_count = decisions
        .iter()
        .filter(|d| d.disposition == Disposition::Refuse)
        .count();
    let mut pending_decisions: Vec<PendingDecision> = decisions
        .iter()
        .filter(|d| d.disposition == Disposition::Refuse)
        .filter_map(|d| build_pending_decision(d, &name_by_id))
        .collect();
    pending_decisions.sort_by_key(|d| std::cmp::Reverse(d.timestamp));
    pending_decisions.truncate(MAX_PENDING_DECISIONS);

    // Open engagements first (the ones an operator cares about day-to-day),
    // then alphabetically within each group so the list is stable across
    // snapshots instead of reflecting directory-read order.
    let mut engagements = engagements.to_vec();
    engagements.sort_by_key(|e| (e.state != EngagementState::Open, e.name.clone()));

    PanopticonSnapshot {
        agents,
        engagements,
        recent_events: events,
        queue_counts: QueueCounts {
            memory: 0,
            config: 0,
            quarantine: quarantine_count,
            cost_ok: true,
        },
        total_cost_usd,
        session_elapsed,
        pending_decisions,
        refusal_count,
    }
}

/// Composite sort key for the agent table. Smaller tuples sort first; the
/// final `Reverse(elapsed)` puts the oldest agent first within each bucket.
///
/// Returns: `(group, status_priority, Reverse(elapsed_seconds))` where
/// - `group`: 0=lead, 1=running non-lead, 2=stopped
/// - `status_priority`: only meaningful for the running group
fn sort_key(row: &AgentRow) -> (u8, u8, std::cmp::Reverse<i128>) {
    let group: u8 = if row.name == "lead" {
        0
    } else if row.is_running {
        1
    } else {
        2
    };
    let status_priority: u8 = match row.status {
        AgentStatus::Working => 0,
        AgentStatus::Idle => 1,
        AgentStatus::Exiting => 2,
        AgentStatus::Crashed => 3,
        AgentStatus::Unknown => 4,
    };
    (
        group,
        status_priority,
        std::cmp::Reverse(row.elapsed.whole_seconds().into()),
    )
}

/// Map a single `ConversationEntry` into a `RecentEvent`. Returns `None` for
/// entries without a timestamp — the merged stream is sorted by timestamp, so
/// undatable entries cannot participate.
///
/// When the entry's `sender_id` matches `operator_id`, the event's source
/// is [`Source::Operator`] (rendered as `"you"`) so a single operator
/// message surfaces as one row in the global stream instead of being
/// fanned out across every recipient agent's row.
fn build_event(
    agent_name: &str,
    entry: &crate::state::ConversationEntry,
    operator_id: Option<IdentityId>,
) -> Option<RecentEvent> {
    let timestamp = entry.timestamp?;
    let kind = match entry.kind {
        EntryKind::Inbound | EntryKind::Outbound => EventKind::Msg,
        EntryKind::System => EventKind::System,
    };
    let is_from_operator = matches!(
        (entry.sender_id, operator_id),
        (Some(s), Some(op)) if s == op
    );
    let source = if is_from_operator {
        Source::Operator
    } else {
        Source::Agent(agent_name.to_owned())
    };
    let summary = summarize_event(&entry.text);
    Some(RecentEvent {
        timestamp,
        source,
        kind,
        summary,
    })
}

/// One-line summary of an entry's body. Strips leading/trailing whitespace
/// and replaces internal newlines with `· ` so the event row stays on one
/// physical line; the renderer truncates the result to the column width.
fn summarize_event(text: &str) -> String {
    text.trim()
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" \u{00B7} ")
}

/// Resolve a raw refusal into a pending-panel row, or `None` when it carries
/// no timestamp (the panel sorts by time, so an undatable refusal cannot be
/// placed — real audit entries always stamp `at`, so this drops only
/// malformed lines). The agent label prefers the registry role name and falls
/// back to the persona name when the `agent_id` is not in the current
/// registry (e.g. a since-unregistered agent still in the log tail).
fn build_pending_decision(
    decision: &AuthorityDecision,
    name_by_id: &std::collections::HashMap<IdentityId, &str>,
) -> Option<PendingDecision> {
    let timestamp = decision.timestamp?;
    let agent_name = name_by_id
        .get(&decision.agent_id)
        .map(|name| (*name).to_owned())
        .unwrap_or_else(|| decision.persona_name.clone());
    Some(PendingDecision {
        timestamp,
        agent_name,
        action: decision.action.clone(),
        rationale: decision.rationale.clone().unwrap_or_default(),
    })
}

// ── IO orchestrator ───────────────────────────────────────────────────────────

/// Read a panopticon snapshot from the on-disk runtime data root.
///
/// `data_dir` is the runtime data root (where `agents/<name>/` directories
/// live); `agent_registry_path` is the agent registry TOML. The registry is
/// re-opened on every call so newly spawned agents appear without the TUI
/// restarting. `operator_id` is plumbed into [`build_snapshot`] for the
/// "you" attribution on operator-originated events.
///
/// Errors opening the registry or reading any single agent's files are
/// absorbed: the returned snapshot omits the affected agent but still
/// includes every other agent that read cleanly. This keeps the screen
/// renderable even during partial outages.
#[must_use]
pub fn read_snapshot(
    data_dir: &Path,
    agent_registry_path: &Path,
    operator_id: Option<IdentityId>,
) -> PanopticonSnapshot {
    let now = OffsetDateTime::now_utc();

    // The TUI swallows registry-open errors the same way every other reader
    // in this crate does (status, cost, conversation): the screen has to
    // stay renderable during transient outages or startup races. An empty
    // snapshot is the right "I don't know yet" presentation.
    let Ok(registry) = AgentRegistry::open(agent_registry_path.to_path_buf()) else {
        return build_snapshot(&[], &[], &[], 0, operator_id, now);
    };

    let mut inputs: Vec<AgentInputs> = Vec::new();
    let mut quarantine_count: usize = 0;

    for record in registry.list() {
        let Ok(dirs) = AgentDirs::open(data_dir, record.name.as_str()) else {
            continue;
        };

        let status_path = dirs.status_path();
        let status = read_status(&status_path);
        let state_changed_at = read_status_mtime(&status_path);
        let cost_usd = read_cost(&dirs.cost_path());
        let conversation =
            read_conversation_tail(&dirs.conversation_path(), CONVERSATION_TAIL_BYTES);

        let is_running = matches!(record.status, reeve_runtime::AgentStatus::Running);
        let is_ghost = !dirs.agent_toml_path().exists();

        inputs.push(AgentInputs {
            name: record.name.as_str().to_owned(),
            identity_id: record.identity_id,
            persona_name: record.persona_name.clone(),
            status,
            is_running,
            is_ghost,
            cost_usd,
            spawned_at: record.spawned_at,
            state_changed_at,
            conversation_tail: conversation,
        });

        quarantine_count += count_quarantine(&record.inbox_dir);
    }

    // One tail-read of the shared audit log per snapshot (the log is global,
    // not per-agent), filtered to authority decisions by the reader.
    let decisions = read_authority_decisions_tail(&audit_log_path(data_dir), AUDIT_TAIL_BYTES);

    // Engagement records are opened fresh every snapshot, same as the agent
    // registry above — a failed open (root not yet provisioned) or a failed
    // list (parse error, torn record) is absorbed into an empty list rather
    // than surfaced as an error; the "always renderable" contract this
    // module's doc comment describes covers both failure points equally.
    let engagements = EngagementRegistry::open(RuntimeLayout::new(data_dir).engagements_root())
        .and_then(|registry| registry.list())
        .unwrap_or_default()
        .into_iter()
        .map(|record| EngagementRow {
            name: record.name,
            state: record.state,
            root: record.root,
            staffed_unit: record.staffed_unit,
        })
        .collect::<Vec<_>>();

    build_snapshot(
        &inputs,
        &engagements,
        &decisions,
        quarantine_count,
        operator_id,
        now,
    )
}

/// Read the status file's mtime as an approximate time-in-state anchor.
///
/// The runtime rewrites this file on every state transition; the mtime is
/// therefore a faithful stand-in for a durable `state_changed_at` field
/// until one lands in the journal.
///
/// Two caveats the renderer needs to know about:
///
/// 1. **`mtime > now` is possible.** Atomic rename, manual `touch`, NTP
///    skew, and some networked filesystems can produce a "modified in the
///    future" stamp. [`time::Duration`] subtraction still yields a value;
///    the renderer's `format_short_duration` clamps the negative case to
///    `"0:00"`, which presents as a perpetually-fresh state. There is no
///    fix at this layer — the only durable solution is a journaled
///    transition timestamp.
/// 2. **Same-value rewrites still bump mtime.** If the runtime ever
///    rewrites the status file with its existing value, the mtime moves
///    even though the state did not change. Today the runtime writes only
///    on transition, so this is theoretical.
fn read_status_mtime(status_path: &Path) -> Option<OffsetDateTime> {
    let modified: SystemTime = std::fs::metadata(status_path).ok()?.modified().ok()?;
    OffsetDateTime::from(modified).into()
}

/// Count files in `<inbox_dir>/quarantine/`. Absent or unreadable directories
/// contribute zero (typed errors are not surfaced — the queue counter is a
/// best-effort indicator, not a security boundary).
fn count_quarantine(inbox_dir: &Path) -> usize {
    let quarantine = inbox_dir.join("quarantine");
    let Ok(entries) = std::fs::read_dir(&quarantine) else {
        return 0;
    };
    entries.filter_map(Result::ok).count()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ConversationEntry, EntryKind};

    fn op_id() -> IdentityId {
        IdentityId::new().unwrap()
    }

    fn entry(text: &str, ts_offset_secs: i64, kind: EntryKind) -> ConversationEntry {
        ConversationEntry {
            kind,
            text: text.to_owned(),
            timestamp: Some(
                OffsetDateTime::from_unix_timestamp(1_700_000_000 + ts_offset_secs).unwrap(),
            ),
            sender_id: Some(op_id()),
        }
    }

    fn now_at(unix_secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(unix_secs).unwrap()
    }

    fn inputs(name: &str, spawned_offset: i64, running: bool, cost: f64) -> AgentInputs {
        inputs_with_status(name, spawned_offset, running, cost, AgentStatus::Idle)
    }

    fn inputs_with_status(
        name: &str,
        spawned_offset: i64,
        running: bool,
        cost: f64,
        status: AgentStatus,
    ) -> AgentInputs {
        AgentInputs {
            name: name.to_owned(),
            identity_id: IdentityId::new().unwrap(),
            persona_name: Some(name.to_owned()),
            status,
            is_running: running,
            is_ghost: false,
            cost_usd: cost,
            spawned_at: OffsetDateTime::from_unix_timestamp(1_700_000_000 + spawned_offset)
                .unwrap(),
            state_changed_at: None,
            conversation_tail: Vec::new(),
        }
    }

    fn ts(secs: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).unwrap()
    }

    /// A refusal at `secs` past the fixture epoch. Persona/layer/rationale are
    /// fixed; tests that need to vary them build the struct inline.
    fn refusal(agent_id: IdentityId, action: &str, secs: i64) -> AuthorityDecision {
        AuthorityDecision {
            timestamp: Some(ts(secs)),
            agent_id,
            persona_name: "worker".to_owned(),
            action: action.to_owned(),
            disposition: Disposition::Refuse,
            layer: Some("profile".to_owned()),
            rationale: Some("refused".to_owned()),
        }
    }

    // P_PENDING: build_snapshot keeps only refusals, resolves each one's
    // agent_id to the registry role name, and orders the panel newest first.
    #[test]
    fn build_snapshot_surfaces_refusals_with_resolved_names() {
        let now = now_at(1_700_100_000);
        let worker_id = IdentityId::new().unwrap();
        let mut worker = inputs("worker-abc", 100, true, 0.0);
        worker.identity_id = worker_id;

        let allow = AuthorityDecision {
            disposition: Disposition::Allow,
            layer: None,
            rationale: None,
            ..refusal(worker_id, "SendMessage(to=lead)", 3)
        };
        let decisions = vec![
            refusal(worker_id, "SpawnAgent(persona=worker)", 2),
            allow,
            refusal(worker_id, "SendMessage(to=ops)", 5),
        ];

        let snap = build_snapshot(
            &[inputs("lead", 0, true, 0.0), worker],
            &[],
            &decisions,
            0,
            None,
            now,
        );

        assert_eq!(
            snap.refusal_count, 2,
            "the allow is excluded from the count"
        );
        assert_eq!(snap.pending_decisions.len(), 2);
        // Newest first: ts(5) before ts(2).
        assert_eq!(snap.pending_decisions[0].action, "SendMessage(to=ops)");
        assert_eq!(
            snap.pending_decisions[0].agent_name, "worker-abc",
            "agent_id resolved to the registry role name"
        );
        assert_eq!(
            snap.pending_decisions[1].action,
            "SpawnAgent(persona=worker)"
        );
    }

    // P_PENDING_FALLBACK: a refusal whose agent_id is not in the registry
    // (a since-unregistered agent still in the log tail) falls back to its
    // persona name rather than dropping out of the panel.
    #[test]
    fn build_snapshot_pending_decision_falls_back_to_persona() {
        let now = now_at(1_700_100_000);
        let stranger = IdentityId::new().unwrap();
        let decisions = vec![AuthorityDecision {
            persona_name: "ghost-persona".to_owned(),
            ..refusal(stranger, "X()", 1)
        }];

        let snap = build_snapshot(&[], &[], &decisions, 0, None, now);
        assert_eq!(snap.pending_decisions.len(), 1);
        assert_eq!(snap.pending_decisions[0].agent_name, "ghost-persona");
    }

    // P_PENDING_CAP: the panel caps displayed rows at MAX_PENDING_DECISIONS
    // but the `▲ N` count reflects every refusal in the tail.
    #[test]
    fn build_snapshot_caps_pending_rows_but_counts_all_refusals() {
        let now = now_at(1_700_100_000);
        let id = IdentityId::new().unwrap();
        let decisions: Vec<AuthorityDecision> = (0..8).map(|i| refusal(id, "X()", i)).collect();

        let snap = build_snapshot(&[], &[], &decisions, 0, None, now);
        assert_eq!(snap.refusal_count, 8);
        assert_eq!(
            snap.pending_decisions.len(),
            MAX_PENDING_DECISIONS,
            "display capped even though all refusals are counted"
        );
    }

    // P1: lead is pinned at row one; within running, Working sorts above
    // Idle (Saskia: "attention is the scarce resource; sort by what demands
    // it"); stopped agents fall to the bottom.
    #[test]
    fn build_snapshot_sorts_lead_first_then_running_by_status_priority() {
        let now = now_at(1_700_100_000);
        let inputs = vec![
            inputs_with_status("worker-idle", 90_000, true, 0.10, AgentStatus::Idle),
            inputs_with_status("worker-working", 50_000, true, 0.20, AgentStatus::Working),
            inputs_with_status("worker-stopped", 500, false, 0.05, AgentStatus::Idle),
            inputs_with_status("lead", 30_000, true, 0.50, AgentStatus::Idle),
        ];

        let snap = build_snapshot(&inputs, &[], &[], 0, None, now);

        let order: Vec<&str> = snap.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            order,
            vec!["lead", "worker-working", "worker-idle", "worker-stopped"],
            "lead first; within running, Working above Idle; stopped last"
        );
    }

    // P2: within the same status bucket, oldest agent sorts first (the
    // stale one is usually the stuck one).
    #[test]
    fn build_snapshot_within_status_bucket_sorts_oldest_first() {
        let now = now_at(1_700_100_000);
        let inputs = vec![
            inputs_with_status("worker-young", 90_000, true, 0.10, AgentStatus::Working),
            inputs_with_status("worker-old", 1_000, true, 0.20, AgentStatus::Working),
        ];

        let snap = build_snapshot(&inputs, &[], &[], 0, None, now);
        let order: Vec<&str> = snap.agents.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            order,
            vec!["worker-old", "worker-young"],
            "oldest first within a status bucket"
        );
    }

    // P3: totals roll up across every agent's cost and the session elapsed
    // tracks the oldest agent in the table.
    #[test]
    fn build_snapshot_totals_cost_and_session_elapsed() {
        let now = now_at(1_700_100_000);
        let inputs = vec![
            inputs("lead", 50_000, true, 0.50),
            inputs("worker-a", 10_000, true, 0.10),
            inputs("worker-b", 90_000, false, 0.05),
        ];

        let snap = build_snapshot(&inputs, &[], &[], 0, None, now);
        assert!((snap.total_cost_usd - 0.65).abs() < 1e-9);
        assert_eq!(
            snap.session_elapsed,
            Some(time::Duration::seconds(
                1_700_100_000 - (1_700_000_000 + 10_000)
            ))
        );
    }

    // P4: events merge across agents and rank newest-first, dropping
    // entries that have no timestamp (they can't participate in a sort).
    #[test]
    fn build_snapshot_merges_events_newest_first_and_drops_undated() {
        let now = now_at(1_700_100_000);
        let mut a = inputs("worker-a", 0, true, 0.0);
        let mut b = inputs("worker-b", 0, true, 0.0);
        a.conversation_tail = vec![
            entry("hello from a", 100, EntryKind::Outbound),
            entry("system note a", 200, EntryKind::System),
        ];
        b.conversation_tail = vec![
            entry("hello from b", 150, EntryKind::Inbound),
            ConversationEntry {
                kind: EntryKind::System,
                text: "undated".to_owned(),
                timestamp: None,
                sender_id: None,
            },
        ];

        let snap = build_snapshot(&[a, b], &[], &[], 0, None, now);

        let order: Vec<(&str, EventKind)> = snap
            .recent_events
            .iter()
            .map(|e| (e.source.label(), e.kind))
            .collect();
        assert_eq!(
            order,
            vec![
                ("worker-a", EventKind::System),
                ("worker-b", EventKind::Msg),
                ("worker-a", EventKind::Msg),
            ],
            "undated entry must not appear in the merged stream"
        );
    }

    // P5: operator-authored entries surface as a single "you" row, not as
    // per-recipient noise. Saskia: "'you' is the only identity the operator
    // has zero ambiguity about — it's the anchor that makes the rest of the
    // stream legible."
    #[test]
    fn build_snapshot_attributes_operator_entries_to_you() {
        let now = now_at(1_700_100_000);
        let operator = op_id();
        let other = op_id();

        let mut worker = inputs("worker-a", 0, true, 0.0);
        worker.conversation_tail = vec![
            ConversationEntry {
                kind: EntryKind::Inbound,
                text: "from operator".to_owned(),
                timestamp: Some(OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap()),
                sender_id: Some(operator),
            },
            ConversationEntry {
                kind: EntryKind::Inbound,
                text: "from another agent".to_owned(),
                timestamp: Some(OffsetDateTime::from_unix_timestamp(1_700_000_050).unwrap()),
                sender_id: Some(other),
            },
        ];

        let snap = build_snapshot(&[worker], &[], &[], 0, Some(operator), now);

        let attribution: Vec<(&str, &str)> = snap
            .recent_events
            .iter()
            .map(|e| (e.source.label(), e.summary.as_str()))
            .collect();
        assert_eq!(
            attribution,
            vec![
                (OPERATOR_LABEL, "from operator"),
                ("worker-a", "from another agent"),
            ],
            "operator-authored entries get 'you'; peer-authored stays attributed to the recipient"
        );
    }

    // P6: when operator_id is None (e.g. registry lookup not yet complete)
    // the builder falls back to per-recipient attribution rather than
    // mislabeling.
    #[test]
    fn build_snapshot_without_operator_id_falls_back_to_recipient() {
        let now = now_at(1_700_100_000);
        let mut worker = inputs("worker-a", 0, true, 0.0);
        worker.conversation_tail = vec![ConversationEntry {
            kind: EntryKind::Inbound,
            text: "no operator known".to_owned(),
            timestamp: Some(OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap()),
            sender_id: Some(op_id()),
        }];

        let snap = build_snapshot(&[worker], &[], &[], 0, None, now);
        assert_eq!(snap.recent_events.len(), 1);
        assert_eq!(
            snap.recent_events[0].source,
            Source::Agent("worker-a".to_owned())
        );
    }

    // P7: quarantine count is threaded through; placeholder piles read zero.
    #[test]
    fn build_snapshot_threads_quarantine_count() {
        let now = now_at(1_700_100_000);
        let snap = build_snapshot(&[inputs("lead", 0, true, 0.0)], &[], &[], 7, None, now);
        assert_eq!(snap.queue_counts.quarantine, 7);
        assert_eq!(snap.queue_counts.memory, 0);
        assert_eq!(snap.queue_counts.config, 0);
        assert!(snap.queue_counts.cost_ok);
    }

    // P8: state_changed_at flows through to AgentRow so the renderer can
    // show time-in-state alongside the working sigil.
    #[test]
    fn build_snapshot_threads_state_changed_at() {
        let now = now_at(1_700_100_000);
        let changed = OffsetDateTime::from_unix_timestamp(1_700_099_988).unwrap();
        let mut row = inputs("lead", 0, true, 0.0);
        row.state_changed_at = Some(changed);
        let snap = build_snapshot(&[row], &[], &[], 0, None, now);
        assert_eq!(snap.agents[0].state_changed_at, Some(changed));
    }

    // P9: summarize_event collapses newlines so an event row stays single-line.
    #[test]
    fn summarize_event_collapses_multiline_text() {
        let s = summarize_event("first\n\nsecond\n  third  ");
        assert_eq!(s, "first \u{00B7} second \u{00B7} third");
    }

    // P10: build_event returns None for an undated entry — preserves the
    // "every recent event has a timestamp" invariant the renderer assumes.
    #[test]
    fn build_event_drops_undated_entries() {
        let e = ConversationEntry {
            kind: EntryKind::System,
            text: "no ts".to_owned(),
            timestamp: None,
            sender_id: None,
        };
        assert!(build_event("lead", &e, None).is_none());
    }

    // P11: integration coverage for the IO orchestrator. Builds a real
    // on-disk fixture (agent registry + two agent dirs with status, cost,
    // and quarantine files) and asserts `read_snapshot` walks it end-to-
    // end. The pure builder (P1–P10) covers merge/rank logic; this proves
    // the file paths and error-absorption wiring line up.
    #[test]
    #[cfg(unix)]
    fn read_snapshot_walks_registry_and_returns_per_agent_data() {
        use reeve_runtime::{
            AgentRecord, AgentRegistry, AgentStatus as RuntimeAgentStatus, ValidatedAgentName,
        };
        use std::fs;
        use std::os::unix::fs::PermissionsExt as _;
        use tempfile::tempdir;

        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        // The on-disk runtime layout uses 0o700 on every directory it owns;
        // tempdir() returns 0o755, which the AgentRegistry opener rejects.
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();

        // Provision two agent trees with realistic content the readers will
        // see: a lead with one conversation line and an idle status, and a
        // worker with a non-empty quarantine folder so the queue counter
        // can be asserted.
        let agents_dir = data_dir.join("agents");
        for (name, status_text, conversation, quarantine_count) in [
            (
                "lead",
                "idle",
                "{\"type\":\"outbound\",\"payload\":\"hi\"}\n",
                0,
            ),
            ("worker-test", "working", "", 2),
        ] {
            let dir = agents_dir.join(name);
            for sub in ["inbox/tmp", "inbox/new", "inbox/quarantine", "log"] {
                fs::create_dir_all(dir.join(sub)).unwrap();
                fs::set_permissions(dir.join(sub), fs::Permissions::from_mode(0o700)).unwrap();
            }
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(dir.join("status"), status_text).unwrap();
            fs::write(dir.join("cost"), "0.0").unwrap();
            fs::write(dir.join("log").join("conversation.jsonl"), conversation).unwrap();
            for q in 0..quarantine_count {
                fs::write(dir.join("inbox/quarantine").join(format!("env-{q}")), b"x").unwrap();
            }
        }
        fs::set_permissions(&agents_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let registry_path = agents_dir.join("registry.toml");
        let mut registry = AgentRegistry::open(registry_path.clone()).unwrap();
        for name in ["lead", "worker-test"] {
            registry
                .register(AgentRecord {
                    name: ValidatedAgentName::new(name).unwrap(),
                    identity_id: IdentityId::new().unwrap(),
                    inbox_dir: agents_dir.join(name).join("inbox"),
                    persona_name: Some(name.to_owned()),
                    spawned_at: OffsetDateTime::now_utc(),
                    status: RuntimeAgentStatus::Running,
                    stopped_reason: None,
                })
                .unwrap();
        }

        let snap = read_snapshot(&data_dir, &registry_path, None);

        let names: Vec<&str> = snap.agents.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"lead"), "lead must appear in snapshot");
        assert!(
            names.contains(&"worker-test"),
            "worker must appear in snapshot"
        );
        assert_eq!(snap.queue_counts.quarantine, 2, "quarantine files counted");
        let worker = snap
            .agents
            .iter()
            .find(|a| a.name == "worker-test")
            .unwrap();
        assert_eq!(
            worker.status,
            AgentStatus::Working,
            "worker status reflects on-disk file"
        );
    }

    // P12: a registry path that doesn't exist surfaces as an empty
    // snapshot rather than a panic — the "always renderable" contract.
    #[test]
    fn read_snapshot_returns_empty_when_registry_unreadable() {
        let snap = read_snapshot(Path::new("/nonexistent"), Path::new("/nonexistent"), None);
        assert!(snap.agents.is_empty());
        assert_eq!(snap.queue_counts.quarantine, 0);
    }

    // P13: read_snapshot sources engagements from the on-disk registry,
    // carrying name, state, root, and staffed unit through untouched.
    #[test]
    fn read_snapshot_reads_engagements_from_registry() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt as _;
        use tempfile::tempdir;

        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o700)).unwrap();

        let agents_dir = data_dir.join("agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::set_permissions(&agents_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let registry_path = agents_dir.join("registry.toml");
        AgentRegistry::open(registry_path.clone()).unwrap();

        let engagements =
            EngagementRegistry::open(RuntimeLayout::new(&data_dir).engagements_root()).unwrap();
        engagements
            .open_engagement(
                "billing",
                "reconcile ledgers",
                Some(PathBuf::from("/repo/billing")),
                OffsetDateTime::now_utc(),
            )
            .unwrap();
        engagements
            .set_staffed_unit(
                "billing",
                Some(StaffedUnit::Team {
                    name: "core-eng".to_owned(),
                }),
            )
            .unwrap();

        let snap = read_snapshot(&data_dir, &registry_path, None);

        assert_eq!(snap.engagements.len(), 1);
        let row = &snap.engagements[0];
        assert_eq!(row.name, "billing");
        assert_eq!(row.state, EngagementState::Open);
        assert_eq!(row.root, Some(PathBuf::from("/repo/billing")));
        assert_eq!(
            row.staffed_unit,
            Some(StaffedUnit::Team {
                name: "core-eng".to_owned()
            })
        );
    }
}
