//! Tool actor protocol.
//!
//! Tools are actix actors that receive [`InvokeTool`] messages and reply with
//! [`ToolResult`] messages on a [`actix::Recipient`] channel supplied by the
//! caller. The two-message pattern (rather than a synchronous request/reply
//! return type) lets an agent dispatch parallel tool calls and process results
//! as they arrive without blocking its mailbox.
//!
//! Each concrete tool actor:
//!
//! 1. Exposes a `descriptor() -> reeve_adapter::Tool` function so the agent
//!    can advertise the tool to the model adapter.
//! 2. Implements `Handler<InvokeTool>` and checks [`check_authority`] for the
//!    invoking identity before executing. The check always returns
//!    [`AuthorityDecision::Allow`] in the multi-agent ladder; ladder 3 fills
//!    in capability-profile enforcement at the same call site without
//!    changing the topology.
//!
//! The authority check lives in the tool actor's own handler — there is no
//! intermediary gate actor. Each tool owns its invariant: "execute only if
//! the caller is permitted."

use actix::{ActorContext, AsyncContext, Recipient};
use reeve_types::IdentityId;

// ── Messages ──────────────────────────────────────────────────────────────────

/// Request a tool actor to execute one tool invocation.
///
/// Carries the `tool_use_id` from the model's response so the matching
/// [`ToolResult`] can be paired back to the originating call. `sender_id` is
/// the authority-check token — the tool's handler resolves the invoker's
/// permission against the tool's capability requirements before executing.
pub struct InvokeTool {
    /// Provider-assigned identifier for the tool call, echoed back in
    /// [`ToolResult::tool_use_id`].
    pub tool_use_id: String,
    /// Tool name the model invoked. The receiving actor may already know its
    /// own name; the field is included for diagnostics and so a generic
    /// dispatcher (if introduced later) can route by name.
    pub name: String,
    /// Arguments the model supplied, conforming to the tool's input schema.
    pub input: serde_json::Value,
    /// Identity of the invoking agent. The handler passes this to
    /// [`check_authority`] before executing.
    pub sender_id: IdentityId,
    /// Channel on which to deliver the matching [`ToolResult`].
    pub reply_to: Recipient<ToolResult>,
}

impl actix::Message for InvokeTool {
    type Result = ();
}

/// The completion of a tool invocation.
///
/// `tool_use_id` matches the request that produced it. Tool results pair to
/// their requests by ID, not by arrival order.
pub struct ToolResult {
    /// Identifier from the [`InvokeTool`] this result answers.
    pub tool_use_id: String,
    /// Tool output as a string. Structured outputs are JSON-encoded upstream;
    /// this field has no schema.
    pub content: String,
    /// `true` if the tool execution failed; the agent forwards this verbatim
    /// to the adapter as the `is_error` field on the matching tool-result
    /// content block.
    pub is_error: bool,
}

impl actix::Message for ToolResult {
    type Result = ();
}

// ── Authority ─────────────────────────────────────────────────────────────────

/// Outcome of an authority check on an incoming [`InvokeTool`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorityDecision {
    /// The invocation is permitted; the tool should execute.
    Allow,
    /// The invocation is refused; the tool must reply with an error result
    /// carrying `reason` and not execute its operation.
    Deny {
        /// Human-readable reason; included verbatim in the `ToolResult`
        /// content the tool returns to the invoking agent.
        reason: String,
    },
}

/// Decide whether an agent identified by `sender_id` may invoke `tool_name`.
///
/// In the multi-agent ladder this always returns [`AuthorityDecision::Allow`].
/// Ladder 3 (`reeve-authority`) replaces the body with a capability-profile
/// lookup; the call sites in tool handlers stay unchanged.
#[must_use]
pub fn check_authority(_sender_id: IdentityId, _tool_name: &str) -> AuthorityDecision {
    AuthorityDecision::Allow
}

// ── SpawnAgentTool ────────────────────────────────────────────────────────────

use crate::spawn_coordinator::{SpawnRequest, SpawnResponse};

/// Production deadline for [`SpawnRelay`]: if the coordinator does not reply
/// within this window, the relay sends an error result and stops.
const SPAWN_RELAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Maximum combined byte length of `task` + `context` fields.
const MAX_SPAWN_CONTEXT_BYTES: usize = 65_536;

/// Maximum byte length of the `persona` field.
const MAX_PERSONA_BYTES: usize = 256;

