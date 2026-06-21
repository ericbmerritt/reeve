//! `list_personas` tool: directory of available personas.
//!
//! Reads `{data_dir}/personas/` and returns one entry per persona found,
//! including its name, display name, and model preferences. An agent can use
//! this to discover which personas are available before calling `spawn_agent`.

use std::path::PathBuf;
use std::sync::Arc;

use actix::{Actor, Context, Handler};

use super::{check_authority, emit_refusal_audit, AuditHandle, InvokeTool, ToolResult};
use crate::agent_fs::RuntimeLayout;
use crate::capability::{CapabilityProfile, ToolCategory};
use crate::config::load_persona_config;

/// Tool that lists every installed persona with its name, display name, and
/// model preferences. Read-only; no side effects.
pub struct ListPersonasTool {
    data_dir: PathBuf,
    profile: Option<Arc<CapabilityProfile>>,
    audit: Option<AuditHandle>,
}

impl ListPersonasTool {
    /// Construct a [`ListPersonasTool`] reading from `data_dir`.
    pub fn new(data_dir: PathBuf, profile: Option<Arc<CapabilityProfile>>) -> Self {
        Self {
            data_dir,
            profile,
            audit: None,
        }
    }

    pub fn with_audit(mut self, audit: AuditHandle) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Adapter-facing tool descriptor.
    #[must_use]
    pub fn descriptor() -> reeve_adapter::Tool {
        reeve_adapter::Tool {
            name: "list_personas".to_owned(),
            description: "List every persona installed in this Reeve runtime. \
                Returns a JSON array of objects: \
                `{name, display_name, model_preferences}` where `name` is the \
                identifier passed to `spawn_agent` and `model_preferences` is \
                the ordered list of models the persona will use. \
                Re-read on every call — newly installed personas appear \
                without a daemon restart. \
                \n\nNo arguments. \
                \n\nFailure mode: `list_personas: PersonasDirUnreadable` \
                (is_error=true) when the personas directory cannot be listed. \
                Individual persona configs that fail to parse are silently \
                skipped and do not cause the call to fail."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }
}

impl Actor for ListPersonasTool {
    type Context = Context<Self>;
}

impl Handler<InvokeTool> for ListPersonasTool {
    type Result = ();

    fn handle(&mut self, msg: InvokeTool, _ctx: &mut Context<Self>) {
        let InvokeTool {
            tool_use_id,
            name: _,
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
                "list_personas",
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

        let personas_dir = RuntimeLayout::new(&self.data_dir).personas_root();
        let read_dir = match std::fs::read_dir(&personas_dir) {
            Ok(rd) => rd,
            Err(err) => {
                tracing::warn!(
                    err = %err,
                    path = %personas_dir.display(),
                    "list_personas: failed to read personas directory"
                );
                reply_to.do_send(ToolResult {
                    tool_use_id,
                    content: "list_personas: PersonasDirUnreadable".to_owned(),
                    is_error: true,
                });
                return;
            }
        };

        let layout = RuntimeLayout::new(&self.data_dir);
        let mut personas: Vec<serde_json::Value> = Vec::new();

        for entry in read_dir.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let config_path = layout.persona_config_path(name_str);
            let Ok(config) = load_persona_config(&config_path) else {
                continue;
            };
            personas.push(serde_json::json!({
                "name": config.name,
                "display_name": config.display_name,
                "model_preferences": config.model_preferences,
            }));
        }

        personas.sort_by(|a, b| {
            a["name"]
                .as_str()
                .unwrap_or("")
                .cmp(b["name"].as_str().unwrap_or(""))
        });

        let content = serde_json::to_string(&personas).unwrap_or_else(|_| "[]".to_owned());
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
    use crate::config::install_defaults;
    use crate::test_support::{
        secure_dir, tool_result_capture_pair as capture_pair, TOOL_RESULT_TIMEOUT as RESULT_TIMEOUT,
    };
    use reeve_types::IdentityId;

    // T_LP1: descriptor has expected shape.
    #[test]
    fn list_personas_descriptor_shape() {
        let d = ListPersonasTool::descriptor();
        assert_eq!(d.name, "list_personas");
        assert!(!d.description.is_empty());
        assert_eq!(d.input_schema["type"], "object");
        let required = d.input_schema["required"]
            .as_array()
            .expect("required array");
        assert!(required.is_empty(), "no required args");
    }

    // T_LP2: after install_defaults, list_personas returns at least lead and
    // deepseek-r1.
    #[test]
    fn list_personas_returns_installed_defaults() {
        let tmp = secure_dir();
        install_defaults(tmp.path()).unwrap();

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = ListPersonasTool::new(tmp.path().to_path_buf(), None).start();
            let (reply_to, rx) = capture_pair();

            tool.do_send(InvokeTool {
                tool_use_id: "tu_lp2".to_owned(),
                name: "list_personas".to_owned(),
                input: serde_json::json!({}),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive")
                .expect("sender dropped");

            assert!(!result.is_error, "got error: {}", result.content);
            let parsed: Vec<serde_json::Value> =
                serde_json::from_str(&result.content).expect("valid JSON array");

            let names: Vec<&str> = parsed.iter().map(|v| v["name"].as_str().unwrap()).collect();
            assert!(names.contains(&"lead"), "missing lead; got: {names:?}");
            assert!(
                names.contains(&"deepseek-r1"),
                "missing deepseek-r1; got: {names:?}"
            );

            let deepseek = parsed.iter().find(|v| v["name"] == "deepseek-r1").unwrap();
            assert_eq!(
                deepseek["model_preferences"][0],
                "deepseek/deepseek-r1-0528"
            );

            actix::System::current().stop();
        });
    }

    // T_LP3: unreadable personas dir surfaces as the documented failure category.
    #[test]
    fn list_personas_missing_dir_returns_error() {
        let tmp = secure_dir();

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = ListPersonasTool::new(tmp.path().to_path_buf(), None).start();
            let (reply_to, rx) = capture_pair();

            tool.do_send(InvokeTool {
                tool_use_id: "tu_lp3".to_owned(),
                name: "list_personas".to_owned(),
                input: serde_json::json!({}),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive")
                .expect("sender dropped");

            assert!(result.is_error, "expected is_error=true");
            assert_eq!(result.content, "list_personas: PersonasDirUnreadable");

            actix::System::current().stop();
        });
    }

    // T_LP4: results are sorted alphabetically by name.
    #[test]
    fn list_personas_sorted_alphabetically() {
        let tmp = secure_dir();
        install_defaults(tmp.path()).unwrap();

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = ListPersonasTool::new(tmp.path().to_path_buf(), None).start();
            let (reply_to, rx) = capture_pair();

            tool.do_send(InvokeTool {
                tool_use_id: "tu_lp4".to_owned(),
                name: "list_personas".to_owned(),
                input: serde_json::json!({}),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive")
                .expect("sender dropped");

            let parsed: Vec<serde_json::Value> =
                serde_json::from_str(&result.content).expect("valid JSON array");
            let names: Vec<&str> = parsed.iter().map(|v| v["name"].as_str().unwrap()).collect();
            let mut sorted = names.clone();
            sorted.sort_unstable();
            assert_eq!(names, sorted, "results must be sorted by name");

            actix::System::current().stop();
        });
    }
}
