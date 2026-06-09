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
//!    `Ok(())` when the invoking agent's capability profile enables the tool's
//!    category; returns `Err(`[`Refusal`]`)` when the category is disabled.
//!    Ladder 3 wired this in at the same call site without changing the topology.
//!
//! The authority check lives in the tool actor's own handler — there is no
//! intermediary gate actor. Each tool owns its invariant: "execute only if
//! the caller is permitted."
//!
//! Concrete tool implementations live in submodules and are re-exported
//! here so external callers continue to address them as `crate::tool::Foo`.

use std::sync::{Arc, RwLock};

use actix::Recipient;
use reeve_types::IdentityId;
use serde::Serialize;

use crate::blacklist::BlacklistRegistry;
use crate::capability::{CapabilityProfile, ToolCategory};

/// Shared handle to the live blacklist registry.
///
/// The daemon holds one of these and writes a new [`BlacklistRegistry`] into
/// it on every successful reload. Tool actors hold a clone of the same
/// `Arc` and read-lock it on each `InvokeTool` invocation. This makes
/// blacklist updates visible to all running agents without restarting them.
pub type BlacklistHandle = Arc<RwLock<BlacklistRegistry>>;

pub mod list_agents;
pub mod list_personas;
pub mod send_message;
pub mod spawn_agent;
pub mod whoami;
pub mod whois;

pub use list_agents::ListAgentsTool;
pub use list_personas::ListPersonasTool;
pub use send_message::SendMessageTool;
pub use spawn_agent::SpawnAgentTool;
pub use whoami::WhoamiTool;
pub use whois::WhoisTool;

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

// ── Refusal ───────────────────────────────────────────────────────────────────

/// A structured authority refusal returned to the invoking agent.
///
/// Serializes to a JSON object with `"layer"` as a discriminator tag.
/// The model receives this as the content of a `ToolResult { is_error: true }`.
///
/// Each variant covers one enforcement layer in the authority check order:
/// profile (ladder 3), blacklist (ladder 3 phase 2), threshold (ladder 3
/// phases 3+4). The enum is non-exhaustive so future layers can be added.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "layer", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Refusal {
    Profile {
        category: ToolCategory,
        rationale: String,
    },
    Blacklist {
        pattern: String,
        rationale: String,
    },
    Threshold {
        name: String,
        current: String,
        limit: String,
        rationale: String,
    },
}

impl Refusal {
    /// Serialize to a JSON string for inclusion in `ToolResult.content`.
    #[must_use]
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"layer":"profile","rationale":"authority check failed"}"#.to_owned()
        })
    }

    /// The layer name as used in audit log entries.
    #[must_use]
    pub fn layer(&self) -> &'static str {
        match self {
            Self::Profile { .. } => "profile",
            Self::Blacklist { .. } => "blacklist",
            Self::Threshold { .. } => "threshold",
        }
    }

    /// The human-readable rationale.
    #[must_use]
    pub fn rationale(&self) -> &str {
        match self {
            Self::Profile { rationale, .. }
            | Self::Blacklist { rationale, .. }
            | Self::Threshold { rationale, .. } => rationale,
        }
    }
}

// ── Authority ─────────────────────────────────────────────────────────────────

/// Check whether an agent may invoke a tool in the given category.
///
/// Returns `Ok(())` on allow and `Err(Refusal::Profile { .. })` when the
/// category is not enabled in the agent's snapshotted profile.
///
/// When `profile` is `None` (e.g., during a legacy or transitional agent
/// startup where no snapshot exists) the check passes — this preserves
/// backwards compatibility for agents spawned before ladder-3 ships.
/// Newly-spawned agents always have a profile snapshot (hard error on
/// missing persona profile at spawn time).
pub fn check_authority(
    profile: Option<&CapabilityProfile>,
    category: ToolCategory,
    _sender_id: IdentityId,
) -> Result<(), Refusal> {
    let Some(profile) = profile else {
        return Ok(());
    };
    if profile.allows(category) {
        Ok(())
    } else {
        Err(Refusal::Profile {
            category,
            rationale: format!("{category} is not enabled in your capability profile"),
        })
    }
}

/// Check whether `action` matches any blacklist entry.
///
/// Returns `Err(Refusal::Blacklist { .. })` on a hit. When `handle` is `None`
/// (tool constructed without a blacklist, e.g. at daemon startup before the
/// blacklist file is wired up) the check passes silently.
pub fn check_blacklist(handle: Option<&BlacklistHandle>, action: &str) -> Result<(), Refusal> {
    let Some(handle) = handle else {
        return Ok(());
    };
    let registry = handle
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((pattern, rationale)) = registry.check(action) {
        return Err(Refusal::Blacklist {
            pattern: pattern.to_owned(),
            rationale: rationale.to_owned(),
        });
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{CapabilityProfile, Thresholds, ToolCategory};

    fn profile_with(categories: &[ToolCategory]) -> CapabilityProfile {
        CapabilityProfile {
            name: "test".to_owned(),
            version: 1,
            enabled_categories: Some(categories.to_vec()),
            thresholds: Thresholds::default(),
        }
    }

    // T1: check_authority returns Ok when the category is enabled.
    #[test]
    fn check_authority_allows_enabled_category() {
        let id = IdentityId::new().unwrap();
        let profile = profile_with(&[ToolCategory::SpawnAgents, ToolCategory::MessagePeers]);
        assert!(check_authority(Some(&profile), ToolCategory::SpawnAgents, id).is_ok());
        assert!(check_authority(Some(&profile), ToolCategory::MessagePeers, id).is_ok());
    }

    // T2: check_authority returns Err(Profile) when the category is missing.
    #[test]
    fn check_authority_denies_missing_category() {
        let id = IdentityId::new().unwrap();
        let profile = profile_with(&[ToolCategory::ReadFiles]);
        let err = check_authority(Some(&profile), ToolCategory::SpawnAgents, id).unwrap_err();
        match err {
            Refusal::Profile { category, .. } => {
                assert_eq!(category, ToolCategory::SpawnAgents);
            }
            other @ (Refusal::Blacklist { .. } | Refusal::Threshold { .. }) => {
                panic!("expected Profile refusal; got {other:?}")
            }
        }
    }

    // T3: check_authority passes when profile is None (no snapshot yet).
    #[test]
    fn check_authority_allows_when_no_profile() {
        let id = IdentityId::new().unwrap();
        assert!(check_authority(None, ToolCategory::SpawnAgents, id).is_ok());
    }

    // T4: Refusal::Profile serializes with the correct JSON shape.
    #[test]
    fn refusal_profile_json_shape() {
        let r = Refusal::Profile {
            category: ToolCategory::SpawnAgents,
            rationale: "not allowed".to_owned(),
        };
        let json: serde_json::Value = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!(json["layer"], "profile");
        assert_eq!(json["category"], "spawn_agents");
        assert_eq!(json["rationale"], "not allowed");
    }
}
