//! Lead agent actor: receive inbound envelopes, call the adapter, and record
//! the conversation journal, status, and cost.
//!
//! A single supervised actor that processes one [`ProcessInbound`] message at a time.
//! Inbound messages drive a round-trip through the registered adapter, and each
//! exchange is appended to the JSONL conversation journal maintained by
//! [`ConversationThread`].
//!
//! Lifecycle:
//! - `started` — writes `"idle"` to the status file and records a system entry.
//! - `restarting` — re-writes `"idle"` after supervisor-driven restart.
//! - `Handler<ProcessInbound>` — transitions status to `"working"`, calls the
//!   adapter, then transitions back to `"idle"` on completion or error.

use std::fmt;
use std::sync::Arc;

use actix::{Actor, ActorContext, AsyncContext, Context, Handler, Supervised};
use time::OffsetDateTime;

use crate::agent_fs::{
    AgentDirs, AgentFsError, AtomicFileWriter, ConversationEntry, ConversationThread,
};
use crate::model_resolution::SpawnSnapshot;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors produced by the lead agent actor and its constructor.
#[derive(Debug)]
pub enum LeadAgentError {
    /// Filesystem or JSONL journal error.
    Fs(AgentFsError),
    /// Error returned by the model adapter.
    Adapter(reeve_adapter::AdapterError),
    /// Unclassified I/O error with path context.
    Io {
        /// File that could not be opened or written.
        path: std::path::PathBuf,
        /// Underlying OS error.
        source: std::io::Error,
    },
    /// JSON serialization or deserialization error.
    Json(serde_json::Error),
}

impl fmt::Display for LeadAgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Fs(source) => write!(f, "agent fs error: {source}"),
            Self::Adapter(source) => write!(f, "adapter error: {source}"),
            Self::Io { path, source } => {
                write!(f, "lead agent IO at {}: {source}", path.display())
            }
            Self::Json(source) => write!(f, "lead agent json error: {source}"),
        }
    }
}

impl std::error::Error for LeadAgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Fs(source) => Some(source),
            Self::Adapter(source) => Some(source),
            Self::Io { source, .. } => Some(source),
            Self::Json(source) => Some(source),
        }
    }
}

// ── ProcessInbound message ────────────────────────────────────────────────────

/// Deliver an inbound envelope payload to the lead agent for processing.
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

// ── LeadAgent actor ───────────────────────────────────────────────────────────

/// Supervised actix actor that implements the lead agent's message loop.
///
/// Calls the registered adapter with the accumulated conversation history
/// and records all exchanges in an append-only JSONL journal.
pub struct LeadAgent {
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
}

/// Default maximum tokens for the adapter response.
const DEFAULT_MAX_TOKENS: u32 = 4096;

