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
//!
//! Concrete tool implementations live in submodules and are re-exported
//! here so external callers continue to address them as `crate::tool::Foo`.

use actix::Recipient;
use reeve_types::IdentityId;

pub mod send_message;
pub mod spawn_agent;

pub use send_message::SendMessageTool;
pub use spawn_agent::SpawnAgentTool;

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
}
