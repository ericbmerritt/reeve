//! Agent actor: receive inbound envelopes, drive the adapter / tool-call loop,
//! and record the conversation journal, status, and cost.
//!
//! A single supervised actor that processes one [`ProcessInbound`] message at
//! a time. An inbound message drives the agent through one or more adapter
//! calls — text-only turns finish in one call; tool-use turns drive a loop:
//! the model returns tool calls, the agent dispatches them as [`InvokeTool`]
//! messages to the registered tool actors, collects [`ToolResult`] replies
//! into the conversation history, and calls the adapter again. The loop
//! terminates on `FinishReason::EndTurn` or when `MAX_TOOL_ITERATIONS` is
//! reached (runaway guard).
//!
//! Lifecycle:
//! - `started` — writes `"idle"` to the status file and records a system entry.
//! - `restarting` — re-writes `"idle"` after supervisor-driven restart.
//! - `Handler<ProcessInbound>` — transitions status to `"working"`, calls the
//!   adapter, dispatches any tool calls, and returns to `"idle"` once the
//!   loop terminates.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use actix::{Actor, ActorContext, AsyncContext, Context, Handler, Recipient, Supervised};
use time::OffsetDateTime;
use tracing::{debug, info, warn};

use crate::agent_fs::{
    AgentDirs, AgentFsError, AtomicFileWriter, ConversationEntry, ConversationThread,
};
use crate::audit::{AuditEvent, AuditLog, AuthorityDisposition};
use crate::capability::Thresholds;
use crate::model_resolution::SpawnSnapshot;
use crate::tool::{InvokeTool, Refusal, ToolResult};

// ── History loader ────────────────────────────────────────────────────────────

/// Reconstruct the in-memory conversation history from an existing journal.
///
/// `Inbound` entries become user-role messages; `Outbound` entries become
/// assistant-role messages; `ToolUse` entries become assistant-role
/// `ToolUse` content blocks; `ToolResult` entries become user-role
/// `ToolResult` content blocks. `System` and `ModelCall` entries are
/// skipped — they carry telemetry or annotations that do not belong in the
/// adapter's context window.
///
/// **Consecutive same-role merging:** The Anthropic API requires that any
/// preamble text and `tool_use` blocks for a single assistant turn appear in
/// one message, and that all `tool_result` blocks for a single batch appear in
/// one user message. The live recording path writes `Outbound` then `ToolUse`
/// entries for a mixed turn, and multiple `ToolResult` entries for a batch.
/// Consecutive journal entries that map to the same role are merged into one
/// `Message` by appending their content blocks rather than pushing a new
/// message, reconstructing the same shape the adapter originally received.
///
/// Also collects `message_id`s from completed `Inbound` turns into a [`SeenIds`] set in
/// journal insertion order.
///
/// A turn is *completed* when its final `ModelCall` is not followed by a
/// Format an inbound payload for the model's view of the conversation with a
/// sender-attribution prefix. The runtime cannot model "another agent" as a
/// distinct role under the Anthropic API (every turn is User or Assistant),
/// so a worker's reply and an operator's prompt would otherwise be
/// indistinguishable user-role text. Prefixing keeps the disambiguation
/// inside the content block.
///
/// Format: `[from <sender_id>]\n<payload>`. The UUID is used directly rather
/// than the agent name because name resolution would require threading the
/// `AgentRegistry` into the agent actor; until that lands, the operator and
/// peers each show up as a stable identifier the model can reason about.
///
/// Legacy journal entries deserialize with `sender_id = None`; for those the
/// raw payload is returned unchanged.
pub(crate) fn format_inbound_payload(
    sender_id: Option<reeve_types::IdentityId>,
    payload: &str,
) -> String {
    match sender_id {
        Some(id) => format!("[from {id}]\n{payload}"),
        None => payload.to_owned(),
    }
}

/// `ToolUse` entry before the next `Inbound` or end of journal. An `Inbound`
/// with an incomplete round-trip (crash before the final `ModelCall`, or
/// a `ModelCall` followed by a dangling `ToolUse`) is an interrupted turn.
/// The watcher re-delivers that envelope, so its `message_id` must NOT be
/// added to `seen_ids` — doing so would silently drop the re-delivery and
/// leave the turn permanently unprocessed.
///
/// Returns `Ok((Vec::new(), SeenIds::new()))` when the journal file is absent
/// (normal: first run). Returns `Err(io::Error)` for all other I/O failures
/// (permission denied, disk fault, etc.).
#[expect(
    clippy::too_many_lines,
    reason = "the match arms each handle a distinct journal entry variant; \
              splitting into sub-functions would fragment tightly coupled logic \
              without reducing actual complexity"
)]
pub(crate) fn load_history_from_journal(
    path: &Path,
) -> Result<(Vec<reeve_adapter::Message>, SeenIds), std::io::Error> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), SeenIds::new()));
        }
        Err(err) => return Err(err),
    };

    let mut history: Vec<reeve_adapter::Message> = Vec::new();
    let mut seen_ids = SeenIds::new();
    // We commit to `seen_ids` only when we can confirm the full round-trip
    // is durable — preventing crash-recovery re-delivery from being silently dropped.
    let mut pending_inbound_id: Option<String> = None;

    // State-machine flags for commit deferral: commit pending_inbound_id only
    // when the entire tool-call chain is durably closed — not at an
    // intermediate ModelCall that may be followed by another ToolUse.
    //
    // Runtime journal write order for a two-iteration tool turn:
    //   Inbound → MC(1) → TU(a) → TR(a) → MC(2) → TU(b) → TR(b) → MC(3)
    // Committing at MC(2) is wrong: if the process crashes after TU(b) the
    // journal ends at TU(b), but the id is already in seen_ids, so re-delivery
    // is silently dropped and the turn is permanently unprocessable.
    //
    // Deferral strategy: commit only when a ModelCall is NOT immediately
    // followed by a ToolUse. `tool_use_after_last_mc` is reset to false on
    // each ModelCall and set to true on each ToolUse. The commit is deferred
    // to (a) the next Inbound boundary or (b) end-of-journal, whichever comes
    // first, and only when `model_call_seen && !tool_use_after_last_mc`.
    let mut model_call_seen: bool = false;
    let mut tool_use_after_last_mc: bool = false;

    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let entry: ConversationEntry = serde_json::from_str(line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

        let (role, block) = match entry {
            ConversationEntry::Inbound {
                message_id,
                sender_id,
                payload,
                ..
            } => {
                // Deferred commit from the previous turn: if the last MC was
                // not followed by a TU, the previous turn completed cleanly.
                if model_call_seen && !tool_use_after_last_mc {
                    if let Some(id) = pending_inbound_id.take() {
                        seen_ids.insert(id);
                    }
                }
                pending_inbound_id = Some(message_id);
                model_call_seen = false;
                tool_use_after_last_mc = false;
                (
                    reeve_adapter::Role::User,
                    reeve_adapter::MessageContent::Text(format_inbound_payload(
                        sender_id, &payload,
                    )),
                )
            }
            ConversationEntry::Outbound { payload, .. } => (
                reeve_adapter::Role::Assistant,
                reeve_adapter::MessageContent::Text(payload),
            ),
            ConversationEntry::ToolUse {
                tool_use_id,
                name,
                input,
                ..
            } => {
                tool_use_after_last_mc = true;
                (
                    reeve_adapter::Role::Assistant,
                    reeve_adapter::MessageContent::ToolUse {
                        id: tool_use_id,
                        name,
                        input,
                    },
                )
            }
            ConversationEntry::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => (
                reeve_adapter::Role::User,
                reeve_adapter::MessageContent::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                },
            ),
            ConversationEntry::System { .. } => continue,
            ConversationEntry::ModelCall { .. } => {
                model_call_seen = true;
                tool_use_after_last_mc = false;
                continue;
            }
        };

        match history.last_mut() {
            Some(last) if last.role == role => {
                last.content.push(block);
            }
            _ => {
                history.push(reeve_adapter::Message {
                    role,
                    content: vec![block],
                });
            }
        }
    }
    // Crash recovery: if the process died after writing an Inbound entry but
    // before writing the Outbound reply, the history ends with a User message.
    // Re-submitting that User turn would cause an API 400 ("messages: final
    // assistant turn…"). Truncate trailing User messages — the watcher
    // re-delivers the envelope, so the turn is not lost.
    while history
        .last()
        .map(|m| m.role == reeve_adapter::Role::User)
        .unwrap_or(false)
    {
        history.pop();
    }
    // Deferred commit for the last turn in the journal: commit when the last
    // ModelCall was not followed by a ToolUse (turn complete). A bare Inbound
    // with no ModelCall, or a ModelCall followed by a ToolUse (turn still
    // open — crash in the tool round-trip), are both not committed so the
    // watcher can re-deliver for retry.
    if model_call_seen && !tool_use_after_last_mc {
        if let Some(id) = pending_inbound_id.take() {
            seen_ids.insert(id);
        }
    }
    Ok((history, seen_ids))
}

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors produced by the agent actor and its constructor.
#[derive(Debug)]
pub enum AgentError {
    /// Filesystem or JSONL journal error.
    Fs(AgentFsError),
    /// Error returned by the model adapter.
    Adapter(reeve_adapter::AdapterError),
    /// Unclassified I/O error with path context.
    Io {
        /// File that could not be opened or written.
        path: PathBuf,
        /// Underlying OS error.
        source: std::io::Error,
    },
    /// JSON serialization or deserialization error.
    Json(serde_json::Error),
    /// The `tools` slice passed to [`Agent::new`] contained two bindings with
    /// the same tool name. The agent cannot route to two recipients under one
    /// name, and the adapter would reject a request with duplicate tool names
    /// anyway.
    DuplicateToolName(String),
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fs(source) => write!(f, "agent fs error: {source}"),
            Self::Adapter(source) => write!(f, "adapter error: {source}"),
            Self::Io { path, source } => {
                write!(f, "agent IO at {}: {source}", path.display())
            }
            Self::Json(source) => write!(f, "agent json error: {source}"),
            Self::DuplicateToolName(name) => {
                write!(f, "duplicate tool name in agent constructor: {name}")
            }
        }
    }
}

impl std::error::Error for AgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fs(source) => Some(source),
            Self::Adapter(source) => Some(source),
            Self::Io { source, .. } => Some(source),
            Self::Json(source) => Some(source),
            Self::DuplicateToolName(_) => None,
        }
    }
}

// ── ProcessInbound message ────────────────────────────────────────────────────

/// Notify the agent that an inbound envelope was quarantined.
///
/// The agent appends a system entry to the conversation thread so the operator
/// can see transport rejections without reading daemon logs.
pub struct QuarantineEvent {
    /// Human-readable quarantine reason (e.g. `"signature_invalid"`).
    pub reason: String,
}

impl actix::Message for QuarantineEvent {
    type Result = ();
}

/// Deliver an inbound envelope payload to the agent for processing.
///
/// The actor appends the payload to its conversation journal, calls the
/// adapter with the full in-memory history, and records the response and token
/// counts before returning to idle.
pub struct ProcessInbound {
    /// Payload text extracted from the inbound envelope.
    pub payload: String,
    /// Stable identifier from the message envelope.
    pub message_id: String,
    /// Identity that signed the envelope. Threaded through to the journal
    /// and to the model's view of the conversation so a worker's reply is
    /// not silently rendered as if it came from the operator.
    pub sender_id: reeve_types::IdentityId,
}

impl actix::Message for ProcessInbound {
    type Result = ();
}

// ── ToolBatchTimeout (internal) ───────────────────────────────────────────────

/// Self-message scheduled when a tool batch is dispatched. Fires after
/// [`TOOL_TIMEOUT`] elapses; the handler synthesizes error results for any
/// tool calls still pending and resumes the adapter loop. Late-arriving
/// `ToolResult` messages are ignored (their `tool_use_id` is no longer in
/// the pending set).
struct ToolBatchTimeout;

impl actix::Message for ToolBatchTimeout {
    type Result = ();
}

// ── Agent actor ───────────────────────────────────────────────────────────

/// Supervised actix actor that implements the agent's message loop.
///
/// Calls the registered adapter with the accumulated conversation history
/// and records all exchanges in an append-only JSONL journal.
pub struct Agent {
    /// Adapter used for all model calls.
    adapter: Arc<dyn reeve_adapter::Adapter>,
    /// Resolved adapter and persona information captured at spawn time.
    snapshot: SpawnSnapshot,
    /// System prompt forwarded to the adapter on every call.
    system_prompt: String,
    /// Append-only JSONL conversation journal.
    conversation: ConversationThread,
    /// Atomic writer for the `status` file (`"idle"` / `"working"`).
    status_writer: AtomicFileWriter,
    /// Atomic writer for the `cost` file (cumulative USD, 6 decimal places).
    cost_writer: AtomicFileWriter,
    /// Cumulative cost in microdollars (USD × `1_000_000`) across all adapter
    /// calls in this session. Stored as `u64` for lossless aggregation;
    /// converted to `f64` only at display boundaries.
    total_cost_microdollars: u64,
    /// True while an adapter call is in flight; new messages are discarded
    /// until the current call completes.
    in_flight: bool,
    /// In-memory conversation history forwarded to the adapter on each call.
    ///
    /// Grows monotonically; reset only when the actor is dropped or restarted
    /// (the on-disk journal is the durable record). When the cumulative history
    /// exceeds the adapter's context window, adapter calls will fail with
    /// `BadRequest`; the actor will return to idle and the next message will
    /// retry with the full history. Reset the agent (restart) to clear the
    /// in-memory history.
    history: Vec<reeve_adapter::Message>,
    /// Tool descriptors advertised to the adapter on every call.
    tool_descriptors: Vec<reeve_adapter::Tool>,
    /// Routes from tool name to the receiving tool actor.
    tool_routes: HashMap<String, Recipient<InvokeTool>>,
    /// `tool_use_id`s of tool calls dispatched in the current batch but not
    /// yet answered. Empty when no tool batch is in flight.
    pending_tool_use_ids: HashSet<String>,
    /// Tool-result content blocks accumulated for the next user turn.
    /// Drained when the batch completes and the next adapter call fires.
    pending_results: Vec<reeve_adapter::MessageContent>,
    /// Adapter calls executed for the current `ProcessInbound`. Reset to 0
    /// at the start of each inbound message; incremented before each call.
    /// Capped by [`MAX_TOOL_ITERATIONS`].
    tool_iteration: u32,
    /// Identity of the agent itself, supplied as `sender_id` on every
    /// [`InvokeTool`] dispatch. The authority hook reads this when deciding
    /// whether the invocation is permitted.
    #[expect(
        clippy::struct_field_names,
        reason = "agent_id is the conventional name for the actor's identity; \
                  renaming would obscure the field's purpose"
    )]
    agent_id: reeve_types::IdentityId,
    /// Inbound messages received while a turn is in flight. Drained at the
    /// end of each turn (`FinishReason::EndTurn`); their payloads form the
    /// next user turn so the model integrates the operator's follow-ups
    /// instead of the runtime dropping them.
    pending_inbound: VecDeque<ProcessInbound>,
    /// Ed25519 keypair for this agent instance. Held in memory only; never
    /// serialized. Used for envelope signing in later phases.
    #[expect(
        dead_code,
        reason = "envelope signing consumed in a later phase; field committed now so the private key has exactly one in-memory home"
    )]
    keypair: reeve_types::Keypair,
    /// Snapshotted cost and concurrency thresholds from the persona profile.
    /// `None` fields mean no limit. Checked before every adapter call.
    thresholds: Thresholds,
    /// Audit log for `authority.decision` events. `None` when the daemon
    /// did not provide one (e.g., test harnesses).
    audit: Option<Arc<AuditLog>>,
    /// Root of the Reeve data directory. Used by the session cost meter to
    /// walk all agent cost files when checking `cost_per_session`.
    data_dir: PathBuf,
    /// `message_id`s the agent has already accepted (whether processed or
    /// queued). Duplicate `ProcessInbound` messages with the same id are
    /// dropped silently.
    ///
    /// Why: the `inbox/cur/` watcher scans the entire directory on every
    /// filesystem event for cross-platform robustness, so a single arriving
    /// envelope can produce multiple `ProcessInbound` messages for the same
    /// file (one per scan that observes it). Without this dedup the agent
    /// would re-process the same envelope after each subsequent arrival.
    ///
    /// Bounded by [`SEEN_MESSAGE_IDS_CAP`]: oldest entries are evicted in
    /// FIFO order. Eviction is safe because `inbox/cur/` rotation moves
    /// envelopes to `archive/` after the cur retention window, so an
    /// envelope evicted from this set has also vanished from the directory
    /// the watcher scans.
    seen_message_ids: SeenIds,
}