/// One-shot relay actor: receives a single [`SpawnResponse`], converts it to a
/// [`ToolResult`] on the waiting `reply_to` recipient, then stops itself.
///
/// Started per-invocation inside [`SpawnAgentTool`]'s handler. The relay
/// decouples the coordinator's async reply from the tool actor's mailbox so
/// concurrent tool calls do not interfere with each other.
///
/// If no `SpawnResponse` arrives within `timeout`, the relay delivers an error
/// result and stops rather than leaking indefinitely.
struct SpawnRelay {
    tool_use_id: String,
    reply_to: Option<Recipient<ToolResult>>,
    timeout: std::time::Duration,
}

impl actix::Actor for SpawnRelay {
    type Context = actix::Context<Self>;

    fn started(&mut self, ctx: &mut actix::Context<Self>) {
        let tool_use_id = self.tool_use_id.clone();
        ctx.run_later(self.timeout, move |actor, ctx| {
            if let Some(r) = actor.reply_to.take() {
                let error_content =
                    "spawn_agent: coordinator did not reply within timeout".to_owned();
                r.do_send(ToolResult {
                    tool_use_id,
                    content: error_content,
                    is_error: true,
                });
            }
            ctx.stop();
        });
    }
}

impl actix::Handler<SpawnResponse> for SpawnRelay {
    type Result = ();

    fn handle(&mut self, msg: SpawnResponse, ctx: &mut actix::Context<Self>) {
        if let Some(r) = self.reply_to.take() {
            let (content, is_error) = match msg {
                SpawnResponse::Success { agent_name, .. } => (agent_name, false),
                SpawnResponse::Failure { message } => {
                    tracing::warn!("spawn_agent: coordinator failure: {message}");
                    (
                        "spawn_agent: coordinator failed to provision agent".to_owned(),
                        true,
                    )
                }
            };
            r.do_send(ToolResult {
                tool_use_id: self.tool_use_id.clone(),
                content,
                is_error,
            });
        }
        ctx.stop();
    }
}

/// Tool that requests the [`SpawnCoordinator`] to provision and start a new
/// subordinate agent, then delivers the outcome back to the calling agent as a
/// [`ToolResult`].
///
/// The descriptor exposes three fields to the model:
/// - `persona` (required): persona name to load for the new agent.
/// - `task` (required): initial task instruction sent as the system prompt.
/// - `context` (optional): additional context appended to the system prompt.
pub struct SpawnAgentTool {
    coordinator: Recipient<SpawnRequest>,
}

impl SpawnAgentTool {
    /// Construct a [`SpawnAgentTool`] wired to the given coordinator recipient.
    pub fn new(coordinator: Recipient<SpawnRequest>) -> Self {
        Self { coordinator }
    }

    /// Adapter-facing tool descriptor for [`SpawnAgentTool`].
    #[must_use]
    pub fn descriptor() -> reeve_adapter::Tool {
        reeve_adapter::Tool {
            name: "spawn_agent".to_owned(),
            description:
                "Provision and start a new subordinate agent with a given persona and task. \
                 Returns the agent's assigned name on success."
                    .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "persona": {
                        "type": "string",
                        "description": "Name of the persona config to load for the new agent."
                    },
                    "task": {
                        "type": "string",
                        "description": "Initial task instruction; becomes the agent's system prompt."
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional additional context appended to the system prompt."
                    }
                },
                "required": ["persona", "task"]
            }),
        }
    }
}

impl actix::Actor for SpawnAgentTool {
    type Context = actix::Context<Self>;
}

impl actix::Handler<InvokeTool> for SpawnAgentTool {
    type Result = ();