impl LeadAgent {
    /// Construct a `LeadAgent`.
    ///
    /// Opens the conversation journal and creates atomic writers for the
    /// status and cost files. Does not start the actor; call
    /// [`actix::Supervisor::start`] with a closure that invokes this.
    pub fn new(
        adapter: Arc<dyn reeve_adapter::Adapter>,
        dirs: &AgentDirs,
        snapshot: SpawnSnapshot,
        system_prompt: String,
    ) -> Result<Self, LeadAgentError> {
        let conversation =
            ConversationThread::open(&dirs.conversation_path()).map_err(LeadAgentError::Fs)?;
        let status_writer =
            AtomicFileWriter::new(dirs.status_path()).map_err(LeadAgentError::Fs)?;
        let cost_writer = AtomicFileWriter::new(dirs.cost_path()).map_err(LeadAgentError::Fs)?;
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

    /// Record the adapter response: journal outbound + model-call entries,
    /// update cost, and return to idle.
    fn handle_response(&mut self, response: &reeve_adapter::Response, ctx: &mut Context<Self>) {
        let text = extract_response_text(&response.content);
        self.history.push(reeve_adapter::Message {
            role: reeve_adapter::Role::Assistant,
            content: reeve_adapter::MessageContent::Text(text.clone()),
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

        self.set_idle(ctx);
    }

    /// Append the outbound and model-call entries to the conversation journal.
    ///
    /// Returns `true` on success. On failure stops the actor and returns
    /// `false`.
    fn append_outbound_and_model_call(
        &self,
        text: &str,
        response: &reeve_adapter::Response,
        ctx: &mut Context<Self>,
    ) -> bool {
        let outbound = ConversationEntry::Outbound {
            payload: text.to_owned(),
            timestamp_utc: OffsetDateTime::now_utc(),
        };
        if self.conversation.append(&outbound).is_err() {
            ctx.stop();
            return false;
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
    fn spawn_adapter_call(&self, ctx: &mut Context<Self>) {
        use actix::fut::WrapFuture as _;
        use actix::ActorFutureExt as _;

        let adapter = Arc::clone(&self.adapter);
        let messages = self.history.clone();
        let params = reeve_adapter::Params {
            max_tokens: DEFAULT_MAX_TOKENS,
            system_prompt: Some(self.system_prompt.clone()),
            ..reeve_adapter::Params::default()
        };
        let fut = async move { adapter.call(&messages, &[], &params).await }
            .into_actor(self)
            .map(|result, actor, inner_ctx| match result {
                Ok(response) => {
                    actor.in_flight = false;
                    actor.handle_response(&response, inner_ctx);
                }
                Err(err) => {
                    actor.in_flight = false;
                    actor.history.pop();
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

impl Actor for LeadAgent {
    type Context = Context<Self>;

    /// Initialize the agent: record the start event, write idle status.
    fn started(&mut self, ctx: &mut Context<Self>) {
        self.append_system_entry("agent started", ctx);
        self.set_idle(ctx);
    }
}

impl Supervised for LeadAgent {
    /// Recover after a supervised restart: restore idle status without
    /// re-logging the start event.
    fn restarting(&mut self, ctx: &mut Context<Self>) {
        self.in_flight = false;
        self.set_idle(ctx);
    }
}

// ── Handler<ProcessInbound> ───────────────────────────────────────────────────

impl Handler<ProcessInbound> for LeadAgent {
    type Result = ();

    fn handle(&mut self, msg: ProcessInbound, ctx: &mut Context<Self>) {
        if self.in_flight {
            let entry = ConversationEntry::System {
                message: format!(
                    "message {} discarded: adapter call in flight",
                    msg.message_id
                ),
                timestamp_utc: OffsetDateTime::now_utc(),
            };
            let _ = self.conversation.append(&entry);
            return;
        }
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
            content: reeve_adapter::MessageContent::Text(msg.payload),
        });
        self.spawn_adapter_call(ctx);
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

    use super::{LeadAgent, ProcessInbound};
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
        }
    }

    // L1: LeadAgent::new succeeds with a valid adapter and provisioned dirs.
    #[test]
    fn lead_agent_new_creates_valid_actor() {
        let tmp = tempdir().unwrap();
        let dirs = AgentDirs::provision(tmp.path(), "lead").unwrap();
        let adapter = Arc::new(MockAdapter::new("mock@test"));
        let result = LeadAgent::new(adapter, &dirs, mock_snapshot(), String::new());
        assert!(result.is_ok(), "LeadAgent::new should succeed");
    }

    // L2: After the actor starts, the status file contains "idle".
    #[test]
    fn lead_agent_started_writes_idle_status() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let status_path = dirs.status_path();
        let adapter = Arc::new(MockAdapter::new("mock@test"));
        let agent = LeadAgent::new(adapter, &dirs, mock_snapshot(), String::new()).unwrap();

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
        let agent = LeadAgent::new(adapter, &dirs, mock_snapshot(), String::new()).unwrap();

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

    // L6: LeadAgentError Display impls are non-empty and informative.
    #[test]
    fn lead_agent_error_display_impls() {
        use std::io;
        use std::path::PathBuf;

        use crate::agent_fs::AgentFsError;

        use super::LeadAgentError;

        let fs_err = LeadAgentError::Fs(AgentFsError::Io {
            path: PathBuf::from("agents/lead/status"),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        });
        let rendered = fs_err.to_string();
        assert!(!rendered.is_empty(), "Fs variant display empty");
        assert!(
            rendered.contains("agent fs"),
            "Fs variant missing context: {rendered}"
        );

        let io_err = LeadAgentError::Io {
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
        let json_err = LeadAgentError::Json(serde_err);
        let rendered = json_err.to_string();
        assert!(!rendered.is_empty(), "Json variant display empty");

        let adapter_err = LeadAgentError::Adapter(reeve_adapter::AdapterError::BadRequest {
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

    // L-B: A second ProcessInbound arriving while an adapter call is in flight
    // is silently discarded; the journal records a system entry with "discarded"
    // and the discarded message_id, and there is only one Inbound entry.
    #[test]
    fn lead_agent_second_message_discarded_while_in_flight() {
        let tmp = tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let dirs = AgentDirs::provision(&data_dir, "lead").unwrap();
        let conversation_path = dirs.conversation_path();
        let conv_path_outer = conversation_path.clone();
        let adapter = Arc::new(SlowMockAdapter);
        let agent = LeadAgent::new(adapter, &dirs, mock_snapshot(), String::new()).unwrap();

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

            // Wait for the slow adapter call to complete plus some margin.
            tokio::time::sleep(Duration::from_millis(500)).await;

            actix::System::current().stop();
        });

        let content = std::fs::read_to_string(&conv_path_outer).unwrap();
        let entries: Vec<serde_json::Value> = content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        // There must be a system entry containing "discarded" and "msg-2".
        let discarded = entries.iter().find(|e| {
            e["type"] == "system"
                && e["message"]
                    .as_str()
                    .map(|m| m.contains("discarded") && m.contains("msg-2"))
                    .unwrap_or(false)
        });
        assert!(
            discarded.is_some(),
            "journal missing 'discarded msg-2' system entry; entries: {entries:?}"
        );

        // There must be exactly one Inbound entry (for "first", not "second").
        let inbound_entries: Vec<_> = entries.iter().filter(|e| e["type"] == "inbound").collect();
        assert_eq!(
            inbound_entries.len(),
            1,
            "expected exactly one inbound entry; entries: {entries:?}"
        );
        assert_eq!(
            inbound_entries[0]["payload"], "first",
            "only the first message should be journaled as inbound"
        );
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
        let agent = LeadAgent::new(adapter, &dirs, mock_snapshot(), String::new()).unwrap();

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
            reeve_adapter::MessageContent::Text("second message".to_owned()),
            "second adapter call should carry the second user message"
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
        let agent = LeadAgent::new(adapter, &dirs, mock_snapshot(), String::new()).unwrap();
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
}