/// Maximum number of `message_id`s the agent retains for dedup. 4096 covers
/// a comfortably high message arrival rate over the 24-hour cur/ retention
/// window (one message every ~21 seconds for a full day).
const SEEN_MESSAGE_IDS_CAP: usize = 4096;

/// Bounded FIFO set of `message_id`s. Insertion is O(1); when the set hits
/// [`SEEN_MESSAGE_IDS_CAP`] the oldest entry is evicted to make room. Used
/// for inbound dedup; see the field-level comment on [`Agent::seen_message_ids`].
#[derive(Debug, Default)]
pub(crate) struct SeenIds {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenIds {
    fn new() -> Self {
        Self::default()
    }

    /// Loads `ids` in order, keeping only the last [`SEEN_MESSAGE_IDS_CAP`]
    /// entries when `ids` exceeds the cap (oldest are dropped). Duplicates
    /// are skipped (first occurrence wins).
    #[cfg(test)]
    fn from_vec(ids: &[String]) -> Self {
        let mut this = Self::default();
        let start = ids.len().saturating_sub(SEEN_MESSAGE_IDS_CAP);
        for id in &ids[start..] {
            this.insert(id.clone());
        }
        this
    }

    #[cfg(test)]
    fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.order.len()
    }

    /// Returns ids in insertion order (`VecDeque`, not `HashSet`).
    #[cfg(test)]
    fn iter(&self) -> impl Iterator<Item = &String> {
        self.order.iter()
    }

    /// Insert `id`. Returns `true` if the id is new (insert took effect),
    /// `false` if it was already present (caller should treat as duplicate).
    fn insert(&mut self, id: String) -> bool {
        if !self.set.insert(id.clone()) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > SEEN_MESSAGE_IDS_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            }
        }
        true
    }
}

/// Default maximum tokens for the adapter response.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Maximum number of consecutive tool-use rounds the agent will execute for
/// one inbound message before aborting with a runaway-guard system entry.
///
/// A turn that needs more than this many tool calls in succession is almost
/// always a model loop, not a useful chain. The agent records the abort and
/// returns to idle so the operator can intervene.
const MAX_TOOL_ITERATIONS: u32 = 16;

/// How long the agent waits for all tool actors to reply with [`ToolResult`]
/// before synthesizing error results for any still-pending invocations.
///
/// 30 seconds covers ordinary tool latency (filesystem, lightweight subprocess
/// calls) without leaving the agent stuck on a wedged tool actor. Tools with
/// genuinely long-running work should structure their handler to return
/// quickly with an in-progress acknowledgment and stream completion separately
/// — that pattern is not used by any tool yet.
const TOOL_TIMEOUT: Duration = Duration::from_secs(30);

impl Agent {
    /// Construct an `Agent`.
    ///
    /// Opens the conversation journal and creates atomic writers for the
    /// status and cost files. Does not start the actor; call
    /// [`actix::Supervisor::start`] with a closure that invokes this.
    ///
    /// `tools` carries the adapter-facing descriptor and the actor route for
    /// each tool the agent may invoke. The descriptor is forwarded to the
    /// model on every adapter call; the route receives [`InvokeTool`]
    /// messages when the model returns a matching tool call. An agent with
    /// an empty tools vector behaves as a text-only agent.
    #[expect(
        clippy::too_many_arguments,
        reason = "agent constructor wires together ten independent collaborators; \
                  bundling into a struct trades clarity for indirection"
    )]
    pub fn new(
        adapter: Arc<dyn reeve_adapter::Adapter>,
        dirs: &AgentDirs,
        snapshot: SpawnSnapshot,
        system_prompt: String,
        agent_id: reeve_types::IdentityId,
        keypair: reeve_types::Keypair,
        tools: Vec<(reeve_adapter::Tool, Recipient<InvokeTool>)>,
        thresholds: Thresholds,
        audit: Option<Arc<AuditLog>>,
        data_dir: PathBuf,
    ) -> Result<Self, AgentError> {
        let conversation_path = dirs.conversation_path();
        let conversation = ConversationThread::open(&conversation_path).map_err(AgentError::Fs)?;
        let status_writer = AtomicFileWriter::new(dirs.status_path()).map_err(AgentError::Fs)?;
        let cost_writer = AtomicFileWriter::new(dirs.cost_path()).map_err(AgentError::Fs)?;

        let (history, seen_message_ids) =
            load_history_from_journal(&conversation_path).map_err(|source| AgentError::Io {
                path: conversation_path.clone(),
                source,
            })?;

        let mut tool_descriptors = Vec::with_capacity(tools.len());
        let mut tool_routes = HashMap::with_capacity(tools.len());
        for (descriptor, recipient) in tools {
            if tool_routes.contains_key(&descriptor.name) {
                return Err(AgentError::DuplicateToolName(descriptor.name));
            }
            tool_routes.insert(descriptor.name.clone(), recipient);
            tool_descriptors.push(descriptor);
        }

        Ok(Self {
            adapter,
            snapshot,
            system_prompt,
            conversation,
            status_writer,
            cost_writer,
            total_cost_microdollars: 0u64,
            in_flight: false,
            history,
            tool_descriptors,
            tool_routes,
            pending_tool_use_ids: HashSet::new(),
            pending_results: Vec::new(),
            tool_iteration: 0,
            agent_id,
            keypair,
            pending_inbound: VecDeque::new(),
            seen_message_ids,
            thresholds,
            audit,
            data_dir,
        })
    }

    /// Write `"idle"` to the status file; stop the actor on failure.
    fn set_idle(&self, ctx: &mut Context<Self>) {
        if self.status_writer.write("idle").is_err() {
            ctx.stop();
        }
    }

    /// Append a system entry to the conversation journal; stop on failure.
    fn append_system_entry(&self, message: &str, ctx: &mut Context<Self>) {
        let entry = ConversationEntry::System {
            message: message.to_owned(),
            timestamp_utc: OffsetDateTime::now_utc(),
        };
        if self.conversation.append(&entry).is_err() {
            ctx.stop();
        }
    }

    /// Append an inbound entry to the journal.
    ///
    /// Returns `true` on success. On failure stops the actor and returns
    /// `false` so the caller can skip subsequent steps.
    fn append_inbound(
        &self,
        message_id: &str,
        sender_id: reeve_types::IdentityId,
        payload: &str,
        ctx: &mut Context<Self>,
    ) -> bool {
        let entry = ConversationEntry::Inbound {
            message_id: message_id.to_owned(),
            sender_id: Some(sender_id),
            payload: payload.to_owned(),
            timestamp_utc: OffsetDateTime::now_utc(),
        };
        if self.conversation.append(&entry).is_err() {
            ctx.stop();
            return false;
        }
        true
    }

    /// Record an adapter response and either continue the tool loop or
    /// finish the turn.
    ///
    /// Common bookkeeping (journal entries, history, cost) runs every time.
    /// The branch on `finish_reason` drives the outcome:
    /// - [`reeve_adapter::FinishReason::ToolUse`] dispatches the requested
    ///   tool calls; the agent stays `in_flight` and waits for `ToolResult`
    ///   messages.
    /// - Any other variant (typically `EndTurn`) ends the turn and returns
    ///   the agent to idle.
    fn handle_response(&mut self, response: &reeve_adapter::Response, ctx: &mut Context<Self>) {
        let text = extract_response_text(&response.content);

        // Push the assistant turn to history with both text and tool_use
        // blocks, in that order, so the next adapter call carries the full
        // turn context the model emitted.
        let mut assistant_blocks: Vec<reeve_adapter::MessageContent> = response.content.clone();
        for tc in &response.tool_calls {
            assistant_blocks.push(reeve_adapter::MessageContent::ToolUse {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input: tc.arguments.clone(),
            });
        }
        self.history.push(reeve_adapter::Message {
            role: reeve_adapter::Role::Assistant,
            content: assistant_blocks,
        });

        if !self.append_outbound_and_model_call(&text, response, ctx) {
            return;
        }

        self.total_cost_microdollars += response.cost.microdollars;
        let display_usd = reeve_adapter::CostEstimate {
            microdollars: self.total_cost_microdollars,
        }
        .usd();
        if self
            .cost_writer
            .write(&format!("{display_usd:.6}"))
            .is_err()
        {
            ctx.stop();
            return;
        }

        if response.finish_reason == reeve_adapter::FinishReason::ToolUse {
            self.dispatch_tool_calls(&response.tool_calls, ctx);
        } else {
            // Turn complete. Reset the tool iteration counter, then either
            // start a queued turn (if messages arrived during this one) or
            // go idle. `in_flight` stays true across queued-turn handoff so
            // a third message arriving in this window stays queued.
            self.tool_iteration = 0;
            if self.pending_inbound.is_empty() {
                self.in_flight = false;
                self.set_idle(ctx);
            } else {
                self.drain_pending_into_turn(ctx);
            }
        }
    }

    /// Dispatch a batch of tool calls returned by the model and arm the
    /// timeout watchdog.
    ///
    /// Each call produces a [`ConversationEntry::ToolUse`] journal entry and
    /// an [`InvokeTool`] message to the matching tool actor. Calls whose
    /// `name` does not resolve to a registered tool are answered immediately
    /// with a synthetic error result rather than dispatched.
    fn dispatch_tool_calls(
        &mut self,
        tool_calls: &[reeve_adapter::ToolCall],
        ctx: &mut Context<Self>,
    ) {
        debug_assert!(
            self.pending_tool_use_ids.is_empty() && self.pending_results.is_empty(),
            "dispatch_tool_calls invoked with non-empty pending state"
        );

        let reply_to: Recipient<ToolResult> = ctx.address().recipient();

        for tc in tool_calls {
            let entry = ConversationEntry::ToolUse {
                tool_use_id: tc.id.clone(),
                name: tc.name.clone(),
                input: tc.arguments.clone(),
                timestamp_utc: OffsetDateTime::now_utc(),
            };
            if self.conversation.append(&entry).is_err() {
                ctx.stop();
                return;
            }

            match self.tool_routes.get(&tc.name) {
                Some(route) => {
                    self.pending_tool_use_ids.insert(tc.id.clone());
                    route.do_send(InvokeTool {
                        tool_use_id: tc.id.clone(),
                        name: tc.name.clone(),
                        input: tc.arguments.clone(),
                        sender_id: self.agent_id,
                        reply_to: reply_to.clone(),
                    });
                }
                None => {
                    // Unknown tool: synthesize an error result locally so the
                    // adapter loop continues. The model will see the error and
                    // can correct course.
                    self.append_tool_result(
                        &tc.id,
                        &format!("unknown tool: {}", tc.name),
                        true,
                        ctx,
                    );
                }
            }
        }

        if self.pending_tool_use_ids.is_empty() {
            // All tools were unknown; advance immediately.
            self.advance_after_tools(ctx);
        } else {
            ctx.run_later(TOOL_TIMEOUT, |_actor, inner_ctx| {
                inner_ctx.address().do_send(ToolBatchTimeout);
            });
        }
    }

    /// Append a tool-result journal entry and accumulate the corresponding
    /// content block for the next user turn.
    fn append_tool_result(
        &mut self,
        tool_use_id: &str,
        content: &str,
        is_error: bool,
        ctx: &mut Context<Self>,
    ) {
        let entry = ConversationEntry::ToolResult {
            tool_use_id: tool_use_id.to_owned(),
            content: content.to_owned(),
            is_error,
            timestamp_utc: OffsetDateTime::now_utc(),
        };
        if self.conversation.append(&entry).is_err() {
            ctx.stop();
            return;
        }
        self.pending_results
            .push(reeve_adapter::MessageContent::ToolResult {
                tool_use_id: tool_use_id.to_owned(),
                content: content.to_owned(),
                is_error,
            });
    }

    /// Push the accumulated tool-result blocks to history as a user turn and
    /// fire the next adapter call.
    fn advance_after_tools(&mut self, ctx: &mut Context<Self>) {
        let blocks = std::mem::take(&mut self.pending_results);
        if !blocks.is_empty() {
            self.history.push(reeve_adapter::Message {
                role: reeve_adapter::Role::User,
                content: blocks,
            });
        }
        self.spawn_adapter_call(ctx);
    }

    /// Append the outbound and model-call entries to the conversation journal.
    ///
    /// Skips the outbound entry when `text` is empty — for example, an
    /// assistant turn that consists only of tool-use blocks with no preamble
    /// text. The model-call entry is always recorded since it carries token
    /// and cost telemetry that does not depend on text presence.
    ///
    /// Returns `true` on success. On failure stops the actor and returns
    /// `false`.
    fn append_outbound_and_model_call(
        &self,
        text: &str,
        response: &reeve_adapter::Response,
        ctx: &mut Context<Self>,
    ) -> bool {
        if !text.is_empty() {
            let outbound = ConversationEntry::Outbound {
                payload: text.to_owned(),
                timestamp_utc: OffsetDateTime::now_utc(),
            };
            if self.conversation.append(&outbound).is_err() {
                ctx.stop();
                return false;
            }
        }

        let model_call = ConversationEntry::ModelCall {
            input_tokens: response.tokens.input,
            output_tokens: response.tokens.output,
            model: self.snapshot.model().to_owned(),
            timestamp_utc: OffsetDateTime::now_utc(),
        };
        if self.conversation.append(&model_call).is_err() {
            ctx.stop();
            return false;
        }
        true
    }

    /// Spawn the async adapter call into the actor's context.
    ///
    /// Increments [`Self::tool_iteration`] before firing. If the iteration
    /// would exceed [`MAX_TOOL_ITERATIONS`] the call is aborted, a system
    /// entry is recorded, and the agent returns to idle — the runaway guard.
    /// Emit a threshold refusal: append a system journal entry with the
    /// serialized `Refusal`, emit an `authority.decision` audit event, and
    /// return the agent to idle. Returns `true` so the caller can early-return.
    fn refuse_threshold(&mut self, refusal: &Refusal, ctx: &mut Context<Self>) -> bool {
        // Human-readable message for the panopticon recent-events stream.
        // Structured data lives in the audit log; Phase 5 surfaces this
        // properly in the Decisions tab.
        let msg = match refusal {
            Refusal::Threshold {
                name,
                current,
                limit,
                ..
            } => {
                format!("refused ({name}): ${current} \u{2265} limit ${limit}")
            }
            Refusal::Profile { .. } | Refusal::Blacklist { .. } => refusal.rationale().to_owned(),
        };
        self.append_system_entry(&msg, ctx);
        if let Some(audit) = &self.audit {
            let event = AuditEvent::AuthorityDecision {
                agent_id: self.agent_id,
                persona_name: self.snapshot.persona_name.clone(),
                profile_version: self.snapshot.persona_version,
                action: format!("{} threshold", refusal.layer()),
                disposition: AuthorityDisposition::Refuse,
                layer: Some(refusal.layer().to_owned()),
                rationale: Some(refusal.rationale().to_owned()),
                blacklist_version: None,
                at: OffsetDateTime::now_utc(),
            };
            let _ = audit.append(&event);
        }
        self.in_flight = false;
        self.tool_iteration = 0;
        self.set_idle(ctx);
        true
    }

    /// Check `cost_per_agent` and `cost_per_session` thresholds before an
    /// adapter call. Returns `true` and goes idle if either threshold is
    /// exceeded; returns `false` if the call may proceed.
    fn check_cost_thresholds(&mut self, ctx: &mut Context<Self>) -> bool {
        let current_usd = reeve_adapter::CostEstimate {
            microdollars: self.total_cost_microdollars,
        }
        .usd();

        if let Some(limit) = self.thresholds.cost_per_agent {
            if current_usd >= limit {
                let refusal = Refusal::Threshold {
                    name: "cost_per_agent".to_owned(),
                    current: format!("{current_usd:.6}"),
                    limit: format!("{limit:.6}"),
                    rationale: format!(
                        "agent cost {current_usd:.6} USD reached limit {limit:.6} USD"
                    ),
                };
                return self.refuse_threshold(&refusal, ctx);
            }
        }

        if let Some(limit) = self.thresholds.cost_per_session {
            let session_usd = crate::cost_meter::session_cost_usd(&self.data_dir);
            if session_usd >= limit {
                let refusal = Refusal::Threshold {
                    name: "cost_per_session".to_owned(),
                    current: format!("{session_usd:.6}"),
                    limit: format!("{limit:.6}"),
                    rationale: format!(
                        "session cost {session_usd:.6} USD reached limit {limit:.6} USD"
                    ),
                };
                return self.refuse_threshold(&refusal, ctx);
            }
        }

        false
    }

    fn spawn_adapter_call(&mut self, ctx: &mut Context<Self>) {
        use actix::fut::WrapFuture as _;
        use actix::ActorFutureExt as _;

        self.tool_iteration += 1;
        if self.tool_iteration > MAX_TOOL_ITERATIONS {
            warn!(
                iteration = self.tool_iteration,
                "tool loop exceeded MAX_TOOL_ITERATIONS; aborting"
            );
            self.append_system_entry(
                &format!("tool loop aborted: exceeded {MAX_TOOL_ITERATIONS} iterations"),
                ctx,
            );
            self.in_flight = false;
            self.tool_iteration = 0;
            self.set_idle(ctx);
            return;
        }

        if self.check_cost_thresholds(ctx) {
            return;
        }

        debug!(
            model = %self.snapshot.model(),
            history_len = self.history.len(),
            iteration = self.tool_iteration,
            "calling adapter"
        );
        let adapter = Arc::clone(&self.adapter);
        let messages = self.history.clone();
        let tools = self.tool_descriptors.clone();
        let params = reeve_adapter::Params {
            max_tokens: DEFAULT_MAX_TOKENS,
            system_prompt: Some(self.system_prompt.clone()),
            ..reeve_adapter::Params::default()
        };
        let fut = async move { adapter.call(&messages, &tools, &params).await }
            .into_actor(self)
            .map(|result, actor, inner_ctx| match result {
                Ok(response) => {
                    info!(
                        input_tokens = response.tokens.input,
                        output_tokens = response.tokens.output,
                        finish_reason = ?response.finish_reason,
                        "response received"
                    );
                    actor.handle_response(&response, inner_ctx);
                }
                Err(err) => {
                    actor.in_flight = false;
                    actor.tool_iteration = 0;
                    actor.history.pop();
                    warn!(err = %err, "adapter call failed");
                    // Adapter error: log to journal (best-effort) then go idle.
                    // The primary error is the adapter failure; a journal error
                    // here does not compound the failure.
                    let entry = ConversationEntry::System {
                        message: format!("adapter call failed: {err}"),
                        timestamp_utc: OffsetDateTime::now_utc(),
                    };
                    let _ = actor.conversation.append(&entry);
                    actor.set_idle(inner_ctx);
                }
            });
        ctx.spawn(fut);
    }
}