    #[expect(
        clippy::too_many_lines,
        reason = "sequential guard chain: each early return is a distinct invariant; \
                  splitting would obscure the order in which guards execute"
    )]
    fn handle(&mut self, msg: InvokeTool, _ctx: &mut actix::Context<Self>) {
        let InvokeTool {
            tool_use_id,
            name,
            input,
            sender_id,
            reply_to,
        } = msg;

        if let AuthorityDecision::Deny { reason } = check_authority(sender_id, &name) {
            reply_to.do_send(ToolResult {
                tool_use_id,
                content: format!("denied: {reason}"),
                is_error: true,
            });
            return;
        }

        let Some(persona) = input.get("persona").and_then(|v| v.as_str()) else {
            reply_to.do_send(ToolResult {
                tool_use_id,
                content: "spawn_agent: missing or non-string `persona` argument".to_owned(),
                is_error: true,
            });
            return;
        };
        let persona = persona.trim();

        if persona.is_empty() {
            reply_to.do_send(ToolResult {
                tool_use_id,
                content: "spawn_agent: persona must not be empty".to_owned(),
                is_error: true,
            });
            return;
        }

        if persona.len() > MAX_PERSONA_BYTES {
            reply_to.do_send(ToolResult {
                tool_use_id,
                content: format!("spawn_agent: persona exceeds {MAX_PERSONA_BYTES}-byte limit"),
                is_error: true,
            });
            return;
        }

        let Some(task) = input.get("task").and_then(|v| v.as_str()).map(str::trim) else {
            reply_to.do_send(ToolResult {
                tool_use_id,
                content: "spawn_agent: missing or non-string `task` argument".to_owned(),
                is_error: true,
            });
            return;
        };

        if task.is_empty() {
            reply_to.do_send(ToolResult {
                tool_use_id,
                content: "spawn_agent: task must not be empty".to_owned(),
                is_error: true,
            });
            return;
        }

        let context = input
            .get("context")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");

        let system_prompt = if context.is_empty() {
            task.to_owned()
        } else {
            format!("{task}\n\n{context}")
        };

        // Caps caller-controlled bytes; the persona system prompt is appended by
        // the coordinator from admin-controlled config and does not count here.
        if system_prompt.len() > MAX_SPAWN_CONTEXT_BYTES {
            reply_to.do_send(ToolResult {
                tool_use_id,
                content: format!(
                    "spawn_agent: task+context exceeds {MAX_SPAWN_CONTEXT_BYTES}-byte limit"
                ),
                is_error: true,
            });
            return;
        }

        // Validate all fields before creating the relay actor. The relay starts
        // a 30-second timeout timer in its started() hook; creating it before
        // validation would leak the timer on validation failure.
        let params = match SpawnRequest::validate(persona, &system_prompt, sender_id) {
            Ok(p) => p,
            Err(err) => {
                reply_to.do_send(ToolResult {
                    tool_use_id,
                    content: format!("spawn_agent: {err}"),
                    is_error: true,
                });
                return;
            }
        };

        let relay_addr = actix::Actor::create(|_ctx| SpawnRelay {
            tool_use_id: tool_use_id.clone(),
            reply_to: Some(reply_to),
            timeout: SPAWN_RELAY_TIMEOUT,
        });

        let req = SpawnRequest::new(params, relay_addr.recipient());
        self.coordinator.do_send(req);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // T1: check_authority returns Allow for any (sender, tool) pair.
    #[test]
    fn check_authority_always_allows() {
        let id = IdentityId::new().unwrap();
        assert_eq!(check_authority(id, "any_tool"), AuthorityDecision::Allow);
        assert_eq!(check_authority(id, ""), AuthorityDecision::Allow);
    }

    // T2: AuthorityDecision::Deny carries its reason.
    #[test]
    fn authority_decision_deny_carries_reason() {
        let d = AuthorityDecision::Deny {
            reason: "no capability".to_owned(),
        };
        match d {
            AuthorityDecision::Deny { reason } => assert_eq!(reason, "no capability"),
            AuthorityDecision::Allow => panic!("expected Deny"),
        }
    }

    // T_SA1: SpawnAgentTool descriptor has the expected shape.
    #[test]
    fn spawn_agent_descriptor_shape() {
        let d = SpawnAgentTool::descriptor();
        assert_eq!(d.name, "spawn_agent");
        assert!(!d.description.is_empty());
        assert_eq!(d.input_schema["type"], "object");
        assert_eq!(d.input_schema["properties"]["persona"]["type"], "string");
        assert_eq!(d.input_schema["properties"]["task"]["type"], "string");
        assert_eq!(d.input_schema["properties"]["context"]["type"], "string");
        let required = d.input_schema["required"]
            .as_array()
            .expect("required array");
        assert_eq!(required.len(), 2);
        assert!(required.contains(&serde_json::json!("persona")));
        assert!(required.contains(&serde_json::json!("task")));
        // context must NOT appear in required
        assert!(!required.contains(&serde_json::json!("context")));
    }

    use crate::test_support::ToolResultCapture;

    /// No-op coordinator stub; safe only for tests where the tool errors before
    /// reaching the coordinator — if it does reach it, the relay blocks for
    /// `SPAWN_RELAY_TIMEOUT`.
    struct SpawnRequestDropper;

    impl actix::Actor for SpawnRequestDropper {
        type Context = actix::Context<Self>;
    }

    impl actix::Handler<SpawnRequest> for SpawnRequestDropper {
        type Result = ();
        fn handle(&mut self, _msg: SpawnRequest, _ctx: &mut actix::Context<Self>) {}
    }

    fn dropper_recipient() -> Recipient<SpawnRequest> {
        use actix::Actor as _;
        SpawnRequestDropper.start().recipient()
    }

    fn capture_pair() -> (
        Recipient<ToolResult>,
        tokio::sync::oneshot::Receiver<ToolResult>,
    ) {
        use actix::Actor as _;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let addr = ToolResultCapture { tx: Some(tx) }.start();
        (addr.recipient(), rx)
    }

    const RESULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    // T_SA3: Missing persona produces error ToolResult with is_error=true.
    #[test]
    fn spawn_agent_tool_missing_persona_returns_error() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let coord = dropper_recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_test".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({ "task": "do something" }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_test");
            assert!(result.is_error, "expected is_error=true");
            assert!(
                result.content.contains("persona"),
                "error content should mention 'persona': {}",
                result.content
            );

            actix::System::current().stop();
        });
    }

    // T_SA3b: Empty persona string returns is_error=true before relay is started.
    #[test]
    fn spawn_agent_tool_empty_persona_string_returns_error() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let coord = dropper_recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_empty_persona".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({ "persona": "", "task": "do something" }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_empty_persona");
            assert!(result.is_error, "expected is_error=true for empty persona");
            assert!(
                result.content.contains("persona"),
                "error content should mention 'persona': {}",
                result.content
            );

            actix::System::current().stop();
        });
    }

    // T_SA4: Missing task produces error ToolResult with is_error=true.
    #[test]
    fn spawn_agent_tool_missing_task_returns_error() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let coord = dropper_recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_test2".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({ "persona": "test-persona" }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_test2");
            assert!(result.is_error, "expected is_error=true");
            assert!(
                result.content.contains("task"),
                "error content should mention 'task': {}",
                result.content
            );

            actix::System::current().stop();
        });
    }

    // T_SA5: SpawnRequest::validate failure returns is_error=true without relay.
    #[test]
    fn spawn_agent_tool_spawn_request_new_failure_returns_error() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let coord = dropper_recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            // control characters are rejected by ValidatedAgentName
            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_test_fail".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({
                    "persona": "\x01invalid",
                    "task": "do something"
                }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_test_fail");
            assert!(
                result.is_error,
                "expected is_error=true for invalid persona name"
            );

            actix::System::current().stop();
        });
    }

    // T_SA6: SpawnResponse::Success relay path — coordinator replies Success;
    // relay delivers non-error ToolResult with agent_name as content.
    #[test]
    fn spawn_agent_tool_success_returns_agent_name_as_content() {
        actix::System::new().block_on(async move {
            use crate::test_support::MockSpawnCoordinator;
            use actix::Actor as _;
            let coord = MockSpawnCoordinator.start().recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_success".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({
                    "persona": "test-persona",
                    "task": "do something"
                }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_success");
            assert!(!result.is_error, "expected is_error=false on success");
            assert_eq!(result.content, "mock-agent");

            actix::System::current().stop();
        });
    }

    // T_SA7: Non-empty context is appended to the system prompt. The coordinator
    // receives a SpawnRequest whose system_prompt contains both task and context
    // in the correct order.
    #[test]
    fn spawn_agent_tool_context_appended_to_system_prompt() {
        use crate::test_support::CapturingSpawnCoordinator;
        use std::sync::{Arc, Mutex};

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let coord = CapturingSpawnCoordinator {
                last_system_prompt: Arc::clone(&captured),
            }
            .start()
            .recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_ctx".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({
                    "persona": "test-persona",
                    "task": "do something",
                    "context": "some additional context"
                }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_ctx");
            assert!(
                !result.is_error,
                "expected is_error=false when context is supplied"
            );
            assert_eq!(result.content, "mock-agent");

            let prompt = captured
                .lock()
                .unwrap()
                .clone()
                .expect("system_prompt not captured");
            assert!(
                prompt.contains("do something"),
                "system_prompt must contain task: {prompt}"
            );
            assert!(
                prompt.contains("some additional context"),
                "system_prompt must contain context: {prompt}"
            );
            let task_pos = prompt.find("do something").unwrap();
            let ctx_pos = prompt.find("some additional context").unwrap();
            assert!(
                task_pos < ctx_pos,
                "task must precede context in system_prompt"
            );

            actix::System::current().stop();
        });
    }

    // T_SA8: SpawnRelay sends is_error=true if the coordinator never replies.
    //
    // Uses a short relay timeout so the test completes in milliseconds, not 30s.
    #[test]
    fn spawn_agent_tool_relay_fires_timeout_when_coordinator_silent() {
        const RELAY_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);
        const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let (capture_tx, capture_rx) = tokio::sync::oneshot::channel::<ToolResult>();
            let capture_addr = ToolResultCapture {
                tx: Some(capture_tx),
            }
            .start();
            let reply_to: Recipient<ToolResult> = capture_addr.recipient();

            // Relay with a short deadline; coordinator never replies.
            let relay = SpawnRelay {
                tool_use_id: "tu_timeout".to_owned(),
                reply_to: Some(reply_to),
                timeout: RELAY_TEST_TIMEOUT,
            };
            let relay_addr = relay.start();

            let result = tokio::time::timeout(WAIT_TIMEOUT, capture_rx)
                .await
                .expect("ToolResult did not arrive within wait window")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_timeout");
            assert!(result.is_error, "expected is_error=true on timeout");
            assert!(
                result.content.contains("timeout"),
                "error message should mention timeout: {}",
                result.content
            );

            drop(relay_addr);
            actix::System::current().stop();
        });
    }

    // T_SA8b: SpawnRelay::Failure branch — coordinator replies Failure; relay
    // delivers is_error=true with a generic scrubbed message, not the internal
    // detail.
    #[test]
    fn spawn_agent_tool_relay_sends_generic_error_on_coordinator_failure() {
        const RELAY_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);
        const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let (capture_tx, capture_rx) = tokio::sync::oneshot::channel::<ToolResult>();
            let capture_addr = ToolResultCapture {
                tx: Some(capture_tx),
            }
            .start();
            let reply_to: Recipient<ToolResult> = capture_addr.recipient();

            let relay = SpawnRelay {
                tool_use_id: "tu_failure".to_owned(),
                reply_to: Some(reply_to),
                timeout: RELAY_TEST_TIMEOUT,
            };
            let relay_addr = relay.start();

            // Send a Failure response with an internal detail that must not
            // appear in the scrubbed ToolResult content.
            relay_addr.do_send(SpawnResponse::Failure {
                message: "internal error detail".to_owned(),
            });

            let result = tokio::time::timeout(WAIT_TIMEOUT, capture_rx)
                .await
                .expect("ToolResult did not arrive within wait window")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_failure");
            assert!(
                result.is_error,
                "expected is_error=true on coordinator failure"
            );
            assert_eq!(
                result.content, "spawn_agent: coordinator failed to provision agent",
                "content must be the generic scrubbed message"
            );
            assert!(
                !result.content.contains("internal error detail"),
                "internal detail must not appear in scrubbed content: {}",
                result.content
            );

            actix::System::current().stop();
        });
    }

    // T_SA9: task+context exceeding 65536 bytes returns is_error=true.
    #[test]
    fn spawn_agent_tool_rejects_oversized_context() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let coord = dropper_recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            let big = "x".repeat(MAX_SPAWN_CONTEXT_BYTES + 1);
            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_oversize".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({
                    "persona": "test-persona",
                    "task": big
                }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_oversize");
            assert!(
                result.is_error,
                "expected is_error=true for oversized input"
            );
            assert!(
                result.content.contains("65536"),
                "error should mention byte limit: {}",
                result.content
            );

            actix::System::current().stop();
        });
    }

    // T_SA10: Empty task string returns is_error=true before relay is started.
    #[test]
    fn spawn_agent_tool_empty_task_string_returns_error() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let coord = dropper_recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_empty_task".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({
                    "persona": "test-persona",
                    "task": ""
                }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_empty_task");
            assert!(result.is_error, "expected is_error=true for empty task");

            actix::System::current().stop();
        });
    }

    // T_SA14: Whitespace-only task trims to empty and returns is_error=true.
    #[test]
    fn spawn_agent_tool_whitespace_only_task_string_returns_error() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let coord = dropper_recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_ws_task".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({
                    "persona": "test-persona",
                    "task": "   "
                }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_ws_task");
            assert!(
                result.is_error,
                "expected is_error=true for whitespace-only task"
            );

            actix::System::current().stop();
        });
    }

    // T_SA15: Whitespace-only context trims to empty and is treated as absent —
    // the system_prompt equals the task with no "\n\n" separator.
    #[test]
    fn spawn_agent_tool_whitespace_only_context_treated_as_absent() {
        use crate::test_support::CapturingSpawnCoordinator;
        use std::sync::{Arc, Mutex};

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let coord = CapturingSpawnCoordinator {
                last_system_prompt: Arc::clone(&captured),
            }
            .start()
            .recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_ws_ctx".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({
                    "persona": "test-persona",
                    "task": "do something",
                    "context": "   "
                }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_ws_ctx");
            assert!(!result.is_error, "whitespace-only context must not error");

            let prompt = captured
                .lock()
                .unwrap()
                .clone()
                .expect("system_prompt not captured");
            assert_eq!(
                prompt, "do something",
                "whitespace-only context must produce no separator; got: {prompt}"
            );

            actix::System::current().stop();
        });
    }

    // T_SA11: task+context exactly at MAX_SPAWN_CONTEXT_BYTES is accepted.
    #[test]
    fn spawn_agent_tool_accepts_exact_boundary_context() {
        actix::System::new().block_on(async move {
            use crate::test_support::MockSpawnCoordinator;
            use actix::Actor as _;
            let coord = MockSpawnCoordinator.start().recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            let exact = "x".repeat(MAX_SPAWN_CONTEXT_BYTES);
            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_exact".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({
                    "persona": "test-persona",
                    "task": exact
                }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_exact");
            assert!(
                !result.is_error,
                "expected is_error=false at exact boundary: {}",
                result.content
            );
            assert_eq!(result.content, "mock-agent");

            actix::System::current().stop();
        });
    }

    // T_SA12: task+context at separator boundary. The composed system_prompt
    // includes "\n\n" between task and context; the size check must account for
    // those 2 bytes.
    //
    // Rejected case: task=32768b + "\n\n" + context=32767b = 65537b > 65536b limit.
    // Accepted case: task=32767b + "\n\n" + context=32767b = 65536b == limit.
    #[test]
    fn spawn_agent_tool_task_plus_context_at_boundary_includes_separator() {
        // Rejected: task(32768) + "\n\n"(2) + context(32767) = 65537 bytes.
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let coord = dropper_recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            let task = "x".repeat(32768);
            let context = "y".repeat(32767);
            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_sep_reject".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({
                    "persona": "test-persona",
                    "task": task,
                    "context": context
                }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_sep_reject");
            assert!(
                result.is_error,
                "expected is_error=true: 32768+2+32767=65537 exceeds limit"
            );

            actix::System::current().stop();
        });

        // Accepted: task(32767) + "\n\n"(2) + context(32767) = 65536 bytes == limit.
        actix::System::new().block_on(async move {
            use crate::test_support::MockSpawnCoordinator;
            use actix::Actor as _;
            let coord = MockSpawnCoordinator.start().recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            let task = "x".repeat(32767);
            let context = "y".repeat(32767);
            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_sep_accept".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({
                    "persona": "test-persona",
                    "task": task,
                    "context": context
                }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_sep_accept");
            assert!(
                !result.is_error,
                "expected is_error=false: 32767+2+32767=65536 is at limit: {}",
                result.content
            );
            assert_eq!(result.content, "mock-agent");

            actix::System::current().stop();
        });
    }

    // T_SA13: persona with leading/trailing whitespace is trimmed before
    // validation; a valid persona name surrounded by spaces succeeds.
    #[test]
    fn spawn_agent_tool_persona_with_leading_trailing_spaces_succeeds() {
        actix::System::new().block_on(async move {
            use crate::test_support::MockSpawnCoordinator;
            use actix::Actor as _;
            let coord = MockSpawnCoordinator.start().recipient();
            let tool_addr = SpawnAgentTool::new(coord).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_trim_persona".to_owned(),
                name: "spawn_agent".to_owned(),
                input: serde_json::json!({
                    "persona": "  test-persona  ",
                    "task": "do something"
                }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_trim_persona");
            assert!(
                !result.is_error,
                "expected is_error=false for trimmed persona"
            );
            assert_eq!(result.content, "mock-agent");

            actix::System::current().stop();
        });
    }
}
