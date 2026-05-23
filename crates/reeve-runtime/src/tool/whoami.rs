//! `whoami` tool: report the calling agent's own identity.
//!
//! The runtime sets [`InvokeTool::sender_id`] from the agent actor's own
//! identity state (not from model input), so the result is authoritative —
//! a model can't spoof its way into a different identity by lying to the
//! tool. Used by agents that need to self-identify when constructing
//! envelopes by hand, or that want to confirm their place in the registry.

use std::path::PathBuf;

use actix::{Actor, Context, Handler};

use super::{check_authority, AuthorityDecision, InvokeTool, ToolResult};
use crate::agent_registry::AgentRegistry;

/// Tool that returns the invoking agent's identity. No arguments.
pub struct WhoamiTool {
    agent_registry_path: PathBuf,
}

impl WhoamiTool {
    /// Construct a [`WhoamiTool`] reading from the given registry file for
    /// name resolution.
    pub fn new(agent_registry_path: PathBuf) -> Self {
        Self {
            agent_registry_path,
        }
    }

    /// Adapter-facing tool descriptor for [`WhoamiTool`].
    #[must_use]
    pub fn descriptor() -> reeve_adapter::Tool {
        reeve_adapter::Tool {
            name: "whoami".to_owned(),
            description: "Return the calling agent's own identity. Returns a \
                JSON object: `{identity_id, name}` where identity_id is the \
                stable UUIDv7 the runtime knows the agent by, and name is \
                the registered agent name (e.g. \"lead\", \
                \"worker-abc12345\") or null if the agent is not in the \
                registry. \
                \n\nNo arguments. \
                \n\nThe identity_id is set by the runtime from the agent \
                actor's own state — model input cannot spoof it. \
                \n\nFailure mode: `whoami: AgentRegistryOpen` (is_error=true) \
                when the registry is unreadable; the identity_id is still \
                in the model's reach as the sender of every send_message call."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }
}

impl Actor for WhoamiTool {
    type Context = Context<Self>;
}

impl Handler<InvokeTool> for WhoamiTool {
    type Result = ();

    fn handle(&mut self, msg: InvokeTool, _ctx: &mut Context<Self>) {
        let InvokeTool {
            tool_use_id,
            name,
            input: _,
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

        // Best-effort name resolution. If the registry is unreachable, return
        // the identity_id alone (still useful) and mark is_error so the agent
        // sees an explicit signal.
        let resolved_name = match AgentRegistry::open(self.agent_registry_path.clone()) {
            Ok(registry) => registry
                .list()
                .find(|r| r.identity_id == sender_id)
                .map(|r| r.name.as_str().to_owned()),
            Err(err) => {
                tracing::warn!(
                    err = %err,
                    "whoami: failed to open agent registry"
                );
                reply_to.do_send(ToolResult {
                    tool_use_id,
                    content: "whoami: AgentRegistryOpen".to_owned(),
                    is_error: true,
                });
                return;
            }
        };

        let content = serde_json::json!({
            "identity_id": sender_id.to_string(),
            "name": resolved_name,
        });
        reply_to.do_send(ToolResult {
            tool_use_id,
            content: content.to_string(),
            is_error: false,
        });
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_registry::{AgentRecord, AgentStatus, ValidatedAgentName};
    use crate::test_support::{
        secure_dir, tool_result_capture_pair as capture_pair, TOOL_RESULT_TIMEOUT as RESULT_TIMEOUT,
    };
    use reeve_types::IdentityId;
    use time::OffsetDateTime;

    // T_WA1: descriptor shape.
    #[test]
    fn whoami_descriptor_shape() {
        let d = WhoamiTool::descriptor();
        assert_eq!(d.name, "whoami");
        assert!(!d.description.is_empty());
        assert_eq!(d.input_schema["type"], "object");
        let required = d.input_schema["required"]
            .as_array()
            .expect("required array");
        assert!(required.is_empty());
    }

    // T_WA2: resolves the agent's name from the registry when the sender_id
    // matches a registered record.
    #[test]
    fn whoami_resolves_name_for_registered_sender() {
        let tmp = secure_dir();
        let path = tmp.path().join("registry.toml");
        let mut registry = AgentRegistry::open(path.clone()).unwrap();
        let sender_id = IdentityId::new().unwrap();
        registry
            .register(AgentRecord {
                name: ValidatedAgentName::new("worker-abc12345").unwrap(),
                identity_id: sender_id,
                inbox_dir: tmp.path().join("inbox"),
                persona_name: Some("worker".to_owned()),
                spawned_at: OffsetDateTime::now_utc(),
                status: AgentStatus::Running,
            })
            .unwrap();

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = WhoamiTool::new(path).start();
            let (reply_to, rx) = capture_pair();

            tool.do_send(InvokeTool {
                tool_use_id: "tu_wa2".to_owned(),
                name: "whoami".to_owned(),
                input: serde_json::json!({}),
                sender_id,
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(!result.is_error, "got error: {}", result.content);
            let parsed: serde_json::Value =
                serde_json::from_str(&result.content).expect("content is JSON");
            assert_eq!(parsed["identity_id"], sender_id.to_string());
            assert_eq!(parsed["name"], "worker-abc12345");

            actix::System::current().stop();
        });
    }

    // T_WA3: when the sender_id is not in the registry, name is null but the
    // identity_id is still returned. Covers the bootstrap case where an agent
    // calls whoami before its own registration is durable.
    #[test]
    fn whoami_returns_null_name_for_unregistered_sender() {
        let tmp = secure_dir();
        let path = tmp.path().join("registry.toml");
        AgentRegistry::open(path.clone()).unwrap(); // create empty
        let sender_id = IdentityId::new().unwrap();

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = WhoamiTool::new(path).start();
            let (reply_to, rx) = capture_pair();

            tool.do_send(InvokeTool {
                tool_use_id: "tu_wa3".to_owned(),
                name: "whoami".to_owned(),
                input: serde_json::json!({}),
                sender_id,
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(!result.is_error);
            let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(parsed["identity_id"], sender_id.to_string());
            assert!(
                parsed["name"].is_null(),
                "name should be null for unregistered sender; got: {}",
                parsed["name"]
            );

            actix::System::current().stop();
        });
    }
}