// ── Actor impl ────────────────────────────────────────────────────────────────

impl Actor for Agent {
    type Context = Context<Self>;

    /// Initialize the agent: record the start event and write idle status.
    fn started(&mut self, ctx: &mut Context<Self>) {
        info!(adapter = %self.snapshot.adapter_id, "agent ready");
        self.append_system_entry("agent started", ctx);
        self.set_idle(ctx);
    }
}

impl Supervised for Agent {
    /// Recover after a supervised restart: restore idle status without
    /// re-logging the start event. Clears any in-flight tool batch state
    /// and any queued inbound messages so a fresh inbound message starts
    /// cleanly. Queued messages are dropped on restart because the prior
    /// turn's history is gone — replaying them out of context would surprise
    /// the model.
    fn restarting(&mut self, ctx: &mut Context<Self>) {
        warn!("agent restarting");
        self.in_flight = false;
        self.tool_iteration = 0;
        self.pending_tool_use_ids.clear();
        self.pending_results.clear();
        self.pending_inbound.clear();
        // scan_cur_and_dispatch does not run on supervisor restart (only at initial
        // WatchInbox setup), so clearing here is safe: no cur/ files are re-dispatched.
        self.seen_message_ids = SeenIds::new();
        self.set_idle(ctx);
    }
}

// ── Handler<ProcessInbound> ───────────────────────────────────────────────────

impl Handler<ProcessInbound> for Agent {
    type Result = ();

    fn handle(&mut self, msg: ProcessInbound, ctx: &mut Context<Self>) {
        // Dedup by message_id. scan_cur_and_dispatch re-dispatches all
        // files in cur/ on restart (at-least-once semantics); the same
        // envelope can arrive multiple times.
        if !self.seen_message_ids.insert(msg.message_id.clone()) {
            debug!(message_id = %msg.message_id, "dropping duplicate inbound");
            return;
        }
        if self.in_flight {
            // Queue the message; it will be processed at the end of the
            // current turn. The inbound journal entry is deferred until we
            // actually consume it so the journal stays in turn-causal order.
            debug!(
                message_id = %msg.message_id,
                queue_depth = self.pending_inbound.len() + 1,
                "queueing inbound until current turn ends"
            );
            self.pending_inbound.push_back(msg);
            return;
        }
        self.start_turn_with(msg, ctx);
    }
}

impl Agent {
    /// Begin a turn from a single fresh inbound message. Sets `in_flight`,
    /// flips status to working, journals the inbound, pushes the user turn
    /// to history, and fires the adapter call.
    fn start_turn_with(&mut self, msg: ProcessInbound, ctx: &mut Context<Self>) {
        let ProcessInbound {
            payload,
            message_id,
            sender_id,
        } = msg;
        info!(message_id = %message_id, "processing");
        self.in_flight = true;
        if self.status_writer.write("working").is_err() {
            ctx.stop();
            return;
        }
        if !self.append_inbound(&message_id, sender_id, &payload, ctx) {
            return;
        }
        let attributed = format_inbound_payload(Some(sender_id), &payload);
        self.history.push(reeve_adapter::Message {
            role: reeve_adapter::Role::User,
            content: vec![reeve_adapter::MessageContent::Text(attributed)],
        });
        self.spawn_adapter_call(ctx);
    }

    /// Begin a turn from the queued inbound messages accumulated during the
    /// previous turn. Each queued message contributes its own inbound journal
    /// entry; their payloads are concatenated (separated by a blank line)
    /// into a single user-turn content block so the model sees one alternating
    /// turn rather than several consecutive user turns.
    fn drain_pending_into_turn(&mut self, ctx: &mut Context<Self>) {
        debug_assert!(
            self.in_flight,
            "drain_pending_into_turn called while not in flight"
        );
        debug_assert!(
            !self.pending_inbound.is_empty(),
            "drain_pending_into_turn called with empty queue"
        );
        let mut combined = String::new();
        while let Some(msg) = self.pending_inbound.pop_front() {
            if !self.append_inbound(&msg.message_id, msg.sender_id, &msg.payload, ctx) {
                return;
            }
            if msg.payload.is_empty() {
                // Empty payloads are journaled (operator visibility) but
                // omitted from the user turn — Anthropic rejects empty
                // text content with HTTP 400.
                continue;
            }
            if !combined.is_empty() {
                combined.push_str("\n\n");
            }
            combined.push_str(&format_inbound_payload(Some(msg.sender_id), &msg.payload));
        }
        if combined.is_empty() {
            // All queued payloads were empty. No user turn to send; just go
            // back to idle.
            self.in_flight = false;
            self.tool_iteration = 0;
            self.set_idle(ctx);
            return;
        }
        self.history.push(reeve_adapter::Message {
            role: reeve_adapter::Role::User,
            content: vec![reeve_adapter::MessageContent::Text(combined)],
        });
        self.spawn_adapter_call(ctx);
    }
}

// ── Handler<QuarantineEvent> ──────────────────────────────────────────────────

impl Handler<QuarantineEvent> for Agent {
    type Result = ();

    fn handle(&mut self, msg: QuarantineEvent, ctx: &mut Context<Self>) {
        warn!(reason = %msg.reason, "envelope quarantined");
        self.append_system_entry(&format!("quarantined: {}", msg.reason), ctx);
    }
}

// ── Handler<ToolResult> ───────────────────────────────────────────────────────

impl Handler<ToolResult> for Agent {
    type Result = ();

    fn handle(&mut self, msg: ToolResult, ctx: &mut Context<Self>) {
        if !self.pending_tool_use_ids.remove(&msg.tool_use_id) {
            // Late-arriving result (timeout already fired) or stray result
            // for an unknown tool_use_id. Drop it silently.
            debug!(tool_use_id = %msg.tool_use_id, "ignoring stale tool result");
            return;
        }
        debug!(
            tool_use_id = %msg.tool_use_id,
            is_error = msg.is_error,
            "tool result received"
        );
        self.append_tool_result(&msg.tool_use_id, &msg.content, msg.is_error, ctx);
        if self.pending_tool_use_ids.is_empty() {
            self.advance_after_tools(ctx);
        }
    }
}

// ── Handler<ToolBatchTimeout> ─────────────────────────────────────────────────

impl Handler<ToolBatchTimeout> for Agent {
    type Result = ();

    fn handle(&mut self, _msg: ToolBatchTimeout, ctx: &mut Context<Self>) {
        if self.pending_tool_use_ids.is_empty() {
            // Batch already completed; this is a stale timeout.
            return;
        }
        let timed_out: Vec<String> = self.pending_tool_use_ids.drain().collect();
        warn!(
            count = timed_out.len(),
            "tool batch timed out; synthesizing error results"
        );
        for id in timed_out {
            self.append_tool_result(&id, "tool call timed out", true, ctx);
        }
        self.advance_after_tools(ctx);
    }
}

// ── Pure helpers ──────────────────────────────────────────────────────────────

