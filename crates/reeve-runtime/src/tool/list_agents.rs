//! `list_agents` tool: read-only directory of the agent registry.
//!
//! Returns the records the dispatcher reads on every send: enough for an
//! agent to discover names it can address with `send_message` without having
//! to remember every spawn it has been told about.
//!
//! Re-opens the registry from disk per call — same pattern as
//! [`crate::dispatcher::MessageDispatcher`] — so newly spawned agents
//! appear without a daemon restart.

use std::path::PathBuf;

use actix::{Actor, Context, Handler};

use super::{check_authority, emit_refusal_audit, AuditHandle, InvokeTool, ToolResult};
use crate::agent_registry::AgentRegistry;
use crate::capability::{CapabilityProfile, ToolCategory};
use std::sync::Arc;

/// Tool that lists every agent in the agent registry, including the lead and
/// any spawned subagents, alive or stopped. Read-only; no side effects.
pub struct ListAgentsTool {
    agent_registry_path: PathBuf,
    profile: Option<Arc<CapabilityProfile>>,
    audit: Option<AuditHandle>,
}

impl ListAgentsTool {
    /// Construct a [`ListAgentsTool`] reading from the given registry file.
    pub fn new(agent_registry_path: PathBuf, profile: Option<Arc<CapabilityProfile>>) -> Self {
        Self {
            agent_registry_path,
            profile,
            audit: None,
        }
    }

    pub fn with_audit(mut self, audit: AuditHandle) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Adapter-facing tool descriptor for [`ListAgentsTool`].
    #[must_use]
    pub fn descriptor() -> reeve_adapter::Tool {
        reeve_adapter::Tool {
            name: "list_agents".to_owned(),
            description: "List every agent in the runtime's registry. \
                Returns a JSON array of objects: \
                `{name, identity_id, persona_name, status}` where status is \
                \"running\" or \"stopped\". Use the `name` field as the `to` \
                argument to send_message. Stopped agents appear with \
                status \"stopped\"; records can be removed by the operator \
                via the TUI. \
                Re-read on every call — newly spawned subagents appear \
                without a daemon restart. \
                \n\nNo arguments. \
                \n\nFailure mode: `list_agents: AgentRegistryOpen` \
                (is_error=true) when the registry file is unreadable \
                (permissions, missing, malformed TOML). This is a runtime \
                fault, not a model-correctable error."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }
}

impl Actor for ListAgentsTool {
    type Context = Context<Self>;
}

impl Handler<InvokeTool> for ListAgentsTool {
    type Result = ();

