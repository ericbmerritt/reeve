//! `send_message` tool: requests the [`crate::dispatcher::MessageDispatcher`]
//! to sign and deposit a message into a named recipient's inbox.
//!
//! The relay actor decouples the dispatcher's two-reply pattern
//! (`SendResult` on success, `SendFailed` on error, with a relay-side timeout
//! covering the silent case) from the tool's mailbox.

use std::sync::Arc;

use actix::{ActorContext, AsyncContext, Recipient};

use super::{
    check_authority, check_blacklist, emit_refusal_audit, AuditHandle, BlacklistHandle, InvokeTool,
    ToolResult,
};
use crate::agent_registry::ValidatedAgentName;
use crate::capability::{CapabilityProfile, ToolCategory};
use crate::dispatcher::{SendFailed, SendMessage, SendResult};

/// Deadline for [`SendRelay`]: if the dispatcher does not reply within this
/// window, the relay sends an error result and stops.
const SEND_RELAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// One-shot relay actor: receives either [`SendResult`] or [`SendFailed`] from
/// the dispatcher, converts it to a [`ToolResult`] on the waiting `reply_to`
/// recipient, then stops. Started per-invocation inside [`SendMessageTool`]'s
/// handler.
///
/// `reply_to` is `Option<Recipient<ToolResult>>` so that whichever of the three
/// inbound paths (success, failure, timeout) fires first delivers and the
/// others see `None` and skip — the `.take()` idempotency pattern called out
/// in the project design defaults.
struct SendRelay {
    tool_use_id: String,
    reply_to: Option<Recipient<ToolResult>>,
    timeout: std::time::Duration,
}

impl actix::Actor for SendRelay {
    type Context = actix::Context<Self>;

    fn started(&mut self, ctx: &mut actix::Context<Self>) {
        let tool_use_id = self.tool_use_id.clone();
        ctx.run_later(self.timeout, move |actor, ctx| {
            if let Some(r) = actor.reply_to.take() {
                r.do_send(ToolResult {
                    tool_use_id,
                    content: "send_message: dispatcher did not reply within timeout".to_owned(),
                    is_error: true,
                });
            }
            ctx.stop();
        });
    }
}

impl actix::Handler<SendResult> for SendRelay {
    type Result = ();

    fn handle(&mut self, msg: SendResult, ctx: &mut actix::Context<Self>) {
        if let Some(r) = self.reply_to.take() {
            r.do_send(ToolResult {
                tool_use_id: self.tool_use_id.clone(),
                content: msg.message_id.to_string(),
                is_error: false,
            });
        }
        ctx.stop();
    }
}

impl actix::Handler<SendFailed> for SendRelay {
    type Result = ();

    fn handle(&mut self, msg: SendFailed, ctx: &mut actix::Context<Self>) {
        if let Some(r) = self.reply_to.take() {
            // Use category(), not Display: the Io and KeypairLoad variants
            // embed filesystem paths in their Display output.
            tracing::warn!(
                category = msg.error.category(),
                "send_message: dispatch failed"
            );
            r.do_send(ToolResult {
                tool_use_id: self.tool_use_id.clone(),
                content: format!("send_message: {}", msg.error.category()),
                is_error: true,
            });
        }
        ctx.stop();
    }
}

/// Tool that requests the [`crate::dispatcher::MessageDispatcher`] to sign and
/// deposit a message into a named recipient's inbox, then delivers the outcome
/// back to the calling agent as a [`ToolResult`].
///
/// The descriptor exposes two fields to the model:
/// - `to` (required): recipient agent name as registered in the agent registry.
/// - `body` (required): message body to deliver.
pub struct SendMessageTool {
    dispatcher: Recipient<SendMessage>,
    profile: Option<Arc<CapabilityProfile>>,
    blacklist: Option<BlacklistHandle>,
    audit: Option<AuditHandle>,
}

impl SendMessageTool {
    /// Construct a [`SendMessageTool`] wired to the given dispatcher recipient.
    pub fn new(
        dispatcher: Recipient<SendMessage>,
        profile: Option<Arc<CapabilityProfile>>,
        blacklist: Option<BlacklistHandle>,
    ) -> Self {
        Self {
            dispatcher,
            profile,
            blacklist,
            audit: None,
        }
    }