/// Extract the first text block from a response content slice.
///
/// Returns the text from the first `Text` block. Non-text content blocks are
/// not forwarded to the conversation thread.
fn extract_response_text(content: &[reeve_adapter::MessageContent]) -> String {
    for block in content {
        if let reeve_adapter::MessageContent::Text(text) = block {
            return text.clone();
        }
    }
    String::new()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use actix::Supervisor;
    use tempfile::tempdir;

    use super::{Agent, ProcessInbound};
    use crate::agent_fs::{AgentDirs, ConversationEntry};
    use crate::model_resolution::SpawnSnapshot;
    use reeve_types::IdentityId;

    // ── Mock adapters ─────────────────────────────────────────────────────────

    /// Adapter stub that always succeeds with a fixed text response.
    ///
    /// Distinct from [`crate::test_support::MockAdapter`], which always returns
    /// `Err(BadRequest)`. Use this type for tests that require the agent to
    /// complete a turn (process inbound, write journal, reach idle).
    struct TextResponseAdapter {
        id: &'static str,
    }

    impl TextResponseAdapter {
        fn new(id: &'static str) -> Self {
            Self { id }
        }
    }

    #[async_trait::async_trait]
    impl reeve_adapter::Adapter for TextResponseAdapter {
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
            Ok(reeve_adapter::Response::new_text(
                vec![reeve_adapter::MessageContent::Text(String::from(
                    "mock response",
                ))],
                reeve_adapter::TokenCounts {
                    input: 10,
                    output: 20,
                    cached: 0,
                },
                reeve_adapter::CostEstimate { microdollars: 42 },
            ))
        }
    }

    /// Adapter that always returns a `BadRequest` error; used to test the
    /// adapter error path.
    struct AlwaysErrorAdapter;

    #[async_trait::async_trait]
    impl reeve_adapter::Adapter for AlwaysErrorAdapter {
        fn id(&self) -> &'static str {
            "always-error@test"
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
                message: String::from("context window exceeded"),
            })
        }
    }

    fn mock_snapshot() -> SpawnSnapshot {
        SpawnSnapshot {
            persona_name: String::from("lead"),
            persona_version: 1,

            adapter_id: String::from("mock@test"),
            agent_id: String::new(),
            system_prompt: String::new(),
        }
    }

    fn mock_agent(
        adapter: Arc<dyn reeve_adapter::Adapter>,
        dirs: &AgentDirs,
        tools: Vec<(
            reeve_adapter::Tool,
            actix::Recipient<crate::tool::InvokeTool>,
        )>,
    ) -> Result<Agent, super::AgentError> {
        Agent::new(
            adapter,
            dirs,
            mock_snapshot(),
            String::new(),
            IdentityId::new().unwrap(),
            reeve_types::Keypair::generate(),
            tools,
            crate::capability::Thresholds::default(),
            None,
            dirs.root().to_path_buf(),
        )
    }

    // L1: Agent::new succeeds with a valid adapter and provisioned dirs.
    #[test]
    fn lead_agent_new_creates_valid_actor() {
        let tmp = tempdir().unwrap();
        let dirs = AgentDirs::provision(tmp.path(), "lead").unwrap();
        let adapter = Arc::new(TextResponseAdapter::new("mock@test"));
        let result = mock_agent(adapter, &dirs, Vec::new());
        assert!(result.is_ok(), "Agent::new should succeed");
    }

    // L2: After the actor starts, the status file contains "idle".
    #[test]
    fn lead_agent_started_writes_idle_status() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let status_path = dirs.status_path();
        let adapter = Arc::new(TextResponseAdapter::new("mock@test"));
        let agent = mock_agent(adapter, &dirs, Vec::new()).unwrap();

        actix::System::new().block_on(async move {
            let _addr = Supervisor::start(move |_| agent);

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "status file did not appear within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            actix::System::current().stop();
        });

        let content =
            std::fs::read_to_string(data_dir.join("agents").join("lead").join("status")).unwrap();
        assert_eq!(content, "idle", "status file should contain 'idle'");
    }

    // L3: After a ProcessInbound message is handled, the journal has Inbound,
    // Outbound, and ModelCall entries.
    #[test]
    fn lead_agent_processes_inbound_updates_conversation() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();
        let adapter = Arc::new(TextResponseAdapter::new("mock@test"));
        let agent = mock_agent(adapter, &dirs, Vec::new()).unwrap();

        actix::System::new().block_on(async move {
            let addr = Supervisor::start(move |_| agent);

            // Wait for the actor to start (status file appears).
            let status_path = data_dir.join("agents").join("lead").join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "actor did not start within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            addr.do_send(ProcessInbound {
                payload: String::from("hello"),
                message_id: String::from("test-1"),
                sender_id: IdentityId::new().unwrap(),
            });

            // Poll until the conversation journal has at least 4 lines:
            // system (started) + inbound + outbound + model_call.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                if content.lines().count() >= 4 {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "conversation journal did not reach 4 entries within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            actix::System::current().stop();
        });

        // Parse and validate journal entries.
        let content = std::fs::read_to_string(&conv_path_outer).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        let has_inbound = entries.iter().any(|e| e["type"] == "inbound");
        let has_outbound = entries.iter().any(|e| e["type"] == "outbound");
        let has_model_call = entries.iter().any(|e| e["type"] == "model_call");

        assert!(
            has_inbound,
            "journal missing inbound entry; entries: {entries:?}"
        );
        assert!(
            has_outbound,
            "journal missing outbound entry; entries: {entries:?}"
        );
        assert!(
            has_model_call,
            "journal missing model_call entry; entries: {entries:?}"
        );

        let inbound = entries.iter().find(|e| e["type"] == "inbound").unwrap();
        assert_eq!(inbound["message_id"], "test-1");
        assert_eq!(inbound["payload"], "hello");
    }

    // L-Dedup: A duplicate ProcessInbound (same message_id) is silently
    // dropped — only the first delivery produces an inbound journal entry
    // and triggers an adapter call. This guards against the cur/ watcher
    // re-sending envelopes on every filesystem event.
    #[test]
    fn agent_drops_duplicate_inbound_message_id() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();
        let adapter = Arc::new(TextResponseAdapter::new("mock@test"));
        let agent = mock_agent(adapter, &dirs, Vec::new()).unwrap();

        actix::System::new().block_on(async move {
            let addr = Supervisor::start(move |_| agent);

            let status_path = data_dir.join("agents").join("lead").join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(std::time::Instant::now() <= deadline, "actor start timeout");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // Send the same message_id three times back to back. Only the
            // first should produce a turn.
            for _ in 0..3 {
                addr.do_send(ProcessInbound {
                    payload: String::from("hello"),
                    message_id: String::from("dup-1"),
                    sender_id: IdentityId::new().unwrap(),
                });
            }

            // Wait until the turn completes (one outbound entry).
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                if content.lines().any(|l| l.contains("\"outbound\"")) {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "turn did not complete; content:\n{content}",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // Give the actor a beat to process any stragglers (shouldn't be any).
            tokio::time::sleep(Duration::from_millis(100)).await;

            actix::System::current().stop();
        });

        let content = std::fs::read_to_string(&conv_path_outer).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        let inbounds = entries.iter().filter(|e| e["type"] == "inbound").count();
        assert_eq!(
            inbounds, 1,
            "duplicate message_ids should only journal once; entries: {entries:?}"
        );
        let model_calls = entries.iter().filter(|e| e["type"] == "model_call").count();
        assert_eq!(
            model_calls, 1,
            "duplicate message_ids should fire one adapter call; entries: {entries:?}"
        );
    }

    // L4: extract_response_text returns the first text block from a content slice.
    #[test]
    fn extract_response_text_returns_first_text() {
        let content = vec![
            reeve_adapter::MessageContent::Text(String::from("first")),
            reeve_adapter::MessageContent::Text(String::from("second")),
        ];
        let text = super::extract_response_text(&content);
        assert_eq!(text, "first");
    }

    // L5: extract_response_text returns an empty string for an empty slice.
    #[test]
    fn extract_response_text_empty_content_returns_empty_string() {
        let text = super::extract_response_text(&[]);
        assert!(
            text.is_empty(),
            "expected empty string for empty content slice"
        );
    }

    // L6: AgentError Display impls are non-empty and informative.
    #[test]
    fn lead_agent_error_display_impls() {
        use std::io;
        use std::path::PathBuf;

        use crate::agent_fs::AgentFsError;

        use super::AgentError;

        let fs_err = AgentError::Fs(AgentFsError::Io {
            path: PathBuf::from("agents/lead/status"),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        });
        let rendered = fs_err.to_string();
        assert!(!rendered.is_empty(), "Fs variant display empty");
        assert!(
            rendered.contains("agent fs"),
            "Fs variant missing context: {rendered}"
        );

        let io_err = AgentError::Io {
            path: PathBuf::from("agents/lead/cost"),
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        let rendered = io_err.to_string();
        assert!(!rendered.is_empty(), "Io variant display empty");
        assert!(
            rendered.contains("agents/lead/cost"),
            "Io variant missing path: {rendered}"
        );

        let serde_err = serde_json::from_str::<serde_json::Value>("bad").unwrap_err();
        let json_err = AgentError::Json(serde_err);
        let rendered = json_err.to_string();
        assert!(!rendered.is_empty(), "Json variant display empty");

        let adapter_err = AgentError::Adapter(reeve_adapter::AdapterError::BadRequest {
            message: String::from("test"),
        });
        let rendered = adapter_err.to_string();
        assert!(!rendered.is_empty(), "Adapter variant display empty");
        assert!(
            rendered.contains("adapter"),
            "Adapter variant missing context: {rendered}"
        );
    }

    // L7: ConversationEntry variants serialize with expected type tags.
    #[test]
    fn conversation_entries_round_trip() {
        use time::OffsetDateTime;

        let now = OffsetDateTime::now_utc();
        let inbound = ConversationEntry::Inbound {
            message_id: String::from("m1"),
            sender_id: None,
            payload: String::from("hello"),
            timestamp_utc: now,
        };
        let json = serde_json::to_string(&inbound).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["type"], "inbound");
        assert_eq!(value["message_id"], "m1");
        assert_eq!(value["payload"], "hello");
    }

    // ── Slow mock adapter (used by L-B and similar) ───────────────────────────

    struct SlowMockAdapter;

    #[async_trait::async_trait]
    impl reeve_adapter::Adapter for SlowMockAdapter {
        fn id(&self) -> &'static str {
            "slow-model@test"
        }

        fn capabilities(&self) -> reeve_adapter::Capabilities {
            reeve_adapter::Capabilities::new()
        }

        async fn call(
            &self,
            _msgs: &[reeve_adapter::Message],
            _tools: &[reeve_adapter::Tool],
            _params: &reeve_adapter::Params,
        ) -> Result<reeve_adapter::Response, reeve_adapter::AdapterError> {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(reeve_adapter::Response::new_text(
                vec![reeve_adapter::MessageContent::Text(
                    "slow response".to_owned(),
                )],
                reeve_adapter::TokenCounts {
                    input: 5,
                    output: 5,
                    cached: 0,
                },
                reeve_adapter::CostEstimate { microdollars: 10 },
            ))
        }
    }

    // ── Two-phase adapter (used by L9) ────────────────────────────────────────

    struct TwoPhaseAdapter {
        calls: Arc<std::sync::Mutex<Vec<Vec<reeve_adapter::Message>>>>,
    }

    #[async_trait::async_trait]
    impl reeve_adapter::Adapter for TwoPhaseAdapter {
        fn id(&self) -> &'static str {
            "two-phase@test"
        }

        fn capabilities(&self) -> reeve_adapter::Capabilities {
            reeve_adapter::Capabilities::new()
        }

        async fn call(
            &self,
            msgs: &[reeve_adapter::Message],
            _: &[reeve_adapter::Tool],
            _: &reeve_adapter::Params,
        ) -> Result<reeve_adapter::Response, reeve_adapter::AdapterError> {
            let mut calls = self
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            calls.push(msgs.to_vec());
            if calls.len() == 1 {
                Err(reeve_adapter::AdapterError::BadRequest {
                    message: "context too large".to_owned(),
                })
            } else {
                Ok(reeve_adapter::Response::new_text(
                    vec![reeve_adapter::MessageContent::Text("ok".to_owned())],
                    reeve_adapter::TokenCounts {
                        input: 10,
                        output: 5,
                        cached: 0,
                    },
                    reeve_adapter::CostEstimate { microdollars: 20 },
                ))
            }
        }
    }

    // L-B: A second ProcessInbound arriving while a turn is in flight is
    // queued, not dropped. Both messages produce inbound journal entries in
    // arrival order, and the journal contains no "discarded" system entries.
    // Two adapter calls fire (one per turn) — verified via two model_call
    // entries.
    #[test]
    fn lead_agent_second_message_queues_during_in_flight() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();
        let adapter = Arc::new(SlowMockAdapter);
        let agent = mock_agent(adapter, &dirs, Vec::new()).unwrap();

        actix::System::new().block_on(async move {
            let addr = Supervisor::start(move |_| agent);

            // Wait for started (status file appears).
            let status_path = data_dir.join("agents").join("lead").join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "actor did not start within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // Send first message, then immediately send a second before the
            // slow adapter (200 ms) finishes.
            addr.do_send(ProcessInbound {
                payload: String::from("first"),
                message_id: String::from("msg-1"),
                sender_id: IdentityId::new().unwrap(),
            });
            addr.do_send(ProcessInbound {
                payload: String::from("second"),
                message_id: String::from("msg-2"),
                sender_id: IdentityId::new().unwrap(),
            });

            // Wait for the journal to contain two model_call entries (two
            // adapter calls fired) — that is the signal both turns ran.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                let model_calls = content
                    .lines()
                    .filter(|line| line.contains("\"model_call\""))
                    .count();
                if model_calls >= 2 {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "two adapter calls did not complete within 5 seconds; content:\n{content}",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            actix::System::current().stop();
        });

        let content = std::fs::read_to_string(&conv_path_outer).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        // No "discarded" system entries.
        let discarded = entries.iter().find(|e| {
            e["type"] == "system"
                && e["message"]
                    .as_str()
                    .map(|m| m.contains("discarded"))
                    .unwrap_or(false)
        });
        assert!(
            discarded.is_none(),
            "no 'discarded' system entries should appear; entries: {entries:?}"
        );

        // Two inbound entries, in arrival order.
        let inbound_entries: Vec<_> = entries.iter().filter(|e| e["type"] == "inbound").collect();
        assert_eq!(
            inbound_entries.len(),
            2,
            "expected two inbound entries; entries: {entries:?}"
        );
        assert_eq!(inbound_entries[0]["payload"], "first");
        assert_eq!(inbound_entries[0]["message_id"], "msg-1");
        assert_eq!(inbound_entries[1]["payload"], "second");
        assert_eq!(inbound_entries[1]["message_id"], "msg-2");

        // Two model_call entries (one per turn).
        let model_calls = entries.iter().filter(|e| e["type"] == "model_call").count();
        assert_eq!(model_calls, 2);
    }

    // L9: After an adapter error, the history is cleaned (user message popped),
    // so the next ProcessInbound sends only that new message to the adapter —
    // not the failed one.
    #[test]
    fn lead_agent_retry_after_error_sends_clean_history() {
        use std::sync::{Arc as StdArc, Mutex};

        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let calls_log: StdArc<Mutex<Vec<Vec<reeve_adapter::Message>>>> =
            StdArc::new(Mutex::new(vec![]));
        let calls_log_for_actor = StdArc::clone(&calls_log);
        let calls_log_for_assert = StdArc::clone(&calls_log);
        let adapter = Arc::new(TwoPhaseAdapter {
            calls: calls_log_for_actor,
        });
        let agent = mock_agent(adapter, &dirs, Vec::new()).unwrap();

        actix::System::new().block_on(async move {
            let addr = Supervisor::start(move |_| agent);

            // Wait for started.
            let status_path = data_dir.join("agents").join("lead").join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "actor did not start within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // First message — adapter returns error on first call.
            addr.do_send(ProcessInbound {
                payload: String::from("first message"),
                message_id: String::from("msg-1"),
                sender_id: IdentityId::new().unwrap(),
            });

            // Wait for the error to be processed and actor to return to idle.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                // system(started) + inbound + system(adapter call failed) = 3
                if content.lines().count() >= 3 {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "first call did not complete within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // Second message — adapter succeeds on second call.
            addr.do_send(ProcessInbound {
                payload: String::from("second message"),
                message_id: String::from("msg-2"),
                sender_id: IdentityId::new().unwrap(),
            });

            // Wait for the second call to complete.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                // Previous 3 + inbound + outbound + model_call = 6
                if content.lines().count() >= 6 {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "second call did not complete within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            actix::System::current().stop();
        });

        let calls = calls_log_for_assert.lock().unwrap();
        assert_eq!(calls.len(), 2, "expected exactly 2 adapter calls");
        // Second call must carry only the second user message —
        // the first user message was popped from history after the error.
        assert_eq!(
            calls[1].len(),
            1,
            "second adapter call should carry exactly one message; got: {:?}",
            calls[1]
        );
        // The runtime now prefixes inbound payloads with the sender id
        // (`[from <uuid>]\n…`) so the model can distinguish operator messages
        // from peer-agent replies. Verify the trailing payload matches.
        let reeve_adapter::MessageContent::Text(ref text) = calls[1][0].content[0] else {
            panic!("expected text content; got: {:?}", calls[1][0].content)
        };
        assert!(
            text.ends_with("second message"),
            "second adapter call should carry the second user message; got: {text}"
        );
    }

    // L8: When the adapter returns an error, the actor returns to idle,
    // the user message is removed from history, and the journal has a
    // "adapter call failed: ..." system entry.
    #[test]
    fn lead_agent_adapter_error_returns_to_idle() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();
        let adapter = Arc::new(AlwaysErrorAdapter);
        let agent = mock_agent(adapter, &dirs, Vec::new()).unwrap();
        let status_path_outer = data_dir.join("agents").join("lead").join("status");

        actix::System::new().block_on(async move {
            let addr = Supervisor::start(move |_| agent);

            // Wait for the actor to start (status file appears).
            let status_path = data_dir.join("agents").join("lead").join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "actor did not start within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            addr.do_send(ProcessInbound {
                payload: String::from("hi"),
                message_id: String::from("err-1"),
                sender_id: IdentityId::new().unwrap(),
            });

            // Poll until the conversation journal has at least 3 entries:
            // system (started) + inbound + system (adapter call failed).
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                if content.lines().count() >= 3 {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "conversation journal did not reach 3 entries within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // Wait for status to return to "idle".
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let status = std::fs::read_to_string(&status_path).unwrap_or_default();
                if status == "idle" {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "status did not return to 'idle' within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            actix::System::current().stop();
        });

        // Status must be "idle" after the error.
        let status_content = std::fs::read_to_string(&status_path_outer).unwrap();
        assert_eq!(
            status_content, "idle",
            "status should be 'idle' after adapter error"
        );

        // Journal must contain a system entry with "adapter call failed".
        let content = std::fs::read_to_string(&conv_path_outer).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        let error_entry = entries.iter().find(|e| {
            e["type"] == "system"
                && e["message"]
                    .as_str()
                    .map(|m| m.contains("adapter call failed"))
                    .unwrap_or(false)
        });
        assert!(
            error_entry.is_some(),
            "journal missing 'adapter call failed' system entry; entries: {entries:?}"
        );
        let msg = error_entry.unwrap()["message"].as_str().unwrap();
        assert!(
            msg.contains("context window exceeded"),
            "error message should include adapter error detail: {msg}"
        );
    }

    // L-Q: A QuarantineEvent surfaces as a system entry in the conversation
    // journal so transport rejections are visible without reading daemon logs.
    #[test]
    fn quarantine_event_appends_system_entry() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();
        let adapter = Arc::new(TextResponseAdapter::new("mock@test"));
        let agent = mock_agent(adapter, &dirs, Vec::new()).unwrap();

        actix::System::new().block_on(async move {
            let addr = Supervisor::start(move |_| agent);

            // Wait for the actor to start.
            let status_path = data_dir.join("agents").join("lead").join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "actor did not start within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            addr.do_send(super::QuarantineEvent {
                reason: String::from("signature_invalid"),
            });

            // Poll until journal has system(started) + system(quarantined) = 2.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                if content.lines().count() >= 2 {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "quarantine system entry did not appear within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            actix::System::current().stop();
        });

        let content = std::fs::read_to_string(&conv_path_outer).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        let quarantine_entry = entries.iter().find(|e| {
            e["type"] == "system"
                && e["message"]
                    .as_str()
                    .map(|m| m.contains("quarantined") && m.contains("signature_invalid"))
                    .unwrap_or(false)
        });
        assert!(
            quarantine_entry.is_some(),
            "journal missing quarantine system entry; entries: {entries:?}"
        );
    }

    // ── Tool loop adapter: ToolUse on call 1, EndTurn on call 2 ───────────────
    //
    // Parameterized so IT-1 and IT-2 can reuse the same skeleton without
    // repeating boilerplate. `preamble_text` is emitted before the tool call
    // on turn 1; `None` produces a tool-only turn (no text blocks).

    struct TwoTurnSpawnAdapter {
        calls: Arc<std::sync::Mutex<u32>>,
        persona: &'static str,
        task: &'static str,
        tool_call_id: &'static str,
        preamble_text: Option<&'static str>,
        end_turn_text: &'static str,
    }

    #[async_trait::async_trait]
    impl reeve_adapter::Adapter for TwoTurnSpawnAdapter {
        fn id(&self) -> &'static str {
            "two-turn-spawn@test"
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
            let mut count = self.calls.lock().unwrap();
            *count += 1;
            let n = *count;
            drop(count);
            if n == 1 {
                let text_blocks = match self.preamble_text {
                    Some(t) => vec![reeve_adapter::MessageContent::Text(t.to_owned())],
                    None => vec![],
                };
                Ok(reeve_adapter::Response::new_tool_use(
                    text_blocks,
                    vec![reeve_adapter::ToolCall {
                        id: self.tool_call_id.to_owned(),
                        name: "spawn_agent".to_owned(),
                        arguments: serde_json::json!({
                            "persona": self.persona,
                            "task": self.task
                        }),
                    }],
                    reeve_adapter::TokenCounts {
                        input: 10,
                        output: 5,
                        cached: 0,
                    },
                    reeve_adapter::CostEstimate { microdollars: 5 },
                ))
            } else {
                Ok(reeve_adapter::Response::new_text(
                    vec![reeve_adapter::MessageContent::Text(
                        self.end_turn_text.to_owned(),
                    )],
                    reeve_adapter::TokenCounts {
                        input: 12,
                        output: 3,
                        cached: 0,
                    },
                    reeve_adapter::CostEstimate { microdollars: 3 },
                ))
            }
        }
    }

    // L-T1: tool execution loop end-to-end. Adapter returns ToolUse on call 1,
    // SpawnAgentTool fires (via MockSpawnCoordinator), ToolResult flows back,
    // adapter returns EndTurn on call 2, the agent goes idle. Journal must
    // contain tool_use and tool_result entries with matching tool_use_id, and
    // the final response text.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "end-to-end loop test with multi-stage poll loops and journal \
                  assertions; splitting fragments the narrative"
    )]
    fn agent_tool_loop_round_trips_through_spawn_agent_tool() {
        use crate::test_support::MockSpawnCoordinator;
        use crate::tool::SpawnAgentTool;
        use actix::Actor as _;

        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();
        let calls_log = Arc::new(std::sync::Mutex::new(0u32));
        let calls_log_assert = Arc::clone(&calls_log);
        let adapter = Arc::new(TwoTurnSpawnAdapter {
            calls: calls_log,
            persona: "test-persona",
            task: "hello world",
            tool_call_id: "tu_1",
            preamble_text: Some("spawning agent"),
            end_turn_text: "done!",
        });

        actix::System::new().block_on(async move {
            // Start the SpawnAgentTool actor backed by a mock coordinator.
            let mock_coord = MockSpawnCoordinator.start();
            let tool_addr = SpawnAgentTool::new(mock_coord.recipient(), None, None).start();
            let tools: Vec<(
                reeve_adapter::Tool,
                actix::Recipient<crate::tool::InvokeTool>,
            )> = vec![(SpawnAgentTool::descriptor(), tool_addr.recipient())];

            let agent = mock_agent(adapter, &dirs, tools).unwrap();
            let addr = Supervisor::start(move |_| agent);

            // Wait for the actor to start.
            let status_path = data_dir.join("agents").join("lead").join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "actor did not start within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            addr.do_send(ProcessInbound {
                payload: String::from("trigger"),
                message_id: String::from("m-1"),
                sender_id: IdentityId::new().unwrap(),
            });

            // Expect 8 entries:
            //   system(started), inbound, outbound(call 1 text), model_call,
            //   tool_use, tool_result, outbound(call 2 text), model_call.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                if content.lines().count() >= 8 {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "tool loop did not complete within 5 seconds; got:\n{content}",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // Verify the agent returned to idle.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                let status = std::fs::read_to_string(&status_path).unwrap_or_default();
                if status == "idle" {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "status did not return to idle; got: {status}",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            actix::System::current().stop();
        });

        // Two adapter calls: call 1 ToolUse, call 2 EndTurn.
        assert_eq!(*calls_log_assert.lock().unwrap(), 2);

        let content = std::fs::read_to_string(&conv_path_outer).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        // Tool-use entry with the expected name and input.
        let tool_use = entries
            .iter()
            .find(|e| e["type"] == "tool_use")
            .expect("tool_use entry missing");
        assert_eq!(tool_use["tool_use_id"], "tu_1");
        assert_eq!(tool_use["name"], "spawn_agent");
        assert_eq!(
            tool_use["input"],
            serde_json::json!({ "persona": "test-persona", "task": "hello world" })
        );

        // Tool-result entry with matching id and the mock agent name content.
        let tool_result = entries
            .iter()
            .find(|e| e["type"] == "tool_result")
            .expect("tool_result entry missing");
        assert_eq!(tool_result["tool_use_id"], "tu_1");
        assert_eq!(tool_result["content"], "mock-agent");
        assert_eq!(tool_result["is_error"], false);

        // Final outbound entry carries the EndTurn text.
        let outbounds: Vec<&serde_json::Value> =
            entries.iter().filter(|e| e["type"] == "outbound").collect();
        assert!(
            outbounds.iter().any(|o| o["payload"] == "done!"),
            "final 'done!' outbound not found; outbounds: {outbounds:?}"
        );
    }

    // ── Adapter that always returns ToolUse — runaway guard test ──────────────

    struct InfiniteToolAdapter {
        counter: Arc<std::sync::atomic::AtomicU64>,
    }

    #[async_trait::async_trait]
    impl reeve_adapter::Adapter for InfiniteToolAdapter {
        fn id(&self) -> &'static str {
            "infinite-tool@test"
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
            let n = self
                .counter
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(reeve_adapter::Response::new_tool_use(
                vec![],
                vec![reeve_adapter::ToolCall {
                    id: format!("tu_{n}"),
                    name: "spawn_agent".to_owned(),
                    arguments: serde_json::json!({ "persona": "test-persona", "task": "spin" }),
                }],
                reeve_adapter::TokenCounts {
                    input: 1,
                    output: 1,
                    cached: 0,
                },
                reeve_adapter::CostEstimate { microdollars: 1 },
            ))
        }
    }

    // L-T2: A model that never stops calling tools eventually trips
    // MAX_TOOL_ITERATIONS. The agent appends a system entry and goes idle
    // rather than spinning forever.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "runaway-guard test with multi-stage poll loops and journal \
                  assertions; splitting fragments the narrative"
    )]
    fn agent_tool_loop_aborts_at_max_iterations() {
        use crate::test_support::MockSpawnCoordinator;
        use crate::tool::SpawnAgentTool;
        use actix::Actor as _;

        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();
        let adapter = Arc::new(InfiniteToolAdapter {
            counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        });

        actix::System::new().block_on(async move {
            let mock_coord = MockSpawnCoordinator.start();
            let tool_addr = SpawnAgentTool::new(mock_coord.recipient(), None, None).start();
            let tools: Vec<(
                reeve_adapter::Tool,
                actix::Recipient<crate::tool::InvokeTool>,
            )> = vec![(SpawnAgentTool::descriptor(), tool_addr.recipient())];

            let agent = mock_agent(adapter, &dirs, tools).unwrap();
            let addr = Supervisor::start(move |_| agent);

            // Wait for the actor to start.
            let status_path = data_dir.join("agents").join("lead").join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "actor did not start within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            addr.do_send(ProcessInbound {
                payload: String::from("trigger"),
                message_id: String::from("m-1"),
                sender_id: IdentityId::new().unwrap(),
            });

            // Wait for the abort system entry to appear.
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                if content.contains("tool loop aborted") {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "abort system entry did not appear within 15 seconds; got:\n{content}",
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            // Confirm idle.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                let status = std::fs::read_to_string(&status_path).unwrap_or_default();
                if status == "idle" {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "status did not return to idle after abort; got: {status}",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            actix::System::current().stop();
        });

        let content = std::fs::read_to_string(&conv_path_outer).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        let abort = entries.iter().find(|e| {
            e["type"] == "system"
                && e["message"]
                    .as_str()
                    .map(|m| m.contains("tool loop aborted"))
                    .unwrap_or(false)
        });
        assert!(
            abort.is_some(),
            "abort system entry missing; entries: {entries:?}"
        );

        // Number of tool_use entries must not exceed MAX_TOOL_ITERATIONS.
        let tool_use_count = entries.iter().filter(|e| e["type"] == "tool_use").count();
        let max_iterations = usize::try_from(super::MAX_TOOL_ITERATIONS).expect("u32 fits usize");
        assert!(
            tool_use_count <= max_iterations,
            "too many tool_use entries: {tool_use_count}",
        );
    }

    // ── Adapter that returns ToolUse with NO text on call 1 ───────────────────

    struct EmptyTextThenEndAdapter {
        calls: Arc<std::sync::Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl reeve_adapter::Adapter for EmptyTextThenEndAdapter {
        fn id(&self) -> &'static str {
            "empty-text-then-end@test"
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
            let mut count = self.calls.lock().unwrap();
            *count += 1;
            let n = *count;
            drop(count);
            if n == 1 {
                // Tool-use turn with no preamble text (only tool_use blocks).
                Ok(reeve_adapter::Response::new_tool_use(
                    vec![],
                    vec![reeve_adapter::ToolCall {
                        id: "tu_1".to_owned(),
                        name: "spawn_agent".to_owned(),
                        arguments: serde_json::json!({ "persona": "test-persona", "task": "x" }),
                    }],
                    reeve_adapter::TokenCounts {
                        input: 1,
                        output: 1,
                        cached: 0,
                    },
                    reeve_adapter::CostEstimate { microdollars: 1 },
                ))
            } else {
                Ok(reeve_adapter::Response::new_text(
                    vec![reeve_adapter::MessageContent::Text("done".to_owned())],
                    reeve_adapter::TokenCounts {
                        input: 1,
                        output: 1,
                        cached: 0,
                    },
                    reeve_adapter::CostEstimate { microdollars: 1 },
                ))
            }
        }
    }

    // L-T3: When the model returns a tool-use turn with no accompanying text,
    // the agent does NOT write an empty outbound entry. The model_call entry
    // is still recorded because it carries token/cost telemetry independent
    // of whether the turn had text.
    #[test]
    fn agent_skips_empty_outbound_for_tool_only_turn() {
        use crate::test_support::MockSpawnCoordinator;
        use crate::tool::SpawnAgentTool;
        use actix::Actor as _;

        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();
        let calls = Arc::new(std::sync::Mutex::new(0u32));
        let adapter = Arc::new(EmptyTextThenEndAdapter {
            calls: Arc::clone(&calls),
        });

        actix::System::new().block_on(async move {
            let mock_coord = MockSpawnCoordinator.start();
            let tool_addr = SpawnAgentTool::new(mock_coord.recipient(), None, None).start();
            let tools: Vec<(
                reeve_adapter::Tool,
                actix::Recipient<crate::tool::InvokeTool>,
            )> = vec![(SpawnAgentTool::descriptor(), tool_addr.recipient())];

            let agent = mock_agent(adapter, &dirs, tools).unwrap();
            let addr = Supervisor::start(move |_| agent);

            let status_path = data_dir.join("agents").join("lead").join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(std::time::Instant::now() <= deadline, "actor start timeout");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            addr.do_send(ProcessInbound {
                payload: "go".to_owned(),
                message_id: "m-1".to_owned(),
                sender_id: IdentityId::new().unwrap(),
            });

            // Wait until the second outbound (with "done") appears.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                if content.contains("\"done\"") {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "loop did not finish; content:\n{content}",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            actix::System::current().stop();
        });

        let content = std::fs::read_to_string(&conv_path_outer).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        // Exactly one outbound entry, and it carries the final-turn text.
        let outbounds: Vec<&serde_json::Value> =
            entries.iter().filter(|e| e["type"] == "outbound").collect();
        assert_eq!(
            outbounds.len(),
            1,
            "expected exactly one outbound; entries: {entries:?}"
        );
        assert_eq!(outbounds[0]["payload"], "done");

        // Both model_calls present (telemetry retained even on tool-only turns).
        let model_calls = entries.iter().filter(|e| e["type"] == "model_call").count();
        assert_eq!(model_calls, 2, "both model_call entries should be present");
    }

    // ── Adapter that calls a tool the agent does not have registered ─────────

    struct UnknownToolThenEndAdapter {
        calls: Arc<std::sync::Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl reeve_adapter::Adapter for UnknownToolThenEndAdapter {
        fn id(&self) -> &'static str {
            "unknown-tool@test"
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
            let mut count = self.calls.lock().unwrap();
            *count += 1;
            let n = *count;
            drop(count);
            if n == 1 {
                Ok(reeve_adapter::Response::new_tool_use(
                    vec![],
                    vec![reeve_adapter::ToolCall {
                        id: "tu_x".to_owned(),
                        name: "nonexistent_tool".to_owned(),
                        arguments: serde_json::json!({}),
                    }],
                    reeve_adapter::TokenCounts {
                        input: 1,
                        output: 1,
                        cached: 0,
                    },
                    reeve_adapter::CostEstimate { microdollars: 1 },
                ))
            } else {
                Ok(reeve_adapter::Response::new_text(
                    vec![reeve_adapter::MessageContent::Text("recovered".to_owned())],
                    reeve_adapter::TokenCounts {
                        input: 1,
                        output: 1,
                        cached: 0,
                    },
                    reeve_adapter::CostEstimate { microdollars: 1 },
                ))
            }
        }
    }

    // L-T4: When the model calls a tool that is not registered, the agent
    // synthesizes a tool_result with is_error: true and continues the loop.
    // The model recovers on the next turn. No tool actor is needed for the
    // unknown tool — the agent handles it locally.
    #[test]
    fn agent_recovers_from_unknown_tool_name() {
        use crate::test_support::MockSpawnCoordinator;
        use crate::tool::SpawnAgentTool;
        use actix::Actor as _;

        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();
        let calls = Arc::new(std::sync::Mutex::new(0u32));
        let adapter = Arc::new(UnknownToolThenEndAdapter {
            calls: Arc::clone(&calls),
        });

        actix::System::new().block_on(async move {
            // Register SpawnAgentTool; the model will call "nonexistent_tool".
            let mock_coord = MockSpawnCoordinator.start();
            let tool_addr = SpawnAgentTool::new(mock_coord.recipient(), None, None).start();
            let tools: Vec<(
                reeve_adapter::Tool,
                actix::Recipient<crate::tool::InvokeTool>,
            )> = vec![(SpawnAgentTool::descriptor(), tool_addr.recipient())];

            let agent = mock_agent(adapter, &dirs, tools).unwrap();
            let addr = Supervisor::start(move |_| agent);

            let status_path = data_dir.join("agents").join("lead").join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(std::time::Instant::now() <= deadline, "actor start timeout");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            addr.do_send(ProcessInbound {
                payload: "go".to_owned(),
                message_id: "m-1".to_owned(),
                sender_id: IdentityId::new().unwrap(),
            });

            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                if content.contains("\"recovered\"") {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "loop did not recover; content:\n{content}",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            actix::System::current().stop();
        });

        let content = std::fs::read_to_string(&conv_path_outer).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        // The synthetic tool_result is in the journal with is_error=true.
        let tool_result = entries
            .iter()
            .find(|e| e["type"] == "tool_result")
            .expect("tool_result missing");
        assert_eq!(tool_result["tool_use_id"], "tu_x");
        assert_eq!(tool_result["is_error"], true);
        assert!(
            tool_result["content"]
                .as_str()
                .unwrap()
                .contains("nonexistent_tool"),
            "tool_result content should name the unknown tool: {tool_result:?}"
        );

        // Adapter was called twice (one to dispatch, one to recover).
        assert_eq!(*calls.lock().unwrap(), 2);
    }

    // L-Dup: Agent::new returns DuplicateToolName when two tool bindings
    // share a name.
    #[test]
    fn agent_new_rejects_duplicate_tool_names() {
        use crate::test_support::MockSpawnCoordinator;
        use crate::tool::SpawnAgentTool;
        use actix::Actor as _;

        let tmp = tempdir().unwrap();
        let dirs = AgentDirs::provision(tmp.path(), "lead").unwrap();
        let adapter = Arc::new(TextResponseAdapter::new("mock@test"));

        actix::System::new().block_on(async move {
            let mock_coord = MockSpawnCoordinator.start();
            let a = SpawnAgentTool::new(mock_coord.clone().recipient(), None, None).start();
            let b = SpawnAgentTool::new(mock_coord.recipient(), None, None).start();
            // Both bindings declare the same name (SpawnAgentTool::descriptor()
            // returns "spawn_agent").
            let tools = vec![
                (SpawnAgentTool::descriptor(), a.recipient()),
                (SpawnAgentTool::descriptor(), b.recipient()),
            ];
            let result = mock_agent(adapter, &dirs, tools);
            match result {
                Err(super::AgentError::DuplicateToolName(name)) => {
                    assert_eq!(name, "spawn_agent");
                }
                Err(other) => panic!("expected DuplicateToolName, got {other:?}"),
                Ok(_) => panic!("expected error, got Ok"),
            }
            actix::System::current().stop();
        });
    }

    // ── SpawnAgentTool integration tests ─────────────────────────────────────

    fn find_spawned_agent_status(data_dir: &std::path::Path) -> Option<std::path::PathBuf> {
        let agents_dir = data_dir.join("agents");
        std::fs::read_dir(&agents_dir)
            .ok()?
            .flatten()
            .find_map(|entry| {
                if entry.file_name() == "lead" {
                    return None;
                }
                let status = entry.path().join("status");
                if status.exists() {
                    Some(status)
                } else {
                    None
                }
            })
    }

    // IT-1: When the lead agent calls spawn_agent, the SpawnCoordinator
    // provisions a new agent that writes "idle" to its status file. The lead
    // agent's conversation journal must contain a tool_result entry with
    // is_error=false and content equal to the spawned agent's name.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "end-to-end integration test: status poll + journal assertion; \
                  splitting would obscure the success-path verification"
    )]
    fn spawn_agent_tool_spawns_agent_with_idle_status() {
        use crate::spawn_coordinator::SpawnCoordinator;
        use crate::test_support::{build_registries, NullDispatcher, NullInboxStarter};
        use crate::tool::SpawnAgentTool;
        use actix::Actor as _;

        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();

        crate::test_support::write_persona_config(&data_dir, "test-persona", "mock");
        crate::test_support::write_full_access_persona_profile(&data_dir, "test-persona");

        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();

        // The lead adapter drives the scenario; the coord adapter is what
        // spawned agents use (must match "test-persona"'s model_preferences).
        let lead_adapter: Arc<dyn reeve_adapter::Adapter> = Arc::new(TwoTurnSpawnAdapter {
            calls: Arc::new(std::sync::Mutex::new(0u32)),
            persona: "test-persona",
            task: "run integration test",
            tool_call_id: "tu_spawn",
            preamble_text: None,
            end_turn_text: "spawned!",
        });
        let coord_adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(crate::test_support::MockAdapter::new("mock@test"));

        let data_dir_for_block = data_dir.clone();
        let agent_registry_path_for_block = agent_registry_path.clone();

        actix::System::new().block_on(async move {
            let null_inbox = NullInboxStarter.start();
            let null_dispatcher = NullDispatcher.start();

            let audit = Arc::new(
                crate::audit::AuditLog::open(data_dir_for_block.clone())
                    .expect("open audit log in test"),
            );
            let spawn_coordinator = SpawnCoordinator::new(
                data_dir_for_block.clone(),
                agent_registry_path_for_block,
                identity_registry,
                vec![Arc::clone(&coord_adapter)],
                audit,
                Arc::clone(&watcher),
                null_inbox.recipient(),
                null_dispatcher.recipient(),
                None,
            );
            let coord_addr = Supervisor::start(move |_| spawn_coordinator);

            let spawn_tool = SpawnAgentTool::new(coord_addr.recipient(), None, None);
            let tools: Vec<(
                reeve_adapter::Tool,
                actix::Recipient<crate::tool::InvokeTool>,
            )> = vec![(SpawnAgentTool::descriptor(), spawn_tool.start().recipient())];

            let agent = mock_agent(lead_adapter, &dirs, tools).unwrap();
            let addr = Supervisor::start(move |_| agent);

            // Wait for lead agent to reach idle.
            let status_path = data_dir_for_block
                .join("agents")
                .join("lead")
                .join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "lead agent did not start within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            addr.do_send(ProcessInbound {
                payload: "go".to_owned(),
                message_id: "m-1".to_owned(),
                sender_id: IdentityId::new().unwrap(),
            });

            // Poll until a spawned agent's status file appears and reads "idle".
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status_path) = find_spawned_agent_status(&data_dir_for_block) {
                    let content = std::fs::read_to_string(&status_path).unwrap_or_default();
                    if content == "idle" {
                        break;
                    }
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "spawned agent did not reach idle within 10 seconds",
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            // Wait until the lead agent's journal records the tool_result before
            // reading it outside block_on.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                let has_success_result = content.lines().any(|line| {
                    if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
                        entry["type"] == "tool_result" && entry["is_error"] == false
                    } else {
                        false
                    }
                });
                if has_success_result {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "success tool_result did not appear in journal within 5 seconds; \
                     journal:\n{content}",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            actix::System::current().stop();
        });

        // Verify outside block_on: at least one non-lead agent status file exists
        // and reads "idle".
        let spawned_status = find_spawned_agent_status(&data_dir)
            .expect("expected at least one spawned agent status file");
        let status_content = std::fs::read_to_string(&spawned_status).unwrap();
        assert_eq!(status_content, "idle", "spawned agent status must be idle");

        // Verify the lead agent's journal recorded the tool_result with
        // is_error=false and content equal to the spawned agent's name.
        let journal_content = std::fs::read_to_string(&conv_path_outer).unwrap();
        let entries: Vec<serde_json::Value> = journal_content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        let tool_result = entries
            .iter()
            .find(|e| e["type"] == "tool_result")
            .expect("journal must contain a tool_result entry");
        assert_eq!(
            tool_result["is_error"], false,
            "tool_result must have is_error=false on success"
        );
        // The spawned agent's name is its content; it was provisioned by the
        // real coordinator so the name has the form "test-persona-<hex>".
        let content_str = tool_result["content"].as_str().unwrap_or("");
        assert!(
            content_str.starts_with("test-persona-"),
            "tool_result content must be the spawned agent name: {content_str}"
        );
    }

    // IT-2: When spawn_agent is called with a persona that does not exist,
    // the tool result has is_error=true and the content mentions "persona not found".
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "full path from tool invocation through coordinator error reply to journal \
                  assertion; splitting would obscure the end-to-end error propagation"
    )]
    fn spawn_agent_tool_returns_error_for_unknown_persona() {
        use crate::spawn_coordinator::SpawnCoordinator;
        use crate::test_support::{build_registries, NullDispatcher, NullInboxStarter};
        use crate::tool::SpawnAgentTool;
        use actix::Actor as _;

        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        // No persona config written — "nonexistent-persona" will not be found.

        let (identity_registry, watcher, agent_registry_path) = build_registries(&data_dir);
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();

        let lead_adapter: Arc<dyn reeve_adapter::Adapter> = Arc::new(TwoTurnSpawnAdapter {
            calls: Arc::new(std::sync::Mutex::new(0u32)),
            persona: "nonexistent-persona",
            task: "this will fail",
            tool_call_id: "tu_bad",
            preamble_text: None,
            end_turn_text: "noted",
        });
        let coord_adapter: Arc<dyn reeve_adapter::Adapter> =
            Arc::new(crate::test_support::MockAdapter::new("mock@test"));

        let data_dir_for_block = data_dir.clone();

        actix::System::new().block_on(async move {
            let null_inbox = NullInboxStarter.start();
            let null_dispatcher = NullDispatcher.start();

            let audit2 = Arc::new(
                crate::audit::AuditLog::open(data_dir_for_block.clone())
                    .expect("open audit log in test"),
            );
            let spawn_coordinator = SpawnCoordinator::new(
                data_dir_for_block.clone(),
                agent_registry_path,
                identity_registry,
                vec![Arc::clone(&coord_adapter)],
                audit2,
                Arc::clone(&watcher),
                null_inbox.recipient(),
                null_dispatcher.recipient(),
                None,
            );
            let coord_addr = Supervisor::start(move |_| spawn_coordinator);

            let spawn_tool = SpawnAgentTool::new(coord_addr.recipient(), None, None);
            let tools: Vec<(
                reeve_adapter::Tool,
                actix::Recipient<crate::tool::InvokeTool>,
            )> = vec![(SpawnAgentTool::descriptor(), spawn_tool.start().recipient())];

            let agent = mock_agent(lead_adapter, &dirs, tools).unwrap();
            let addr = Supervisor::start(move |_| agent);

            // Wait for lead to start.
            let status_path = data_dir_for_block
                .join("agents")
                .join("lead")
                .join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "lead agent did not start within 5 seconds",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            addr.do_send(ProcessInbound {
                payload: "go".to_owned(),
                message_id: "m-1".to_owned(),
                sender_id: IdentityId::new().unwrap(),
            });

            // Wait for the tool_result with is_error=true to appear in the journal.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                let has_error_result = content.lines().any(|line| {
                    if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
                        entry["type"] == "tool_result" && entry["is_error"] == true
                    } else {
                        false
                    }
                });
                if has_error_result {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "error tool_result did not appear within 10 seconds; journal:\n{content}",
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }

            actix::System::current().stop();
        });

        let content = std::fs::read_to_string(&conv_path_outer).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        let error_result = entries
            .iter()
            .find(|e| e["type"] == "tool_result" && e["is_error"] == true)
            .expect("journal must contain a tool_result with is_error=true");

        assert_eq!(error_result["tool_use_id"], "tu_bad");
        let content_str = error_result["content"].as_str().unwrap_or("");
        assert!(
            content_str.starts_with("spawn_agent:"),
            "error content must be spawn_agent: prefixed coordinator message: {content_str}"
        );
    }

    // ── load_history_from_journal unit tests ─────────────────────────────────

    #[test]
    fn load_history_from_journal_empty_file_returns_empty_vec() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        std::fs::write(&path, "").unwrap();
        let (history, _seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn load_history_from_journal_missing_file_returns_empty_vec() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("nonexistent.jsonl");
        let (history, _seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn load_history_from_journal_inbound_becomes_user_message() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        // Pair Inbound with Outbound so the turn is complete; a trailing User
        // would be truncated by crash-recovery logic.
        let lines = [
            r#"{"type":"inbound","message_id":"m1","payload":"hello","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"outbound","payload":"world","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (history, _seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, reeve_adapter::Role::User);
        assert_eq!(
            history[0].content,
            vec![reeve_adapter::MessageContent::Text("hello".to_owned())]
        );
    }

    #[test]
    fn load_history_from_journal_outbound_becomes_assistant_message() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        let line =
            r#"{"type":"outbound","payload":"world","timestamp_utc":"2024-01-01T00:00:00Z"}"#;
        std::fs::write(&path, format!("{line}\n")).unwrap();
        let (history, _seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, reeve_adapter::Role::Assistant);
        assert_eq!(
            history[0].content,
            vec![reeve_adapter::MessageContent::Text("world".to_owned())]
        );
    }

    #[test]
    fn load_history_from_journal_skips_system_and_model_call() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        let lines = [
            r#"{"type":"system","message":"startup","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":10,"output_tokens":5,"model":"test","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (history, _seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(
            history.is_empty(),
            "expected empty history for system/model_call only, got {history:?}"
        );
    }

    #[test]
    fn load_history_from_journal_reconstructs_tool_use_and_result() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        // Complete the tool turn with a final Outbound; a trailing ToolResult
        // (User role) would be truncated by crash-recovery logic.
        let lines = [
            r#"{"type":"tool_use","tool_use_id":"tu1","name":"echo","input":{"text":"hi"},"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_result","tool_use_id":"tu1","content":"hi","is_error":false,"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"outbound","payload":"done","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (history, _seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert_eq!(history.len(), 3, "expected 3 messages, got {history:?}");

        // tool_use → assistant turn
        assert_eq!(history[0].role, reeve_adapter::Role::Assistant);
        assert_eq!(
            history[0].content,
            vec![reeve_adapter::MessageContent::ToolUse {
                id: "tu1".to_owned(),
                name: "echo".to_owned(),
                input: serde_json::json!({"text": "hi"}),
            }]
        );

        // tool_result → user turn
        assert_eq!(history[1].role, reeve_adapter::Role::User);
        assert_eq!(
            history[1].content,
            vec![reeve_adapter::MessageContent::ToolResult {
                tool_use_id: "tu1".to_owned(),
                content: "hi".to_owned(),
                is_error: false,
            }]
        );

        // final outbound → assistant turn
        assert_eq!(history[2].role, reeve_adapter::Role::Assistant);
    }

    #[test]
    fn load_history_from_journal_interleaved_in_out() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        let lines = [
            r#"{"type":"inbound","message_id":"m1","payload":"ping","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"system","message":"note","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"outbound","payload":"pong","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"inbound","message_id":"m2","payload":"again","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"outbound","payload":"replied","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (history, _seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].role, reeve_adapter::Role::User);
        assert_eq!(
            history[0].content,
            vec![reeve_adapter::MessageContent::Text("ping".to_owned())]
        );
        assert_eq!(history[1].role, reeve_adapter::Role::Assistant);
        assert_eq!(
            history[1].content,
            vec![reeve_adapter::MessageContent::Text("pong".to_owned())]
        );
        assert_eq!(history[2].role, reeve_adapter::Role::User);
        assert_eq!(
            history[2].content,
            vec![reeve_adapter::MessageContent::Text("again".to_owned())]
        );
        assert_eq!(history[3].role, reeve_adapter::Role::Assistant);
        assert_eq!(
            history[3].content,
            vec![reeve_adapter::MessageContent::Text("replied".to_owned())]
        );
    }

    // Crash recovery: two complete turns followed by a crash-interrupted
    // Inbound — the trailing user message must be truncated so restart does
    // not submit a double-user turn to the adapter.
    #[test]
    fn load_history_from_journal_truncates_trailing_user_message() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        let lines = [
            r#"{"type":"inbound","message_id":"m1","payload":"turn1","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"outbound","payload":"reply1","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"inbound","message_id":"m2","payload":"turn2","timestamp_utc":"2024-01-01T00:00:01Z"}"#,
            r#"{"type":"outbound","payload":"reply2","timestamp_utc":"2024-01-01T00:00:01Z"}"#,
            // Crash-interrupted: Inbound written, Outbound never written.
            r#"{"type":"inbound","message_id":"m3","payload":"crashed","timestamp_utc":"2024-01-01T00:00:02Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (history, _seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert_eq!(
            history.len(),
            4,
            "trailing user message must be truncated; got {history:?}"
        );
        assert_eq!(
            history.last().unwrap().role,
            reeve_adapter::Role::Assistant,
            "last message after truncation must be assistant"
        );
    }

    // Edge case: a journal with only a single Inbound (very first turn
    // interrupted) must truncate to an empty history.
    #[test]
    fn load_history_from_journal_single_inbound_truncated_to_empty() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        let line = r#"{"type":"inbound","message_id":"m1","payload":"only","timestamp_utc":"2024-01-01T00:00:00Z"}"#;
        std::fs::write(&path, line).unwrap();
        let (history, _seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(
            history.is_empty(),
            "single interrupted inbound must produce empty history; got {history:?}"
        );
    }

    // Blank lines in the journal (e.g. trailing newline added by text editors)
    // must be skipped rather than causing an InvalidData error.
    #[test]
    fn load_history_from_journal_skips_blank_lines() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        let content = [
            r#"{"type":"inbound","message_id":"m1","payload":"hello","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            "",
            r#"{"type":"outbound","payload":"world","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            "",
        ]
        .join("\n");
        std::fs::write(&path, content).unwrap();
        let (history, _seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert_eq!(
            history.len(),
            2,
            "blank lines must be skipped; got {history:?}"
        );
        assert_eq!(history[0].role, reeve_adapter::Role::User);
        assert_eq!(history[1].role, reeve_adapter::Role::Assistant);
    }

    // Supervisor restart must clear seen_message_ids so the watcher's
    // re-delivery of inbox/cur/ envelopes is not silently dropped.
    #[test]
    fn restarting_clears_seen_message_ids() {
        let tmp = tempdir().unwrap();
        let dirs = AgentDirs::provision(tmp.path(), "lead").unwrap();
        let adapter = Arc::new(TextResponseAdapter::new("mock@test"));
        let mut agent = mock_agent(adapter, &dirs, Vec::new()).unwrap();

        // Simulate a message that was seen before the restart.
        agent
            .seen_message_ids
            .insert("msg-before-restart".to_owned());
        assert!(
            !agent
                .seen_message_ids
                .insert("msg-before-restart".to_owned()),
            "id must be present before restarting"
        );

        // Call restarting directly (actix::Supervised::restarting takes &mut
        // Context<Self> but we only need to verify the field reset here).
        actix::System::new().block_on(async move {
            let addr = Supervisor::start(move |ctx: &mut actix::Context<Agent>| {
                // Manually invoke the restarting path by stopping, which
                // triggers restarting → started via Supervised. Instead,
                // verify the invariant directly by inspecting state after
                // construction; the restarting handler is tested by running
                // an actor round-trip.
                //
                // Simpler approach: call restarting on a live context.
                use actix::AsyncContext as _;
                ctx.run_later(Duration::from_millis(0), |act, ctx| {
                    use actix::Supervised as _;
                    // Insert a sentinel before calling restarting.
                    act.seen_message_ids.insert("sentinel".to_owned());
                    act.restarting(ctx);
                    // After restarting, the sentinel must be gone.
                    assert!(
                        act.seen_message_ids.insert("sentinel".to_owned()),
                        "restarting must clear seen_message_ids"
                    );
                    actix::System::current().stop();
                });
                agent
            });
            let _ = addr;
        });
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "integration-style test: provisions dirs, primes the journal, \
                  drives a turn, and asserts both the journal contents and the \
                  in-memory history. Splitting fragments a tightly coupled scenario."
    )]
    fn agent_new_loads_prior_history_and_appends_after_it() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();

        // Pre-seed the journal with two entries.
        {
            let thread = crate::agent_fs::ConversationThread::open(&conversation_path).unwrap();
            thread
                .append(&ConversationEntry::Inbound {
                    message_id: "prior-1".to_owned(),
                    sender_id: None,
                    payload: "seed message".to_owned(),
                    timestamp_utc: time::OffsetDateTime::now_utc(),
                })
                .unwrap();
            thread
                .append(&ConversationEntry::Outbound {
                    payload: "seed reply".to_owned(),
                    timestamp_utc: time::OffsetDateTime::now_utc(),
                })
                .unwrap();
        }

        // Verify that load_history_from_journal sees the 2 prior entries.
        let (prior_history, _seen_ids) =
            super::load_history_from_journal(&conversation_path).unwrap();
        assert_eq!(
            prior_history.len(),
            2,
            "load_history_from_journal must see 2 prior entries"
        );
        assert_eq!(prior_history[0].role, reeve_adapter::Role::User);
        assert_eq!(prior_history[1].role, reeve_adapter::Role::Assistant);

        // Agent::new reads the same path and loads the history.
        let adapter = Arc::new(TextResponseAdapter::new("mock@test"));
        let agent = mock_agent(adapter, &dirs, Vec::new()).unwrap();

        // The agent carries the prior history in memory; verify via a turn.
        let conv_path_outer = conversation_path.clone();
        actix::System::new().block_on(async move {
            let addr = Supervisor::start(move |_| agent);

            // Wait for started.
            let status_path = data_dir.join("agents").join("lead").join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(std::time::Instant::now() <= deadline, "actor start timeout");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            addr.do_send(ProcessInbound {
                payload: "new message".to_owned(),
                message_id: "new-1".to_owned(),
                sender_id: IdentityId::new().unwrap(),
            });

            // Wait until at least: prior 2 + system(started) + inbound + outbound + model_call = 6.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let content = std::fs::read_to_string(&conversation_path).unwrap_or_default();
                if content.lines().count() >= 6 {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "journal did not reach 6 entries; content:\n{content}",
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            actix::System::current().stop();
        });

        let content = std::fs::read_to_string(&conv_path_outer).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        // Prior entries must still be first.
        let inbound_entries: Vec<_> = entries.iter().filter(|e| e["type"] == "inbound").collect();
        assert!(
            inbound_entries.len() >= 2,
            "expected at least 2 inbound entries; got: {entries:?}"
        );
        assert_eq!(
            inbound_entries[0]["message_id"], "prior-1",
            "prior entry must appear before new entry"
        );
        let new_inbound = inbound_entries.iter().find(|e| e["message_id"] == "new-1");
        assert!(
            new_inbound.is_some(),
            "new inbound entry missing; entries: {entries:?}"
        );
    }

    #[test]
    fn load_history_from_journal_returns_err_on_invalid_line() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        let lines = [
            r#"{"type":"inbound","message_id":"m1","payload":"before","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            "not valid json at all",
            r#"{"type":"outbound","payload":"after","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let result = super::load_history_from_journal(&path);
        assert!(
            result.is_err(),
            "corrupt journal line must return Err, got Ok({:?})",
            result.ok()
        );
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData,
            "corrupt journal line must return InvalidData"
        );
    }

    #[test]
    fn load_history_from_journal_merges_outbound_and_tool_use_into_one_message() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        let lines = [
            r#"{"type":"outbound","payload":"text_preamble","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_use","tool_use_id":"tu1","name":"echo","input":{"text":"hi"},"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (history, _seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert_eq!(
            history.len(),
            1,
            "consecutive assistant entries must merge into one message; got {history:?}"
        );
        assert_eq!(history[0].role, reeve_adapter::Role::Assistant);
        assert_eq!(
            history[0].content,
            vec![
                reeve_adapter::MessageContent::Text("text_preamble".to_owned()),
                reeve_adapter::MessageContent::ToolUse {
                    id: "tu1".to_owned(),
                    name: "echo".to_owned(),
                    input: serde_json::json!({"text": "hi"}),
                },
            ],
            "merged assistant message must contain Text block then ToolUse block"
        );
    }

    #[test]
    fn load_history_from_journal_merges_consecutive_tool_results_into_one_message() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        // Complete the tool turn with a final Outbound; trailing ToolResult
        // entries (User role) would be truncated by crash-recovery logic.
        let lines = [
            r#"{"type":"tool_result","tool_use_id":"tu1","content":"result1","is_error":false,"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_result","tool_use_id":"tu2","content":"result2","is_error":false,"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"outbound","payload":"response","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (history, _seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert_eq!(
            history.len(),
            2,
            "two tool_results must merge into one user message + one assistant; got {history:?}"
        );
        assert_eq!(history[0].role, reeve_adapter::Role::User);
        assert_eq!(
            history[0].content,
            vec![
                reeve_adapter::MessageContent::ToolResult {
                    tool_use_id: "tu1".to_owned(),
                    content: "result1".to_owned(),
                    is_error: false,
                },
                reeve_adapter::MessageContent::ToolResult {
                    tool_use_id: "tu2".to_owned(),
                    content: "result2".to_owned(),
                    is_error: false,
                },
            ],
            "merged user message must contain both ToolResult blocks"
        );
        assert_eq!(history[1].role, reeve_adapter::Role::Assistant);
    }

    // Fix B — new tests ────────────────────────────────────────────────────────

    // FB1: load_history_from_journal collects message_ids from completed Inbound
    // turns (each followed by an Outbound) into the returned Vec, in journal
    // order. Outbound entries must not contribute ids.
    #[test]
    fn load_history_from_journal_collects_inbound_message_ids() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        // Both turns are complete: Inbound → Outbound → ModelCall. ModelCall
        // (model_call_seen=T, tool_use_after_last_mc=F) triggers the deferred
        // commit at the next Inbound boundary and at end-of-journal.
        let lines = [
            r#"{"type":"inbound","message_id":"inbound-1","payload":"hello","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"outbound","payload":"world","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":3,"output_tokens":1,"model":"test","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"inbound","message_id":"inbound-2","payload":"again","timestamp_utc":"2024-01-01T00:00:01Z"}"#,
            r#"{"type":"outbound","payload":"replied","timestamp_utc":"2024-01-01T00:00:01Z"}"#,
            r#"{"type":"model_call","input_tokens":3,"output_tokens":1,"model":"test","timestamp_utc":"2024-01-01T00:00:01Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (_history, seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert_eq!(
            seen_ids.len(),
            2,
            "expected exactly 2 inbound message_ids; got {seen_ids:?}"
        );
        assert!(
            seen_ids.iter().any(|id| id == "inbound-1"),
            "seen_ids must contain inbound-1; got {seen_ids:?}"
        );
        assert!(
            seen_ids.iter().any(|id| id == "inbound-2"),
            "seen_ids must contain inbound-2; got {seen_ids:?}"
        );
    }

    // w2: An interrupted inbound turn (Inbound with no subsequent Outbound)
    // must NOT contribute its message_id to seen_ids, so crash-recovery
    // re-delivery is not silently dropped.
    #[test]
    fn seen_ids_excludes_interrupted_inbound() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        // First turn is complete (Inbound → Outbound → ModelCall). Second is
        // interrupted — no ModelCall written before the next Inbound arrives.
        let lines = [
            r#"{"type":"inbound","message_id":"completed-turn","payload":"hello","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"outbound","payload":"world","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":3,"output_tokens":1,"model":"test","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"inbound","message_id":"interrupted-turn","payload":"crash","timestamp_utc":"2024-01-01T00:00:01Z"}"#,
            // No outbound or ModelCall — process crashed before writing the reply.
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (_history, seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(
            seen_ids.iter().any(|id| id == "completed-turn"),
            "seen_ids must contain the completed turn's id; got {seen_ids:?}"
        );
        assert!(
            !seen_ids.iter().any(|id| id == "interrupted-turn"),
            "seen_ids must NOT contain the interrupted turn's id; got {seen_ids:?}"
        );
    }

    // GAP1: A complete tool-only turn must commit the message_id to seen_ids.
    //
    // Actual runtime journal write order for a tool turn with no preamble text
    // and an empty-text EndTurn on the second adapter call:
    //   Inbound → ModelCall(1) → ToolUse → ToolResult → ModelCall(2)
    //
    // The first ModelCall is written BEFORE ToolUse (before the crash window is
    // closed), so committing there is unsafe. The deferral fix commits at
    // end-of-journal when the last MC was not followed by a TU — here MC(2).
    // The fixture includes both ModelCalls to match reality.
    #[test]
    fn load_history_from_journal_commits_message_id_on_model_call() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        // Complete tool-only turn: Inbound → ModelCall(1) → ToolUse → ToolResult
        // → ModelCall(2). MC(2) has tool_use_after_last_mc=F → deferred commit.
        let lines = [
            r#"{"type":"inbound","message_id":"msg-tool-only","payload":"do the thing","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":5,"output_tokens":3,"model":"test","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_use","tool_use_id":"tu1","name":"spawn_agent","input":{"persona":"p","task":"t"},"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_result","tool_use_id":"tu1","content":"ok","is_error":false,"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":8,"output_tokens":1,"model":"test","timestamp_utc":"2024-01-01T00:00:01Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (_history, seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(
            seen_ids.contains("msg-tool-only"),
            "tool-only turn's message_id must be committed to seen_ids at second ModelCall; \
             got seen_ids with {} entries",
            seen_ids.len()
        );
    }

    // Crash scenario: a crash between ModelCall(1) and ToolResult leaves the
    // journal as Inbound → ModelCall(1) → ToolUse. The pending_inbound_id must
    // NOT be committed — the watcher must re-deliver so the turn can be retried.
    #[test]
    fn seen_ids_excludes_crash_between_model_call_and_tool_result() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        // Incomplete tool turn: crash between ToolUse write and ToolResult write.
        let lines = [
            r#"{"type":"inbound","message_id":"msg-crash","payload":"hi","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":5,"output_tokens":0,"model":"test","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_use","tool_use_id":"tu1","name":"spawn_agent","input":{},"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (_history, seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(
            !seen_ids.contains("msg-crash"),
            "interrupted tool turn must NOT be committed to seen_ids; \
             got seen_ids with {} entries",
            seen_ids.len()
        );
    }

    // GAP2: A complete two-iteration tool turn must commit the message_id exactly
    // once.
    //
    // Journal: Inbound → MC(1) → TU(a) → TR(a) → MC(2) → TU(b) → TR(b) → MC(3)
    //
    // With the single-iteration fix (commit at ToolResult→ModelCall boundary),
    // this would commit at MC(2) — correct for the complete turn but too early if
    // the turn crashes after TU(b). The deferral fix commits only at
    // end-of-journal when the last MC was not followed by a TU: here MC(3).
    #[test]
    fn load_history_from_journal_commits_message_id_on_two_iteration_tool_turn() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        let lines = [
            r#"{"type":"inbound","message_id":"msg-two-iter","payload":"do two things","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":5,"output_tokens":3,"model":"test","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_use","tool_use_id":"tu-a","name":"spawn_agent","input":{"persona":"p","task":"t"},"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_result","tool_use_id":"tu-a","content":"ok-a","is_error":false,"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":8,"output_tokens":3,"model":"test","timestamp_utc":"2024-01-01T00:00:01Z"}"#,
            r#"{"type":"tool_use","tool_use_id":"tu-b","name":"spawn_agent","input":{"persona":"q","task":"u"},"timestamp_utc":"2024-01-01T00:00:01Z"}"#,
            r#"{"type":"tool_result","tool_use_id":"tu-b","content":"ok-b","is_error":false,"timestamp_utc":"2024-01-01T00:00:01Z"}"#,
            r#"{"type":"model_call","input_tokens":12,"output_tokens":1,"model":"test","timestamp_utc":"2024-01-01T00:00:02Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (_history, seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(
            seen_ids.contains("msg-two-iter"),
            "two-iteration tool turn's message_id must be committed to seen_ids; \
             got seen_ids with {} entries",
            seen_ids.len()
        );
    }

    // Crash scenario: crash between TU(b) and TR(b) in a two-iteration turn.
    // Journal ends: Inbound → MC(1) → TU(a) → TR(a) → MC(2) → TU(b).
    // The message_id must NOT be committed — the watcher re-delivers for retry.
    // This is the core regression caught by priya.p1: the old state machine
    // committed at MC(2) (when tool_result_seen=T from TR(a)), so a subsequent
    // crash at TU(b) left the id permanently in seen_ids, silently dropping
    // re-delivery.
    #[test]
    fn seen_ids_excludes_crash_in_second_tool_iteration() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        let lines = [
            r#"{"type":"inbound","message_id":"msg-two-iter-crash","payload":"do two things","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":5,"output_tokens":3,"model":"test","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_use","tool_use_id":"tu-a","name":"spawn_agent","input":{"persona":"p","task":"t"},"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_result","tool_use_id":"tu-a","content":"ok-a","is_error":false,"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":8,"output_tokens":3,"model":"test","timestamp_utc":"2024-01-01T00:00:01Z"}"#,
            r#"{"type":"tool_use","tool_use_id":"tu-b","name":"spawn_agent","input":{"persona":"q","task":"u"},"timestamp_utc":"2024-01-01T00:00:01Z"}"#,
            // crash here — TR(b) was never written
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (_history, seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(
            !seen_ids.contains("msg-two-iter-crash"),
            "crash in second tool iteration must NOT commit message_id to seen_ids; \
             got seen_ids with {} entries",
            seen_ids.len()
        );
    }

    // Preamble-text tool turn crash: journal has Outbound before ToolUse.
    // Sequence: Inbound → Outbound("preamble") → MC(1) → ToolUse(a) → [crash]
    // The Outbound arm must NOT commit pending_inbound_id — the turn is incomplete
    // because ToolUse(a) has not been followed by its ToolResult + final MC.
    // This is the core m1 regression: the old code committed at Outbound, which
    // would silently drop re-delivery after a crash in the tool round-trip.
    #[test]
    fn seen_ids_excludes_crash_in_preamble_text_tool_turn() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        let lines = [
            r#"{"type":"inbound","message_id":"msg-preamble-crash","payload":"do it","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"outbound","payload":"Sure, I'll do that.","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":5,"output_tokens":3,"model":"test","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_use","tool_use_id":"tu1","name":"spawn_agent","input":{"persona":"p","task":"t"},"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            // crash here — TR and final MC never written
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (_history, seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(
            !seen_ids.contains("msg-preamble-crash"),
            "crash in preamble-text tool turn must NOT commit message_id to seen_ids; \
             got seen_ids with {} entries",
            seen_ids.len()
        );
    }

    // Positive case: complete preamble-text tool turn commits the message_id.
    // Sequence: Inbound → Outbound("preamble") → MC(1) → ToolUse → ToolResult → MC(2)
    // MC(2) has tool_use_after_last_mc=F → deferred commit fires at end-of-journal.
    #[test]
    fn load_history_from_journal_commits_message_id_for_preamble_text_tool_turn() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        let lines = [
            r#"{"type":"inbound","message_id":"msg-preamble-complete","payload":"do it","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"outbound","payload":"Sure, I'll do that.","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":5,"output_tokens":3,"model":"test","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_use","tool_use_id":"tu1","name":"spawn_agent","input":{"persona":"p","task":"t"},"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_result","tool_use_id":"tu1","content":"ok","is_error":false,"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":8,"output_tokens":1,"model":"test","timestamp_utc":"2024-01-01T00:00:01Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (_history, seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(
            seen_ids.contains("msg-preamble-complete"),
            "complete preamble-text tool turn must commit message_id after final MC; \
             got seen_ids with {} entries",
            seen_ids.len()
        );
    }

    // Crash after ToolResult but before the following ModelCall.
    // Sequence: Inbound → MC(1) → TU(a) → TR(a) → [crash before MC(2)]
    // tool_use_after_last_mc is still true (set by TU(a), not yet reset by MC(2)),
    // so the deferred-commit condition is false → id NOT committed → watcher re-delivers.
    #[test]
    fn seen_ids_excludes_crash_after_tool_result_before_mc() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        let lines = [
            r#"{"type":"inbound","message_id":"msg-tr-crash","payload":"do it","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":5,"output_tokens":3,"model":"test","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_use","tool_use_id":"tu1","name":"spawn_agent","input":{"persona":"p","task":"t"},"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"tool_result","tool_use_id":"tu1","content":"ok","is_error":false,"timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            // crash here — MC(2) never written
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (_history, seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(
            !seen_ids.contains("msg-tr-crash"),
            "crash after TR but before next MC must NOT commit message_id to seen_ids; \
             got seen_ids with {} entries",
            seen_ids.len()
        );
    }

    // priya.p3: Real production sequence for text-response turns. The runtime
    // writes Outbound followed by ModelCall. Verify the id is committed and the
    // post-loop condition does not erroneously skip it.
    #[test]
    fn load_history_from_journal_commits_message_id_for_real_text_turn_sequence() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        // Inbound → Outbound → ModelCall: the trailing ModelCall
        // (model_call_seen=T, tool_use_after_last_mc=F) triggers the deferred
        // commit. Outbound no longer commits (preamble-text tool turns use the
        // same arm and must not commit before the tool round-trip closes).
        let lines = [
            r#"{"type":"inbound","message_id":"msg-text-real","payload":"hello","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"outbound","payload":"world","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":3,"output_tokens":1,"model":"test","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (_history, seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(
            seen_ids.contains("msg-text-real"),
            "text-turn id must be committed by deferred-MC arm; \
             got seen_ids with {} entries",
            seen_ids.len()
        );
        assert_eq!(
            seen_ids.len(),
            1,
            "exactly one id must be committed (no double-commit from trailing MC); \
             got seen_ids with {} entries",
            seen_ids.len()
        );
    }

    // GAP1b: A tool-only turn where the model returns EndTurn with empty text
    // immediately (no tool calls at all) produces a journal with only
    // Inbound + ModelCall. The message_id must still be committed.
    #[test]
    fn load_history_from_journal_commits_message_id_on_model_call_no_tools() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        // Minimal turn: Inbound followed immediately by ModelCall (no tool calls,
        // no Outbound — model returned EndTurn with empty text).
        let lines = [
            r#"{"type":"inbound","message_id":"msg-empty-endturn","payload":"hello","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            r#"{"type":"model_call","input_tokens":3,"output_tokens":1,"model":"test","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (_history, seen_ids) = super::load_history_from_journal(&path).unwrap();
        assert!(
            seen_ids.contains("msg-empty-endturn"),
            "empty-EndTurn turn's message_id must be committed to seen_ids via ModelCall arm; \
             got seen_ids with {} entries",
            seen_ids.len()
        );
    }

    // g3: SeenIds::from_vec truncates to the cap when given more than
    // SEEN_MESSAGE_IDS_CAP entries, retaining the last (most-recent) entries.
    #[test]
    fn seen_ids_from_vec_truncates_to_cap() {
        let cap = super::SEEN_MESSAGE_IDS_CAP;
        // Build a Vec with cap+1 distinct entries. The extra entry is at index 0
        // (oldest), so after truncation it must not be present.
        let ids: Vec<String> = (0..=cap).map(|i| format!("msg-{i}")).collect();
        let oldest = ids[0].clone();
        let newest = ids[cap].clone();

        let seen = super::SeenIds::from_vec(&ids);

        assert_eq!(
            seen.order.len(),
            cap,
            "SeenIds must hold exactly SEEN_MESSAGE_IDS_CAP entries after truncation; \
             got {}",
            seen.order.len()
        );
        assert!(
            !seen.set.contains(&oldest),
            "oldest entry must have been evicted by cap truncation; oldest = {oldest}"
        );
        assert!(
            seen.set.contains(&newest),
            "newest entry must be retained; newest = {newest}"
        );
    }

    // FB2: An agent constructed with a journal that already contains an Inbound
    // entry for a message_id drops a subsequent ProcessInbound with that same id
    // without calling the adapter. Uses MockAdapter (always BadRequest); if the
    // adapter were called, an error journal entry would be written — assert none
    // appears.
    #[expect(
        clippy::too_many_lines,
        reason = "the probe-based drain requires setup, actor lifecycle, journal polling, \
                  and multiple post-assertions; each step is necessary and splitting \
                  into helpers would obscure the test's intent"
    )]
    #[test]
    fn seen_ids_rebuilt_from_journal_prevents_reprocessing() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();

        // Pre-seed the journal with a completed turn so the message_id is
        // present and the history ends on an assistant turn (no truncation).
        {
            let thread = crate::agent_fs::ConversationThread::open(&conversation_path).unwrap();
            thread
                .append(&ConversationEntry::Inbound {
                    message_id: "msg-already-processed".to_owned(),
                    sender_id: None,
                    payload: "prior turn".to_owned(),
                    timestamp_utc: time::OffsetDateTime::now_utc(),
                })
                .unwrap();
            thread
                .append(&ConversationEntry::Outbound {
                    payload: "prior reply".to_owned(),
                    timestamp_utc: time::OffsetDateTime::now_utc(),
                })
                .unwrap();
            thread
                .append(&ConversationEntry::ModelCall {
                    input_tokens: 3,
                    output_tokens: 1,
                    model: "test".to_owned(),
                    timestamp_utc: time::OffsetDateTime::now_utc(),
                })
                .unwrap();
        }

        // MockAdapter always returns BadRequest. If a call is made the agent
        // appends a system error entry to the journal.
        let adapter = Arc::new(crate::test_support::MockAdapter::new("mock@test"));
        let agent = mock_agent(adapter, &dirs, Vec::new()).unwrap();

        let conv_path_outer = conversation_path.clone();
        let conv_path_assert = conv_path_outer.clone();
        actix::System::new().block_on(async move {
            let addr = Supervisor::start(move |_| agent);

            // Wait for the actor to start (status file appears).
            let status_path = data_dir.join("agents").join("lead").join("status");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if status_path.exists() {
                    break;
                }
                assert!(
                    std::time::Instant::now() <= deadline,
                    "actor did not start within 5 seconds"
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            // Send a ProcessInbound with the already-seen message_id.
            addr.do_send(ProcessInbound {
                payload: "re-delivered payload".to_owned(),
                message_id: "msg-already-processed".to_owned(),
                sender_id: IdentityId::new().unwrap(),
            });

            // Send a distinct probe message after the duplicate. The actor
            // processes messages sequentially; once the probe's Inbound entry
            // appears in the journal, the duplicate has already been handled
            // (or dropped), draining the mailbox past it.
            addr.do_send(ProcessInbound {
                payload: "probe".to_owned(),
                message_id: "msg-probe".to_owned(),
                sender_id: IdentityId::new().unwrap(),
            });

            // Poll until the probe's Inbound entry is visible in the journal.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let content = std::fs::read_to_string(&conv_path_outer).unwrap_or_default();
                    let probe_present = content.lines().filter(|l| !l.is_empty()).any(|l| {
                        serde_json::from_str::<serde_json::Value>(l)
                            .map(|e| e["type"] == "inbound" && e["message_id"] == "msg-probe")
                            .unwrap_or(false)
                    });
                    if probe_present {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("probe Inbound entry never appeared in journal within 5 s");

            actix::System::current().stop();
        });

        // The duplicate must have been silently dropped. If the adapter was
        // called it would fail (BadRequest) and the agent would append a
        // system error entry. Verify no such entry is present — the journal
        // should contain only the pre-seeded Inbound + Outbound + ModelCall +
        // system(started) plus the probe's entries.
        let content = std::fs::read_to_string(&conv_path_assert).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();

        // The pre-seeded entry must be present exactly once; the re-delivered
        // duplicate must not have produced a second journal entry.
        let already_processed_inbound_count = entries
            .iter()
            .filter(|e| e["type"] == "inbound" && e["message_id"] == "msg-already-processed")
            .count();
        assert_eq!(
            already_processed_inbound_count, 1,
            "duplicate inbound must not produce a second journal entry; entries: {entries:?}"
        );

        // The probe must be present (confirms the actor ran past the duplicate).
        let probe_inbound_count = entries
            .iter()
            .filter(|e| e["type"] == "inbound" && e["message_id"] == "msg-probe")
            .count();
        assert_eq!(
            probe_inbound_count, 1,
            "probe Inbound entry must appear exactly once; entries: {entries:?}"
        );

        // The probe triggers a MockAdapter error ("adapter call failed"). The duplicate
        // must NOT have triggered a second adapter call. Count adapter-error system
        // entries and assert exactly 1 (from the probe).
        let adapter_call_count = entries
            .iter()
            .filter(|e| {
                e["type"] == "system"
                    && e["message"]
                        .as_str()
                        .map(|m| m.contains("adapter call failed"))
                        .unwrap_or(false)
            })
            .count();
        assert_eq!(
            adapter_call_count, 1,
            "exactly 1 adapter call expected (probe only); duplicate must not trigger adapter; entries: {entries:?}"
        );
    }

    // P2: A journal file that exists but cannot be read returns Err, not Ok.
    #[test]
    #[cfg(unix)]
    fn load_history_from_journal_unreadable_file_returns_err() {
        use std::os::unix::fs::PermissionsExt;

        // root bypasses DAC; chmod 0 does not deny reads.
        let uid_output = std::process::Command::new("id")
            .arg("-u")
            .output()
            .expect("id -u must be available on unix");
        let uid = String::from_utf8_lossy(&uid_output.stdout)
            .trim()
            .parse::<u32>()
            .unwrap_or(0);
        if uid == 0 {
            return;
        }

        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conv.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"system","message":"x","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = super::load_history_from_journal(&path);

        // tempdir cleanup panics if it cannot unlink the file.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(
            result.is_err(),
            "unreadable file should return Err, got Ok({:?})",
            result.ok()
        );
    }

    // ── Cost threshold tests ──────────────────────────────────────────────────

    /// Build a mock agent with a given `Thresholds` and the `TextResponseAdapter`
    /// (42 µUSD/call).
    fn agent_with_thresholds(
        data_dir: &std::path::Path,
        dirs: &AgentDirs,
        thresholds: crate::capability::Thresholds,
    ) -> Agent {
        Agent::new(
            Arc::new(TextResponseAdapter::new("mock@test")),
            dirs,
            mock_snapshot(),
            String::new(),
            IdentityId::new().unwrap(),
            reeve_types::Keypair::generate(),
            Vec::new(),
            thresholds,
            None,
            data_dir.to_path_buf(),
        )
        .unwrap()
    }

    /// Poll the journal file until it contains at least `min_entries` entries
    /// or the timeout expires. Returns the lines.
    async fn poll_journal(path: &std::path::Path, min_entries: usize) -> Vec<String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            let lines: Vec<String> = content.lines().map(str::to_owned).collect();
            if lines.len() >= min_entries {
                return lines;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {min_entries} journal entries; got {}",
                lines.len()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // CT1: When cost_per_agent is set to 42 µUSD (= $0.000042) and the agent has
    // already spent that amount (after one successful call), the next adapter call
    // is refused: a system entry carrying the threshold refusal JSON is appended
    // and the agent returns to idle.
    #[test]
    fn cost_per_agent_threshold_refuses_when_exceeded() {
        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();

        // 42 µUSD limit — TextResponseAdapter returns exactly 42 µUSD per call,
        // so after the first call the running cost equals the limit and the
        // second call must be refused.
        let thresholds = crate::capability::Thresholds {
            cost_per_agent: Some(0.000_042), // 42 µUSD in USD
            ..Default::default()
        };
        let agent = agent_with_thresholds(&data_dir, &dirs, thresholds);

        let conv_path = dirs.conversation_path();
        let sender_id = IdentityId::new().unwrap();

        actix::System::new().block_on(async move {
            let addr = Supervisor::start(move |_| agent);

            // First message → adapter call succeeds, cost updates to 42 µUSD.
            addr.do_send(ProcessInbound {
                message_id: "ct1-msg-1".to_owned(),
                sender_id,
                payload: "hello".to_owned(),
            });
            // Wait: system(started) + inbound + outbound + model_call = 4 entries
            poll_journal(&conv_path, 4).await;

            // Second message → threshold check fires before the adapter call.
            addr.do_send(ProcessInbound {
                message_id: "ct1-msg-2".to_owned(),
                sender_id,
                payload: "hello again".to_owned(),
            });
            // Wait for the inbound + system(refusal) entries.
            let lines = poll_journal(&conv_path, 6).await;

            let has_threshold_refusal = lines
                .iter()
                .any(|l| l.contains("\"type\":\"system\"") && l.contains("cost_per_agent"));
            assert!(
                has_threshold_refusal,
                "journal must contain a threshold-refusal system entry; entries: {lines:?}"
            );

            actix::System::current().stop();
        });
    }

    // CT2: When cost_per_session is set to $0.04 and another agent's cost file
    // already shows $0.04, the first adapter call on this agent is refused because
    // the session total (0.04 + 0) >= 0.04.
    #[test]
    fn cost_per_session_threshold_refuses_when_session_total_exceeded() {
        let tmp = crate::test_support::secure_dir();
        let data_dir = tmp.path().to_path_buf();

        // Provision the lead dirs first so agents/ is created with the right
        // permissions (0o700); then add the peer cost file inside it.
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let peer_dirs = AgentDirs::provision(&data_dir, "worker-peer").unwrap();
        std::fs::write(peer_dirs.cost_path(), "0.040000").unwrap();
        let thresholds = crate::capability::Thresholds {
            cost_per_session: Some(0.04),
            ..Default::default()
        };
        let agent = agent_with_thresholds(&data_dir, &dirs, thresholds);

        let conv_path = dirs.conversation_path();
        let sender_id = IdentityId::new().unwrap();

        actix::System::new().block_on(async move {
            let addr = Supervisor::start(move |_| agent);

            addr.do_send(ProcessInbound {
                message_id: "ct2-msg-1".to_owned(),
                sender_id,
                payload: "hello".to_owned(),
            });
            // Wait: system(started) + inbound + system(refusal) = 3 entries
            let lines = poll_journal(&conv_path, 3).await;

            let has_session_refusal = lines
                .iter()
                .any(|l| l.contains("\"type\":\"system\"") && l.contains("cost_per_session"));
            assert!(
                has_session_refusal,
                "journal must contain a cost_per_session refusal system entry; entries: {lines:?}"
            );

            actix::System::current().stop();
        });
    }
}