    fn handle(&mut self, msg: InvokeTool, _ctx: &mut Context<Self>) {
        let InvokeTool {
            tool_use_id,
            name,
            input: _,
            sender_id,
            reply_to,
        } = msg;

        if let Err(refusal) =
            check_authority(self.profile.as_deref(), ToolCategory::ReadFiles, sender_id)
        {
            emit_refusal_audit(
                self.audit.as_ref(),
                &refusal,
                sender_id,
                "list_agents",
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
        let _ = name;

        let registry = match AgentRegistry::open(self.agent_registry_path.clone()) {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(
                    err = %err,
                    "list_agents: failed to open agent registry"
                );
                reply_to.do_send(ToolResult {
                    tool_use_id,
                    content: "list_agents: AgentRegistryOpen".to_owned(),
                    is_error: true,
                });
                return;
            }
        };

        let records: Vec<serde_json::Value> = registry
            .list()
            .map(|r| {
                serde_json::json!({
                    "name": r.name.as_str(),
                    "identity_id": r.identity_id.to_string(),
                    "persona_name": r.persona_name,
                    "status": match r.status {
                        crate::agent_registry::AgentStatus::Running => "running",
                        crate::agent_registry::AgentStatus::Stopped => "stopped",
                    },
                })
            })
            .collect();

        let content = serde_json::to_string(&records).unwrap_or_else(|_| "[]".to_owned());
        reply_to.do_send(ToolResult {
            tool_use_id,
            content,
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
    use std::path::Path;
    use time::OffsetDateTime;

    // T_LA1: descriptor has the expected shape.
    #[test]
    fn list_agents_descriptor_shape() {
        let d = ListAgentsTool::descriptor();
        assert_eq!(d.name, "list_agents");
        assert!(!d.description.is_empty());
        assert_eq!(d.input_schema["type"], "object");
        let required = d.input_schema["required"]
            .as_array()
            .expect("required array");
        assert!(required.is_empty(), "no required args");
    }

    /// Build a registry path at `<base>/registry.toml` and pre-populate with
    /// the given records. Returns the path.
    fn prepare_registry(base: &Path, records: &[(&str, AgentStatus, Option<&str>)]) -> PathBuf {
        let path = base.join("registry.toml");
        let mut registry = AgentRegistry::open(path.clone()).unwrap();
        for (name, status, persona) in records {
            registry
                .register(AgentRecord {
                    name: ValidatedAgentName::new(name).unwrap(),
                    identity_id: IdentityId::new().unwrap(),
                    inbox_dir: base.join(name).join("inbox"),
                    persona_name: persona.map(str::to_owned),
                    spawned_at: OffsetDateTime::now_utc(),
                    status: *status,
                    stopped_reason: None,
                })
                .unwrap();
        }
        path
    }

    // T_LA2: returns a JSON array with one entry per agent in the registry.
    #[test]
    fn list_agents_returns_registry_records() {
        let tmp = secure_dir();
        let path = prepare_registry(
            tmp.path(),
            &[
                ("lead", AgentStatus::Running, Some("lead")),
                ("worker-abc12345", AgentStatus::Running, Some("worker")),
                ("worker-def67890", AgentStatus::Stopped, Some("worker")),
            ],
        );

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = ListAgentsTool::new(path, None).start();
            let (reply_to, rx) = capture_pair();

            tool.do_send(InvokeTool {
                tool_use_id: "tu_la2".to_owned(),
                name: "list_agents".to_owned(),
                input: serde_json::json!({}),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(!result.is_error, "got error: {}", result.content);
            let parsed: Vec<serde_json::Value> =
                serde_json::from_str(&result.content).expect("content is JSON array");
            assert_eq!(parsed.len(), 3);

            let names: Vec<&str> = parsed.iter().map(|v| v["name"].as_str().unwrap()).collect();
            assert!(names.contains(&"lead"));
            assert!(names.contains(&"worker-abc12345"));
            assert!(names.contains(&"worker-def67890"));

            let statuses: Vec<&str> = parsed
                .iter()
                .map(|v| v["status"].as_str().unwrap())
                .collect();
            assert!(statuses.contains(&"running"));
            assert!(statuses.contains(&"stopped"));

            actix::System::current().stop();
        });
    }

    // T_LA3: an unreadable registry path surfaces as the documented failure
    // category, not as a panic or generic Io.
    #[test]
    fn list_agents_returns_registry_open_error_when_path_missing() {
        let tmp = secure_dir();
        // Point at a path inside a non-directory parent so AgentRegistry::open
        // fails on the parent ensure_directory check.
        let path = tmp.path().join("not-a-dir").join("registry.toml");
        std::fs::write(tmp.path().join("not-a-dir"), b"sentinel").unwrap();

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = ListAgentsTool::new(path, None).start();
            let (reply_to, rx) = capture_pair();

            tool.do_send(InvokeTool {
                tool_use_id: "tu_la3".to_owned(),
                name: "list_agents".to_owned(),
                input: serde_json::json!({}),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(result.is_error, "expected is_error=true");
            assert_eq!(result.content, "list_agents: AgentRegistryOpen");

            actix::System::current().stop();
        });
    }

    // T_LA4: registry with no records returns an empty JSON array.
    #[test]
    fn list_agents_empty_registry_returns_empty_array() {
        let tmp = secure_dir();
        let path = prepare_registry(tmp.path(), &[]);

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = ListAgentsTool::new(path, None).start();
            let (reply_to, rx) = capture_pair();

            tool.do_send(InvokeTool {
                tool_use_id: "tu_la4".to_owned(),
                name: "list_agents".to_owned(),
                input: serde_json::json!({}),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(!result.is_error);
            assert_eq!(result.content, "[]");

            actix::System::current().stop();
        });
    }
}