    pub fn with_audit(mut self, audit: AuditHandle) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Action descriptor for blacklist matching: `SendMessage(to=<name>)`.
    pub fn canonical_action(input: &serde_json::Value) -> Option<String> {
        let to = input.get("to")?.as_str()?;
        Some(format!("SendMessage(to={})", to.trim()))
    }

    /// Adapter-facing tool descriptor for [`SendMessageTool`].
    #[must_use]
    pub fn descriptor() -> reeve_adapter::Tool {
        reeve_adapter::Tool {
            name: "send_message".to_owned(),
            description: "Send a signed message to another agent by name. \
                Returns the dispatched message ID (UUIDv7) on success. \
                Delivery is asynchronous: the recipient's reply, if any, \
                arrives on a later turn as a normal inbound message — there \
                is no built-in await primitive. Use list_agents to discover \
                recipient names. \
                \n\nLimits: body and the resulting signed envelope must each \
                be at most 1 MiB; the dispatcher enforces this pre- and \
                post-serialization. \
                \n\nFailure mode: on error the tool result content is \
                `send_message: <Category>` (is_error=true) where <Category> \
                is one of:\n\
                - RecipientNotFound — the `to` name is not registered\n\
                - SenderNotFound — the calling agent is not in the registry\n\
                - KeyNotFound — the sender has no active key (revoked?)\n\
                - IdentityLookupFailed — identity registry I/O error\n\
                - KeypairLoad — could not load the sender's keypair\n\
                - SigningFailed — envelope signing failed\n\
                - Io — filesystem write to the recipient's inbox failed\n\
                - BodyTooLarge — body or envelope exceeded the 1 MiB cap\n\
                - MessageIdFailed — clock skew prevented minting a UUIDv7\n\
                - SymlinkRejected — recipient inbox path is a symlink\n\
                - AgentRegistryOpen — registry file could not be re-opened\n\
                These categories are stable identifiers; the exact path or \
                detail behind them is logged but not returned to the model."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "to": {
                        "type": "string",
                        "description": "Recipient agent name as registered in the agent registry. \
                            Whitespace is trimmed. Use list_agents to discover live names."
                    },
                    "body": {
                        "type": "string",
                        "description": "Message body to deliver, byte-for-byte (no trim). \
                            Body + envelope overhead must fit in 1 MiB; whitespace-only \
                            bodies are rejected as empty."
                    }
                },
                "required": ["to", "body"]
            }),
        }
    }
}

impl actix::Actor for SendMessageTool {
    type Context = actix::Context<Self>;
}

impl actix::Handler<InvokeTool> for SendMessageTool {
    type Result = ();

