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

use actix::Recipient;
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

// ── EchoTool ──────────────────────────────────────────────────────────────────

/// Round-trip tool whose output is its input verbatim.
///
/// Used to exercise the agent's tool execution loop end-to-end without taking
/// a dependency on a real tool implementation. The descriptor declares a
/// single required string argument named `text`; the handler returns that
/// argument as the result content.
///
/// Removed when the multi-agent ladder's `spawn_agent` and `send_message`
/// tools land — `EchoTool` exists only to validate the loop machinery.
pub struct EchoTool;

impl EchoTool {
    /// Adapter-facing tool descriptor for [`EchoTool`].
    #[must_use]
    pub fn descriptor() -> reeve_adapter::Tool {
        reeve_adapter::Tool {
            name: "echo".to_owned(),
            description: "Return the input string unchanged. Used to verify the tool loop."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Text to echo back verbatim."
                    }
                },
                "required": ["text"]
            }),
        }
    }
}

impl actix::Actor for EchoTool {
    type Context = actix::Context<Self>;
}

impl actix::Handler<InvokeTool> for EchoTool {
    type Result = ();

    fn handle(&mut self, msg: InvokeTool, _ctx: &mut actix::Context<Self>) {
        // Authority check first — Allow today, real check in ladder 3.
        let (content, is_error) = match check_authority(msg.sender_id, &msg.name) {
            AuthorityDecision::Allow => match msg.input.get("text").and_then(|v| v.as_str()) {
                Some(text) => (text.to_owned(), false),
                None => (
                    "echo: missing or non-string `text` argument".to_owned(),
                    true,
                ),
            },
            AuthorityDecision::Deny { reason } => (format!("denied: {reason}"), true),
        };
        msg.reply_to.do_send(ToolResult {
            tool_use_id: msg.tool_use_id,
            content,
            is_error,
        });
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

    // T3: EchoTool descriptor declares the schema the agent advertises.
    #[test]
    fn echo_tool_descriptor_shape() {
        let d = EchoTool::descriptor();
        assert_eq!(d.name, "echo");
        assert!(!d.description.is_empty());
        assert_eq!(d.input_schema["type"], "object");
        assert_eq!(d.input_schema["properties"]["text"]["type"], "string");
        let required = d.input_schema["required"].as_array().expect("required");
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "text");
    }
}
