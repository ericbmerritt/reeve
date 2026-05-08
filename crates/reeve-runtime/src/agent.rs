//! Agent actor: receive inbound envelopes, drive the adapter / tool-call loop,
//! and record the conversation journal, status, and cost.
//!
//! A single supervised actor that processes one [`ProcessInbound`] message at
//! a time. An inbound message drives the agent through one or more adapter
//! calls — text-only turns finish in one call; tool-use turns drive a loop:
//! the model returns tool calls, the agent dispatches them as [`InvokeTool`]
//! messages to the registered tool actors, collects [`ToolResult`] replies
//! into the conversation history, and calls the adapter again. The loop
//! terminates on `FinishReason::EndTurn` or when [`MAX_TOOL_ITERATIONS`] is
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
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use actix::{Actor, ActorContext, AsyncContext, Context, Handler, Recipient, Supervised};
use time::OffsetDateTime;
use tracing::{debug, info, warn};

use crate::agent_fs::{
    AgentDirs, AgentFsError, AtomicFileWriter, ConversationEntry, ConversationThread,
};
use crate::model_resolution::SpawnSnapshot;
use crate::tool::{InvokeTool, ToolResult};

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
    /// Path to `agents/lead/inbox/cur/`. Watched for new verified envelopes.
    inbox_cur: PathBuf,
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
struct SeenIds {
    set: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenIds {
    fn new() -> Self {
        Self::default()
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
        reason = "agent constructor wires together six independent collaborators; \
                  bundling into a struct trades complexity for indirection"
    )]
    pub fn new(
        adapter: Arc<dyn reeve_adapter::Adapter>,
        dirs: &AgentDirs,
        snapshot: SpawnSnapshot,
        system_prompt: String,
        agent_id: reeve_types::IdentityId,
        tools: Vec<(reeve_adapter::Tool, Recipient<InvokeTool>)>,
    ) -> Result<Self, AgentError> {
        let conversation =
            ConversationThread::open(&dirs.conversation_path()).map_err(AgentError::Fs)?;
        let status_writer = AtomicFileWriter::new(dirs.status_path()).map_err(AgentError::Fs)?;
        let cost_writer = AtomicFileWriter::new(dirs.cost_path()).map_err(AgentError::Fs)?;
        let inbox_cur = dirs.inbox_root().join("cur");

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
            history: Vec::new(),
            inbox_cur,
            tool_descriptors,
            tool_routes,
            pending_tool_use_ids: HashSet::new(),
            pending_results: Vec::new(),
            tool_iteration: 0,
            agent_id,
            pending_inbound: VecDeque::new(),
            seen_message_ids: SeenIds::new(),
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
    fn append_inbound(&self, message_id: &str, payload: &str, ctx: &mut Context<Self>) -> bool {
        let entry = ConversationEntry::Inbound {
            message_id: message_id.to_owned(),
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

    /// Initialize the agent: record the start event, write idle status, and
    /// start the inbox/cur/ watcher that forwards verified envelopes.
    fn started(&mut self, ctx: &mut Context<Self>) {
        info!(adapter = %self.snapshot.adapter_id, "agent ready");
        self.append_system_entry("agent started", ctx);
        self.set_idle(ctx);

        let addr = ctx.address();
        watch_inbox_cur(&self.inbox_cur, addr);
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
        self.set_idle(ctx);
    }
}

// ── Handler<ProcessInbound> ───────────────────────────────────────────────────

impl Handler<ProcessInbound> for Agent {
    type Result = ();

    fn handle(&mut self, msg: ProcessInbound, ctx: &mut Context<Self>) {
        // Dedup by message_id. The inbox/cur/ watcher scans the whole
        // directory on every filesystem event, so the same envelope can
        // arrive as ProcessInbound multiple times.
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
        info!(message_id = %msg.message_id, "processing");
        self.in_flight = true;
        if self.status_writer.write("working").is_err() {
            ctx.stop();
            return;
        }
        if !self.append_inbound(&msg.message_id, &msg.payload, ctx) {
            return;
        }
        self.history.push(reeve_adapter::Message {
            role: reeve_adapter::Role::User,
            content: vec![reeve_adapter::MessageContent::Text(msg.payload)],
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
            if !self.append_inbound(&msg.message_id, &msg.payload, ctx) {
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
            combined.push_str(&msg.payload);
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

// ── inbox/cur/ watcher ────────────────────────────────────────────────────────

/// Read a verified envelope file from `inbox/cur/` and return the body as a
/// UTF-8 string, or `None` if the file cannot be read or decoded.
///
/// The file is opened with `O_NOFOLLOW`. The body field of the [`Envelope`] is
/// already decoded from its base64 wire representation by `serde_json`.
fn read_envelope_payload(path: &Path) -> Option<String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    crate::fs_util::set_nofollow(&mut options);
    let mut file = options.open(path).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let envelope: reeve_types::Envelope = serde_json::from_slice(&buf).ok()?;
    String::from_utf8(envelope.body).ok()
}

/// Send one [`ProcessInbound`] message for each path in the `cur/` directory.
///
/// Called both at startup (crash-recovery for envelopes deposited before the
/// watcher subscribed) and on every filesystem event in `cur/`. Calling on
/// every event guarantees no envelope is missed across the variations of
/// `FSEvents` and `inotify` behavior, but it also produces duplicate
/// `ProcessInbound` messages for envelopes still on disk. The agent dedups
/// these by `message_id` in [`Handler<ProcessInbound>`].
fn scan_cur(inbox_cur: &Path, addr: &actix::Addr<Agent>) {
    let Ok(entries) = std::fs::read_dir(inbox_cur) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(payload) = read_envelope_payload(&path) {
            let message_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_owned();
            addr.do_send(ProcessInbound {
                payload,
                message_id,
            });
        }
    }
}

/// Watch `inbox/cur/` for `Create` events and forward each new verified
/// envelope to the [`Agent`] as a [`ProcessInbound`] message.
///
/// Subscribes to filesystem events first, then performs a one-shot startup
/// scan so envelopes deposited before this function ran are not silently
/// dropped (crash-recovery). If `notify` setup fails the function returns
/// silently; the agent can still be spawned and respond if messages are
/// delivered after the next restart.
fn watch_inbox_cur(inbox_cur: &Path, addr: actix::Addr<Agent>) {
    use notify::{RecursiveMode, Watcher as _};
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel();
    let Ok(mut watcher) = notify::RecommendedWatcher::new(tx, notify::Config::default()) else {
        return;
    };
    if watcher
        .watch(inbox_cur, RecursiveMode::NonRecursive)
        .is_err()
    {
        return;
    }

    // Crash-recovery: process files already in cur/ before the watch started.
    scan_cur(inbox_cur, &addr);

    // Spawn a detached OS thread for the blocking event loop.
    //
    // `tokio::task::spawn_blocking` is intentionally NOT used here: tokio
    // runtime shutdown waits for all `spawn_blocking` threads to complete,
    // which would cause the runtime to hang indefinitely because the watcher
    // loop exits only when the `RecommendedWatcher` is dropped (which happens
    // only after the loop exits — a deadlock). A detached `std::thread` is
    // not owned by the tokio runtime and is killed by the OS when the process
    // exits normally.
    let inbox_cur = inbox_cur.to_owned();
    let _ = std::thread::spawn(move || {
        // Hold watcher alive for the duration of the loop.
        let _watcher = watcher;
        // Scan on every notification regardless of event kind. Discriminating
        // event types is fragile: on macOS FSEvents an atomic rename(2)
        // produces Modify(Name(To)) not Create, and the mapping varies by
        // platform. Re-scanning produces duplicate ProcessInbound messages
        // for envelopes still on disk; the agent dedups them by message_id.
        while rx.recv().is_ok() {
            scan_cur(&inbox_cur, &addr);
        }
    });
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

    // ── Mock adapters ─────────────────────────────────────────────────────────

    struct MockAdapter {
        id: &'static str,
    }

    impl MockAdapter {
        fn new(id: &'static str) -> Self {
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
            capability_profile: None,
            adapter_id: String::from("mock@test"),
            agent_id: String::new(),
        }
    }

    // L1: Agent::new succeeds with a valid adapter and provisioned dirs.
    #[test]
    fn lead_agent_new_creates_valid_actor() {
        let tmp = tempdir().unwrap();
        let dirs = AgentDirs::provision(tmp.path(), "lead").unwrap();
        let adapter = Arc::new(MockAdapter::new("mock@test"));
        let result = Agent::new(
            adapter,
            &dirs,
            mock_snapshot(),
            String::new(),
            reeve_types::IdentityId::new().unwrap(),
            Vec::new(),
        );
        assert!(result.is_ok(), "Agent::new should succeed");
    }

    // L2: After the actor starts, the status file contains "idle".
    #[test]
    fn lead_agent_started_writes_idle_status() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let status_path = dirs.status_path();
        let adapter = Arc::new(MockAdapter::new("mock@test"));
        let agent = Agent::new(
            adapter,
            &dirs,
            mock_snapshot(),
            String::new(),
            reeve_types::IdentityId::new().unwrap(),
            Vec::new(),
        )
        .unwrap();

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
        let adapter = Arc::new(MockAdapter::new("mock@test"));
        let agent = Agent::new(
            adapter,
            &dirs,
            mock_snapshot(),
            String::new(),
            reeve_types::IdentityId::new().unwrap(),
            Vec::new(),
        )
        .unwrap();

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
        let adapter = Arc::new(MockAdapter::new("mock@test"));
        let agent = Agent::new(
            adapter,
            &dirs,
            mock_snapshot(),
            String::new(),
            reeve_types::IdentityId::new().unwrap(),
            Vec::new(),
        )
        .unwrap();

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
    #[expect(
        clippy::too_many_lines,
        reason = "two-message integration test with status, journal, and \
                  model-call assertions; splitting fragments the narrative"
    )]
    fn lead_agent_second_message_queues_during_in_flight() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();
        let adapter = Arc::new(SlowMockAdapter);
        let agent = Agent::new(
            adapter,
            &dirs,
            mock_snapshot(),
            String::new(),
            reeve_types::IdentityId::new().unwrap(),
            Vec::new(),
        )
        .unwrap();

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
            });
            addr.do_send(ProcessInbound {
                payload: String::from("second"),
                message_id: String::from("msg-2"),
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
    #[expect(
        clippy::too_many_lines,
        reason = "two-phase integration test with sequential poll loops; \
                  splitting fragments the narrative"
    )]
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
        let agent = Agent::new(
            adapter,
            &dirs,
            mock_snapshot(),
            String::new(),
            reeve_types::IdentityId::new().unwrap(),
            Vec::new(),
        )
        .unwrap();

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
        assert_eq!(
            calls[1][0].content,
            vec![reeve_adapter::MessageContent::Text(
                "second message".to_owned()
            )],
            "second adapter call should carry the second user message"
        );
    }

    // L8: When the adapter returns an error, the actor returns to idle,
    // the user message is removed from history, and the journal has a
    // "adapter call failed: ..." system entry.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "integration test with sequential poll loops for journal and \
                  status assertions; splitting fragments the narrative"
    )]
    fn lead_agent_adapter_error_returns_to_idle() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();
        let adapter = Arc::new(AlwaysErrorAdapter);
        let agent = Agent::new(
            adapter,
            &dirs,
            mock_snapshot(),
            String::new(),
            reeve_types::IdentityId::new().unwrap(),
            Vec::new(),
        )
        .unwrap();
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
        let adapter = Arc::new(MockAdapter::new("mock@test"));
        let agent = Agent::new(
            adapter,
            &dirs,
            mock_snapshot(),
            String::new(),
            reeve_types::IdentityId::new().unwrap(),
            Vec::new(),
        )
        .unwrap();

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

    struct TwoTurnEchoAdapter {
        calls: Arc<std::sync::Mutex<u32>>,
    }

    #[async_trait::async_trait]
    impl reeve_adapter::Adapter for TwoTurnEchoAdapter {
        fn id(&self) -> &'static str {
            "two-turn-echo@test"
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
                    vec![reeve_adapter::MessageContent::Text(
                        "calling echo".to_owned(),
                    )],
                    vec![reeve_adapter::ToolCall {
                        id: "tu_1".to_owned(),
                        name: "echo".to_owned(),
                        arguments: serde_json::json!({ "text": "hello world" }),
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
                    vec![reeve_adapter::MessageContent::Text("done!".to_owned())],
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
    // EchoTool fires, ToolResult flows back, adapter returns EndTurn on call 2,
    // the agent goes idle. Journal must contain tool_use and tool_result entries
    // with matching tool_use_id, and the final response text.
    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "end-to-end loop test with multi-stage poll loops and journal \
                  assertions; splitting fragments the narrative"
    )]
    fn agent_tool_loop_round_trips_through_echo_tool() {
        use crate::tool::EchoTool;
        use actix::Actor as _;

        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();
        let calls_log = Arc::new(std::sync::Mutex::new(0u32));
        let calls_log_assert = Arc::clone(&calls_log);
        let adapter = Arc::new(TwoTurnEchoAdapter { calls: calls_log });

        actix::System::new().block_on(async move {
            // Start the EchoTool actor and assemble the route.
            let echo_addr = EchoTool.start();
            let tools: Vec<(
                reeve_adapter::Tool,
                actix::Recipient<crate::tool::InvokeTool>,
            )> = vec![(EchoTool::descriptor(), echo_addr.recipient())];

            let agent = Agent::new(
                adapter,
                &dirs,
                mock_snapshot(),
                String::new(),
                reeve_types::IdentityId::new().unwrap(),
                tools,
            )
            .unwrap();
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
        assert_eq!(tool_use["name"], "echo");
        assert_eq!(
            tool_use["input"],
            serde_json::json!({ "text": "hello world" })
        );

        // Tool-result entry with matching id and the echoed content.
        let tool_result = entries
            .iter()
            .find(|e| e["type"] == "tool_result")
            .expect("tool_result entry missing");
        assert_eq!(tool_result["tool_use_id"], "tu_1");
        assert_eq!(tool_result["content"], "hello world");
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
                    name: "echo".to_owned(),
                    arguments: serde_json::json!({ "text": "spin" }),
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
        use crate::tool::EchoTool;
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
            let echo_addr = EchoTool.start();
            let tools: Vec<(
                reeve_adapter::Tool,
                actix::Recipient<crate::tool::InvokeTool>,
            )> = vec![(EchoTool::descriptor(), echo_addr.recipient())];

            let agent = Agent::new(
                adapter,
                &dirs,
                mock_snapshot(),
                String::new(),
                reeve_types::IdentityId::new().unwrap(),
                tools,
            )
            .unwrap();
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
                        name: "echo".to_owned(),
                        arguments: serde_json::json!({ "text": "x" }),
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
        use crate::tool::EchoTool;
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
            let echo_addr = EchoTool.start();
            let tools: Vec<(
                reeve_adapter::Tool,
                actix::Recipient<crate::tool::InvokeTool>,
            )> = vec![(EchoTool::descriptor(), echo_addr.recipient())];

            let agent = Agent::new(
                adapter,
                &dirs,
                mock_snapshot(),
                String::new(),
                reeve_types::IdentityId::new().unwrap(),
                tools,
            )
            .unwrap();
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
        use crate::tool::EchoTool;
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
            // Register only EchoTool; the model will call "nonexistent_tool".
            let echo_addr = EchoTool.start();
            let tools: Vec<(
                reeve_adapter::Tool,
                actix::Recipient<crate::tool::InvokeTool>,
            )> = vec![(EchoTool::descriptor(), echo_addr.recipient())];

            let agent = Agent::new(
                adapter,
                &dirs,
                mock_snapshot(),
                String::new(),
                reeve_types::IdentityId::new().unwrap(),
                tools,
            )
            .unwrap();
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
        use crate::tool::EchoTool;
        use actix::Actor as _;

        let tmp = tempdir().unwrap();
        let dirs = AgentDirs::provision(tmp.path(), "lead").unwrap();
        let adapter = Arc::new(MockAdapter::new("mock@test"));

        actix::System::new().block_on(async move {
            let a = EchoTool.start();
            let b = EchoTool.start();
            // Both bindings declare the same name (EchoTool::descriptor()
            // returns "echo").
            let tools = vec![
                (EchoTool::descriptor(), a.recipient()),
                (EchoTool::descriptor(), b.recipient()),
            ];
            let result = Agent::new(
                adapter,
                &dirs,
                mock_snapshot(),
                String::new(),
                reeve_types::IdentityId::new().unwrap(),
                tools,
            );
            match result {
                Err(super::AgentError::DuplicateToolName(name)) => {
                    assert_eq!(name, "echo");
                }
                Err(other) => panic!("expected DuplicateToolName, got {other:?}"),
                Ok(_) => panic!("expected error, got Ok"),
            }
            actix::System::current().stop();
        });
    }
}