    #[expect(
        clippy::too_many_lines,
        reason = "authority checks + validation + dispatch; each branch is short"
    )]
    fn handle(&mut self, msg: InvokeTool, _ctx: &mut actix::Context<Self>) {
        let InvokeTool {
            tool_use_id,
            name: _,
            input,
            sender_id,
            reply_to,
        } = msg;

        let action_str =
            Self::canonical_action(&input).unwrap_or_else(|| "send_message".to_owned());

        if let Err(refusal) = check_authority(
            self.profile.as_deref(),
            ToolCategory::MessagePeers,
            sender_id,
        ) {
            emit_refusal_audit(
                self.audit.as_ref(),
                &refusal,
                sender_id,
                &action_str,
                self.profile.as_deref(),
                None,
            );
            reply_to.do_send(ToolResult {
                tool_use_id,
                content: refusal.to_json(),
                is_error: true,
            });
            return;
        }

        if let Err(refusal) = check_blacklist(self.blacklist.as_ref(), &action_str) {
            let bv = self.blacklist.as_ref().and_then(|h| {
                h.read().map_or_else(
                    |e| Some(e.into_inner().version_hash.clone()),
                    |bl| Some(bl.version_hash.clone()),
                )
            });
            emit_refusal_audit(
                self.audit.as_ref(),
                &refusal,
                sender_id,
                &action_str,
                self.profile.as_deref(),
                bv,
            );
            reply_to.do_send(ToolResult {
                tool_use_id,
                content: refusal.to_json(),
                is_error: true,
            });
            return;
        }

        let Some(to_str) = input.get("to").and_then(|v| v.as_str()).map(str::trim) else {
            reply_to.do_send(ToolResult {
                tool_use_id,
                content: "send_message: missing or non-string `to` argument".to_owned(),
                is_error: true,
            });
            return;
        };

        let to_name = match ValidatedAgentName::new(to_str) {
            Ok(name) => name,
            Err(err) => {
                reply_to.do_send(ToolResult {
                    tool_use_id,
                    content: format!("send_message: invalid `to` ({err})"),
                    is_error: true,
                });
                return;
            }
        };

        let Some(body) = input.get("body").and_then(|v| v.as_str()) else {
            reply_to.do_send(ToolResult {
                tool_use_id,
                content: "send_message: missing or non-string `body` argument".to_owned(),
                is_error: true,
            });
            return;
        };

        // Reject whitespace-only bodies; preserve the original (untrimmed)
        // content on the wire so leading/trailing whitespace in legitimate
        // bodies (e.g., code blocks) is not lost.
        if body.trim().is_empty() {
            reply_to.do_send(ToolResult {
                tool_use_id,
                content: "send_message: body must not be empty".to_owned(),
                is_error: true,
            });
            return;
        }

        let relay_addr = actix::Actor::create(|_ctx| SendRelay {
            tool_use_id: tool_use_id.clone(),
            reply_to: Some(reply_to),
            timeout: SEND_RELAY_TIMEOUT,
        });

        self.dispatcher.do_send(SendMessage {
            from_id: sender_id,
            to_name,
            body: body.to_owned(),
            reply_to: Some(relay_addr.clone().recipient()),
            error_to: Some(relay_addr.recipient()),
        });
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{
        tool_result_capture_pair as capture_pair, TOOL_RESULT_TIMEOUT as RESULT_TIMEOUT,
    };
    use reeve_types::{IdentityId, MessageId};
    use std::sync::Arc;

    use crate::dispatcher::SendError;

    /// dispatcher is wired to this, the relay's `SEND_RELAY_TIMEOUT` would fire.
    struct SendMessageDropper;

    impl actix::Actor for SendMessageDropper {
        type Context = actix::Context<Self>;
    }

    impl actix::Handler<SendMessage> for SendMessageDropper {
        type Result = ();
        fn handle(&mut self, _msg: SendMessage, _ctx: &mut actix::Context<Self>) {}
    }

    fn dispatcher_dropper() -> Recipient<SendMessage> {
        use actix::Actor as _;
        SendMessageDropper.start().recipient()
    }

    /// echoed nowhere — this stub only exercises the success path of the relay.
    struct MockSuccessDispatcher;

    impl actix::Actor for MockSuccessDispatcher {
        type Context = actix::Context<Self>;
    }

    impl actix::Handler<SendMessage> for MockSuccessDispatcher {
        type Result = ();

        fn handle(&mut self, msg: SendMessage, _ctx: &mut actix::Context<Self>) {
            if let Some(reply) = msg.reply_to {
                reply.do_send(SendResult {
                    message_id: MessageId::new().unwrap(),
                });
            }
        }
    }

    /// Stub dispatcher: takes the `error_to` from each [`SendMessage`] and
    /// replies with a [`SendFailed`] carrying
    /// [`SendError::RecipientNotFound`].
    struct MockFailureDispatcher;

    impl actix::Actor for MockFailureDispatcher {
        type Context = actix::Context<Self>;
    }

    impl actix::Handler<SendMessage> for MockFailureDispatcher {
        type Result = ();

        fn handle(&mut self, msg: SendMessage, _ctx: &mut actix::Context<Self>) {
            if let Some(error_to) = msg.error_to {
                error_to.do_send(SendFailed {
                    error: SendError::RecipientNotFound {
                        to_name: msg.to_name.as_str().to_owned(),
                    },
                });
            }
        }
    }

    /// Capturing dispatcher: stores the `body` and `to_name` of every received
    /// [`SendMessage`] into shared slots, then replies [`SendResult`] so the
    /// tool returns success. Used to verify that the tool propagates inputs
    /// verbatim (body) or with documented normalization (`to` trimmed).
    struct CapturingDispatcher {
        captured_body: Arc<std::sync::Mutex<Option<String>>>,
        captured_to: Arc<std::sync::Mutex<Option<String>>>,
    }

    impl actix::Actor for CapturingDispatcher {
        type Context = actix::Context<Self>;
    }

    impl actix::Handler<SendMessage> for CapturingDispatcher {
        type Result = ();

        fn handle(&mut self, msg: SendMessage, _ctx: &mut actix::Context<Self>) {
            *self.captured_body.lock().unwrap() = Some(msg.body);
            *self.captured_to.lock().unwrap() = Some(msg.to_name.as_str().to_owned());
            if let Some(reply) = msg.reply_to {
                reply.do_send(SendResult {
                    message_id: MessageId::new().unwrap(),
                });
            }
        }
    }

    // T_SM1: SendMessageTool descriptor has the expected shape.
    #[test]
    fn send_message_descriptor_shape() {
        let d = SendMessageTool::descriptor();
        assert_eq!(d.name, "send_message");
        assert!(!d.description.is_empty());
        assert_eq!(d.input_schema["type"], "object");
        assert_eq!(d.input_schema["properties"]["to"]["type"], "string");
        assert_eq!(d.input_schema["properties"]["body"]["type"], "string");
        let required = d.input_schema["required"]
            .as_array()
            .expect("required array");
        assert_eq!(required.len(), 2);
        assert!(required.contains(&serde_json::json!("to")));
        assert!(required.contains(&serde_json::json!("body")));
    }

    // T_SM2: missing `to` produces error ToolResult.
    #[test]
    fn send_message_missing_to_returns_error() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool_addr = SendMessageTool::new(dispatcher_dropper(), None, None).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_sm_no_to".to_owned(),
                name: "send_message".to_owned(),
                input: serde_json::json!({ "body": "hi" }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(result.is_error, "expected is_error=true");
            assert!(
                result.content.contains("`to`"),
                "expected error to mention `to`; got: {}",
                result.content
            );

            actix::System::current().stop();
        });
    }

    // T_SM3: non-string `to` produces error ToolResult.
    #[test]
    fn send_message_non_string_to_returns_error() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool_addr = SendMessageTool::new(dispatcher_dropper(), None, None).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_sm_to_int".to_owned(),
                name: "send_message".to_owned(),
                input: serde_json::json!({ "to": 42, "body": "hi" }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(result.is_error, "expected is_error=true");
            assert!(
                result.content.contains("`to`"),
                "expected error to mention `to`; got: {}",
                result.content
            );

            actix::System::current().stop();
        });
    }

    // T_SM4: empty `to` is rejected by ValidatedAgentName.
    #[test]
    fn send_message_empty_to_returns_error() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool_addr = SendMessageTool::new(dispatcher_dropper(), None, None).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_sm_empty_to".to_owned(),
                name: "send_message".to_owned(),
                input: serde_json::json!({ "to": "", "body": "hi" }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(result.is_error, "expected is_error=true");
            assert!(
                result.content.contains("invalid `to`"),
                "expected validation error mentioning `to`; got: {}",
                result.content
            );

            actix::System::current().stop();
        });
    }

    // T_SM5: `to` containing reserved characters is rejected.
    #[test]
    fn send_message_to_with_slash_returns_error() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool_addr = SendMessageTool::new(dispatcher_dropper(), None, None).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_sm_to_slash".to_owned(),
                name: "send_message".to_owned(),
                input: serde_json::json!({ "to": "evil/agent", "body": "hi" }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(result.is_error, "expected is_error=true");
            assert!(
                result.content.contains("invalid `to`"),
                "expected validation error; got: {}",
                result.content
            );

            actix::System::current().stop();
        });
    }

    // T_SM6: missing `body` produces error ToolResult.
    #[test]
    fn send_message_missing_body_returns_error() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool_addr = SendMessageTool::new(dispatcher_dropper(), None, None).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_sm_no_body".to_owned(),
                name: "send_message".to_owned(),
                input: serde_json::json!({ "to": "recipient" }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(result.is_error, "expected is_error=true");
            assert!(
                result.content.contains("`body`"),
                "expected error to mention `body`; got: {}",
                result.content
            );

            actix::System::current().stop();
        });
    }

    // T_SM7: whitespace-only `body` produces error ToolResult.
    #[test]
    fn send_message_whitespace_only_body_returns_error() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool_addr = SendMessageTool::new(dispatcher_dropper(), None, None).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_sm_ws_body".to_owned(),
                name: "send_message".to_owned(),
                input: serde_json::json!({ "to": "recipient", "body": "   \n\t  " }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(result.is_error, "expected is_error=true");
            assert!(
                result.content.contains("body must not be empty"),
                "expected empty-body error; got: {}",
                result.content
            );

            actix::System::current().stop();
        });
    }

    // T_SM8: success path — mock dispatcher replies SendResult; ToolResult
    // carries the message_id and is_error=false.
    #[test]
    fn send_message_success_returns_message_id() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let mock = MockSuccessDispatcher.start();
            let tool_addr = SendMessageTool::new(mock.recipient(), None, None).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_sm_ok".to_owned(),
                name: "send_message".to_owned(),
                input: serde_json::json!({ "to": "recipient", "body": "hello" }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_sm_ok");
            assert!(
                !result.is_error,
                "expected is_error=false; content: {}",
                result.content
            );
            // message_id is a UUID; sanity check the format.
            assert!(
                uuid::Uuid::parse_str(&result.content).is_ok(),
                "expected content to be a UUID; got: {}",
                result.content
            );

            actix::System::current().stop();
        });
    }

    // T_SM9: failure path — mock dispatcher replies SendFailed; ToolResult is
    // is_error=true and carries the SendError::category() string, never a
    // filesystem path.
    #[test]
    fn send_message_failure_surfaces_category() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let mock = MockFailureDispatcher.start();
            let tool_addr = SendMessageTool::new(mock.recipient(), None, None).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_sm_fail".to_owned(),
                name: "send_message".to_owned(),
                input: serde_json::json!({ "to": "ghost", "body": "hi" }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert_eq!(result.tool_use_id, "tu_sm_fail");
            assert!(result.is_error, "expected is_error=true");
            assert!(
                result.content.contains("RecipientNotFound"),
                "expected category in content; got: {}",
                result.content
            );
            // Negative: the "ghost" name appears only in the SendError's
            // Display string, never in the surfaced ToolResult content.
            assert!(
                !result.content.contains("ghost"),
                "tool result must not leak the recipient name from SendError Display; got: {}",
                result.content
            );

            actix::System::current().stop();
        });
    }

    // T_SM10: body is passed to the dispatcher byte-for-byte. The handler doc
    // commits to preserving content; a future regression that adds `.trim()`
    // would be caught here.
    #[test]
    fn send_message_body_dispatched_untrimmed() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let captured_body = Arc::new(std::sync::Mutex::new(None));
            let captured_to = Arc::new(std::sync::Mutex::new(None));
            let mock = CapturingDispatcher {
                captured_body: Arc::clone(&captured_body),
                captured_to: Arc::clone(&captured_to),
            }
            .start();
            let tool_addr = SendMessageTool::new(mock.recipient(), None, None).start();
            let (reply_to, rx) = capture_pair();

            let body_with_whitespace = "  meaningful body with spaces  ";
            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_sm_untrimmed".to_owned(),
                name: "send_message".to_owned(),
                input: serde_json::json!({
                    "to": "recipient",
                    "body": body_with_whitespace
                }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");
            assert!(!result.is_error, "tool returned error: {}", result.content);

            let captured = captured_body
                .lock()
                .unwrap()
                .clone()
                .expect("dispatcher captured no body");
            assert_eq!(
                captured, body_with_whitespace,
                "body must reach the dispatcher byte-for-byte (no trim)"
            );

            actix::System::current().stop();
        });
    }

    // T_SM11: `to` with leading/trailing whitespace is trimmed before
    // ValidatedAgentName, matching SpawnAgentTool's persona-trim convention.
    // If the trim regresses, ValidatedAgentName::new will reject the padded
    // string and the tool will return an error instead of dispatching.
    #[test]
    fn send_message_to_with_whitespace_is_trimmed() {
        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let captured_body = Arc::new(std::sync::Mutex::new(None));
            let captured_to = Arc::new(std::sync::Mutex::new(None));
            let mock = CapturingDispatcher {
                captured_body: Arc::clone(&captured_body),
                captured_to: Arc::clone(&captured_to),
            }
            .start();
            let tool_addr = SendMessageTool::new(mock.recipient(), None, None).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_sm_trim_to".to_owned(),
                name: "send_message".to_owned(),
                input: serde_json::json!({
                    "to": "  recipient\n",
                    "body": "hi"
                }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");
            assert!(
                !result.is_error,
                "padded `to` should be trimmed and accepted; got: {}",
                result.content
            );

            let captured = captured_to
                .lock()
                .unwrap()
                .clone()
                .expect("dispatcher captured no to_name");
            assert_eq!(
                captured, "recipient",
                "to_name reaching dispatcher must be the trimmed form"
            );

            actix::System::current().stop();
        });
    }

    /// Register an agent for the cross-agent integration test: provisions the
    /// full inbox tree via [`crate::agent_fs::AgentDirs::provision`] (the same
    /// helper the daemon uses in production), then enrolls the agent in both
    /// the agent registry and the identity registry with a real Ed25519
    /// keypair. Returns the agent's identity id and inbox root.
    #[cfg(unix)]
    fn register_agent(
        agent_registry: &mut crate::agent_registry::AgentRegistry,
        identity_registry: &crate::identity_registry::IdentityRegistry,
        data_dir: &std::path::Path,
        name: &str,
        operator_id: IdentityId,
    ) -> (IdentityId, std::path::PathBuf) {
        use crate::agent_fs::AgentDirs;
        use crate::agent_registry::{generate_or_load_keypair, AgentRecord, AgentStatus};
        use crate::identity_registry::StoredIdentity;
        use reeve_types::{Identity, KeyRecord};
        use time::OffsetDateTime;

        let dirs = AgentDirs::provision(data_dir, name).unwrap();
        let inbox_dir = dirs.inbox_root();

        let identity_id = IdentityId::new().unwrap();
        let record = AgentRecord {
            name: ValidatedAgentName::new(name).unwrap(),
            identity_id,
            inbox_dir: inbox_dir.clone(),
            persona_name: None,
            spawned_at: OffsetDateTime::now_utc(),
            status: AgentStatus::Running,
        };
        agent_registry.register(record).unwrap();

        let keypair = generate_or_load_keypair(&dirs.identity_key_path()).unwrap();
        let mut identity = Identity::new_agent(name.to_owned(), operator_id).unwrap();
        identity.identity_id = identity_id;
        let key_record = KeyRecord::new(identity_id, *keypair.public()).unwrap();
        let stored = StoredIdentity::new(identity, key_record).unwrap();
        identity_registry.write(&stored).unwrap();

        (identity_id, inbox_dir)
    }
    // T_SM_INTEGRATION: Phase 4 done_when — a message_id returned by the tool
    // appears in the recipient's delivery ledger after the watcher processes
    // the deposited envelope. Exercises the full chain: SendMessageTool →
    // MessageDispatcher → inbox/new/ → Watcher::process_file → DeliveryLedger.
    #[test]
    #[cfg(unix)]
    #[expect(
        clippy::too_many_lines,
        reason = "end-to-end cross-agent flow with sequential setup, async \
                  dispatch, and post-flow ledger assertions; splitting \
                  fragments the narrative"
    )]
    fn send_message_cross_agent_appears_in_recipient_delivery_ledger() {
        use crate::agent_registry::AgentRegistry;
        use crate::audit::AuditLog;
        use crate::dispatcher::MessageDispatcher;
        use crate::identity_registry::IdentityRegistry;
        use crate::inbox::AgentInbox;
        use crate::ledger::{DeliveryKey, DeliveryLedger, ReplayLedger};
        use crate::test_support::secure_dir;
        use crate::watcher::{ProcessOutcome, Watcher};
        use reeve_types::MessageId;
        use std::fs;
        use std::sync::Arc;

        let tmp = secure_dir();
        let data_dir = tmp.path().to_path_buf();

        let identity_registry = Arc::new(IdentityRegistry::open(data_dir.clone()).unwrap());
        let replay = Arc::new(ReplayLedger::open(data_dir.clone()).unwrap());
        let delivery = Arc::new(DeliveryLedger::open(data_dir.clone()).unwrap());
        let audit = Arc::new(AuditLog::open(data_dir.clone()).unwrap());

        let agent_registry_path = data_dir.join("agents").join("registry.toml");
        let mut agent_registry = AgentRegistry::open(agent_registry_path.clone()).unwrap();

        let operator_id = IdentityId::new().unwrap();
        let (sender_id, _sender_inbox_dir) = register_agent(
            &mut agent_registry,
            &identity_registry,
            &data_dir,
            "sender",
            operator_id,
        );
        let (recipient_id, recipient_inbox_dir) = register_agent(
            &mut agent_registry,
            &identity_registry,
            &data_dir,
            "recipient",
            operator_id,
        );

        let watcher = Watcher::new(
            &identity_registry,
            &replay,
            Arc::clone(&delivery),
            audit,
            agent_registry_path.clone(),
        );

        let _ = agent_registry; // fixture-built; dispatcher re-opens from path

        let dispatcher_registry_path = agent_registry_path.clone();
        let message_id_str = actix::System::new().block_on(async move {
            use actix::Actor as _;

            let dispatcher =
                MessageDispatcher::new(dispatcher_registry_path, Arc::clone(&identity_registry));
            let dispatcher_addr = actix::Supervisor::start(move |_| dispatcher);

            let tool_addr = SendMessageTool::new(dispatcher_addr.recipient(), None, None).start();
            let (reply_to, rx) = capture_pair();

            tool_addr.do_send(InvokeTool {
                tool_use_id: "tu_sm_x_agent".to_owned(),
                name: "send_message".to_owned(),
                input: serde_json::json!({
                    "to": "recipient",
                    "body": "hello from the integration test"
                }),
                sender_id,
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(!result.is_error, "tool returned error: {}", result.content);

            let captured = result.content.clone();
            actix::System::current().stop();
            captured
        });

        let parsed_uuid =
            uuid::Uuid::parse_str(&message_id_str).expect("tool result content is a UUID string");
        let message_id = MessageId::try_from(parsed_uuid).expect("UUIDv7");

        let new_dir = recipient_inbox_dir.join("new");
        let entries: Vec<_> = fs::read_dir(&new_dir).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1, "expected one envelope in recipient new/");
        assert_eq!(
            entries[0].file_name().to_string_lossy(),
            message_id_str,
            "envelope filename must match the tool-returned message_id"
        );

        let inbox = AgentInbox::from_path(recipient_inbox_dir);
        let outcome = watcher
            .process_file(&entries[0].path(), recipient_id, &inbox, |id| {
                id == recipient_id
            })
            .expect("process_file returned an error");

        match outcome {
            ProcessOutcome::Delivered {
                message_id: delivered_id,
                ..
            } => assert_eq!(
                delivered_id, message_id,
                "watcher delivered a different message_id than the tool returned"
            ),
            ProcessOutcome::Quarantined { reason } => {
                panic!("expected Delivered, watcher quarantined: {reason:?}")
            }
            ProcessOutcome::AlreadyDelivered { .. }
            | ProcessOutcome::AlreadyProcessed
            | ProcessOutcome::InvalidFilename { .. } => {
                panic!("expected Delivered, got {outcome:?}")
            }
        }

        let key = DeliveryKey {
            recipient_id,
            message_id,
        };
        assert!(
            delivery.contains(&key).unwrap(),
            "tool-returned message_id is not in recipient's delivery ledger"
        );
    }
}
