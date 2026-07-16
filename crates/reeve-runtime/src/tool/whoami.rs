//! `whoami` tool: report the calling agent's own identity.
//!
//! The runtime sets [`InvokeTool::sender_id`] from the agent actor's own
//! identity state (not from model input), so the result is authoritative —
//! a model can't spoof its way into a different identity by lying to the
//! tool. Used by agents that need to self-identify when constructing
//! envelopes by hand, or that want to confirm their place in the registry.

use std::path::PathBuf;

use actix::{Actor, Context, Handler};

use super::{check_authority, emit_refusal_audit, AuditHandle, InvokeTool, ToolResult};
use crate::agent_registry::AgentRegistry;
use crate::capability::ToolCategory;

/// Tool that returns the invoking agent's identity. No arguments.
pub struct WhoamiTool {
    agent_registry_path: PathBuf,
    /// Root of the Reeve data directory. Used to read the resolved agent's
    /// own `agent.toml` for its current engagement name and working root.
    data_dir: PathBuf,
    profile: Option<std::sync::Arc<crate::capability::CapabilityProfile>>,
    audit: Option<AuditHandle>,
}

impl WhoamiTool {
    /// Construct a [`WhoamiTool`] reading from the given registry file for
    /// name resolution.
    pub fn new(
        agent_registry_path: PathBuf,
        data_dir: PathBuf,
        profile: Option<std::sync::Arc<crate::capability::CapabilityProfile>>,
    ) -> Self {
        Self {
            agent_registry_path,
            data_dir,
            profile,
            audit: None,
        }
    }

    pub fn with_audit(mut self, audit: AuditHandle) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Adapter-facing tool descriptor for [`WhoamiTool`].
    #[must_use]
    pub fn descriptor() -> reeve_adapter::Tool {
        reeve_adapter::Tool {
            name: "whoami".to_owned(),
            description: "Return the calling agent's own identity. Returns a \
                JSON object: `{identity_id, name, engagement, working_root}` \
                where identity_id is the stable UUIDv7 the runtime knows the \
                agent by, name is the registered agent name (e.g. \"lead\", \
                \"worker-abc12345\") or null if the agent is not in the \
                registry, and engagement/working_root are the currently \
                staffed engagement's name and root — both null when \
                unstaffed (no engagement, no root, no daemon-cwd fallback). \
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

        // whoami is an informational tool; no category-based authority check
        // in phase 1. Future ladders may assign a category when the full
        // category set is defined.
        if let Err(refusal) =
            check_authority(self.profile.as_deref(), ToolCategory::ReadFiles, sender_id)
        {
            emit_refusal_audit(
                self.audit.as_ref(),
                &refusal,
                sender_id,
                "whoami",
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

        // Best-effort: an agent with no readable agent.toml (unlikely, but
        // possible under disk corruption) just gets null engagement/root
        // rather than an error — the identity_id/name above is the part
        // whoami exists to guarantee.
        let (engagement, working_root) = resolved_name
            .as_deref()
            .and_then(|name| crate::agent_fs::AgentDirs::open(&self.data_dir, name).ok())
            .and_then(|dirs| std::fs::read_to_string(dirs.agent_toml_path()).ok())
            .and_then(|text| toml::from_str::<crate::model_resolution::SpawnSnapshot>(&text).ok())
            .map_or((None, None), |snapshot| {
                (snapshot.engagement_name, snapshot.working_root)
            });

        let content = serde_json::json!({
            "identity_id": sender_id.to_string(),
            "name": resolved_name,
            "engagement": engagement,
            "working_root": working_root,
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
                stopped_reason: None,
            })
            .unwrap();

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = WhoamiTool::new(path, tmp.path().to_path_buf(), None).start();
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

    // T_WA4: a staffed agent's whoami reports its engagement name and
    // working root, read from its own agent.toml.
    #[test]
    fn whoami_reports_engagement_and_root_when_staffed() {
        use crate::agent_fs::AgentDirs;
        use crate::model_resolution::{write_spawn_snapshot, SpawnSnapshot};

        let tmp = secure_dir();
        let data_dir = tmp.path().to_path_buf();
        let registry_path = data_dir.join("registry.toml");
        let mut registry = AgentRegistry::open(registry_path.clone()).unwrap();
        let sender_id = IdentityId::new().unwrap();
        registry
            .register(AgentRecord {
                name: ValidatedAgentName::new("default-lead").unwrap(),
                identity_id: sender_id,
                inbox_dir: data_dir.join("inbox"),
                persona_name: Some("lead".to_owned()),
                spawned_at: OffsetDateTime::now_utc(),
                status: AgentStatus::Running,
                stopped_reason: None,
            })
            .unwrap();
        let dirs = AgentDirs::provision(&data_dir, "default-lead").unwrap();
        write_spawn_snapshot(
            &dirs,
            &SpawnSnapshot {
                persona_name: "lead".to_owned(),
                persona_version: 1,
                adapter_id: "claude-opus-4-7@anthropic-direct".to_owned(),
                agent_id: sender_id.to_string(),
                system_prompt: String::new(),
                system_prompt_source: None,
                engagement_name: Some("reconciler".to_owned()),
                working_root: Some(PathBuf::from("/repo/reconciler")),
            },
        )
        .unwrap();

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = WhoamiTool::new(registry_path, data_dir, None).start();
            let (reply_to, rx) = capture_pair();

            tool.do_send(InvokeTool {
                tool_use_id: "tu_wa4".to_owned(),
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
            assert_eq!(parsed["engagement"], "reconciler");
            assert_eq!(parsed["working_root"], "/repo/reconciler");

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
            let tool = WhoamiTool::new(path, tmp.path().to_path_buf(), None).start();
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
