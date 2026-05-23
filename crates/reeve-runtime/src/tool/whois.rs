//! `whois` tool: resolve an `identity_id` to a human-readable record.
//!
//! Pairs with [`crate::tool::WhoamiTool`] for self-identification and
//! [`crate::tool::ListAgentsTool`] for agent-registry enumeration. The
//! complementary use case is the one the lead actually runs into: an
//! inbound message carries a `[from <uuid>]` prefix, and the model wants
//! to know which human or agent that UUID belongs to before deciding how
//! to respond.
//!
//! Re-opens the identity registry per call so identities enrolled after
//! daemon start are visible without a restart — same pattern as the
//! [`crate::dispatcher::MessageDispatcher`] and [`crate::tool::ListAgentsTool`].

use std::path::PathBuf;

use actix::{Actor, Context, Handler};
use reeve_types::IdentityId;

use super::{check_authority, AuthorityDecision, InvokeTool, ToolResult};
use crate::identity_registry::IdentityRegistry;

/// Tool that looks up an `identity_id` in the operator-owned identity
/// registry and returns the matching `{kind, display_name}` record.
pub struct WhoisTool {
    data_dir: PathBuf,
}

impl WhoisTool {
    /// Construct a [`WhoisTool`] rooted at the runtime's `data_dir`. The
    /// identity registry directory lives under this path.
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }

    /// Adapter-facing tool descriptor for [`WhoisTool`].
    #[must_use]
    pub fn descriptor() -> reeve_adapter::Tool {
        reeve_adapter::Tool {
            name: "whois".to_owned(),
            description: "Resolve an identity_id to its registered record. \
                Use this when an inbound message arrives with a \
                `[from <uuid>]` prefix and you want to know which human \
                operator or agent that identity belongs to. \
                \n\nReturns a JSON object: \
                `{identity_id, kind, display_name}` where kind is one of \
                \"Operator\", \"Agent\", or \"External\". \
                Returns `{identity_id, kind: null, display_name: null}` \
                (with is_error=false) when the identity is not registered \
                — distinguishes \"unknown id\" from a registry fault. \
                \n\nArguments: \
                \n  `identity_id` (string, required) — UUIDv7 to look up. \
                \n\nFailure modes (is_error=true):\n\
                - `whois: InvalidArgs` — the `identity_id` argument is \
                  missing or not a string\n\
                - `whois: InvalidIdentityId` — argument is not a valid \
                  UUIDv7\n\
                - `whois: IdentityRegistryOpen` — registry is unreadable \
                  (permissions, missing directory). Runtime fault, not \
                  model-correctable.\n\
                - `whois: IdentityLookup` — registry opened but lookup \
                  failed (corrupt entry, IO error mid-read)."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "identity_id": {
                        "type": "string",
                        "description": "UUIDv7 identifier to resolve.",
                    }
                },
                "required": ["identity_id"],
            }),
        }
    }
}

impl Actor for WhoisTool {
    type Context = Context<Self>;
}

/// Resolve `input.identity_id` against the registry at `data_dir`, mapping
/// each failure mode to the matching `whois:` error string. Pulled out of
/// the handler so the actor body stays focused on the reply plumbing.
fn resolve_input(
    data_dir: &std::path::Path,
    input: &serde_json::Value,
) -> Result<serde_json::Value, &'static str> {
    let id_str = input
        .get("identity_id")
        .and_then(|v| v.as_str())
        .ok_or("whois: InvalidArgs")?;

    let id = uuid::Uuid::parse_str(id_str)
        .ok()
        .and_then(|u| IdentityId::try_from(u).ok())
        .ok_or("whois: InvalidIdentityId")?;

    let registry = IdentityRegistry::open(data_dir.to_path_buf()).map_err(|err| {
        tracing::warn!(err = %err, "whois: failed to open identity registry");
        "whois: IdentityRegistryOpen"
    })?;

    match registry.lookup(id) {
        Ok(Some(stored)) => {
            let ident = stored.identity();
            Ok(serde_json::json!({
                "identity_id": ident.identity_id.to_string(),
                "kind": ident.identity_type.to_string(),
                "display_name": ident.display_name,
            }))
        }
        Ok(None) => Ok(serde_json::json!({
            "identity_id": id.to_string(),
            "kind": serde_json::Value::Null,
            "display_name": serde_json::Value::Null,
        })),
        Err(err) => {
            tracing::warn!(err = %err, identity_id = %id, "whois: lookup failed");
            Err("whois: IdentityLookup")
        }
    }
}

impl Handler<InvokeTool> for WhoisTool {
    type Result = ();

    fn handle(&mut self, msg: InvokeTool, _ctx: &mut Context<Self>) {
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

        let (content, is_error) = match resolve_input(&self.data_dir, &input) {
            Ok(value) => (value.to_string(), false),
            Err(category) => (category.to_owned(), true),
        };
        reply_to.do_send(ToolResult {
            tool_use_id,
            content,
            is_error,
        });
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity_registry::StoredIdentity;
    use crate::test_support::{
        secure_dir, tool_result_capture_pair as capture_pair, TOOL_RESULT_TIMEOUT as RESULT_TIMEOUT,
    };
    use reeve_types::{Identity, KeyRecord, Keypair};

    /// Enroll an identity into a registry at `data_dir` and return its id.
    fn enroll(data_dir: &std::path::Path, identity: Identity) -> IdentityId {
        let registry = IdentityRegistry::open(data_dir.to_path_buf()).unwrap();
        let id = identity.identity_id;
        let keypair = Keypair::generate();
        let key_record = KeyRecord::new(id, *keypair.public()).unwrap();
        let stored = StoredIdentity::new(identity, key_record).unwrap();
        registry.write(&stored).unwrap();
        id
    }

    // T_WI1: descriptor shape.
    #[test]
    fn whois_descriptor_shape() {
        let d = WhoisTool::descriptor();
        assert_eq!(d.name, "whois");
        assert!(!d.description.is_empty());
        assert_eq!(d.input_schema["type"], "object");
        let required = d.input_schema["required"].as_array().expect("required");
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "identity_id");
    }

    // T_WI2: resolves an enrolled operator to its display_name and kind.
    #[test]
    fn whois_resolves_registered_operator() {
        let tmp = secure_dir();
        let identity = Identity::new_operator("eric".to_owned()).unwrap();
        let id = enroll(tmp.path(), identity);

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = WhoisTool::new(tmp.path().to_path_buf()).start();
            let (reply_to, rx) = capture_pair();

            tool.do_send(InvokeTool {
                tool_use_id: "tu_wi2".to_owned(),
                name: "whois".to_owned(),
                input: serde_json::json!({ "identity_id": id.to_string() }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(!result.is_error, "got error: {}", result.content);
            let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(parsed["identity_id"], id.to_string());
            assert_eq!(parsed["kind"], "Operator");
            assert_eq!(parsed["display_name"], "eric");

            actix::System::current().stop();
        });
    }

    // T_WI3: an id that isn't in the registry returns nulls with is_error=false.
    // Distinguishes "no such identity" from "registry unreachable".
    #[test]
    fn whois_unknown_id_returns_nulls() {
        let tmp = secure_dir();
        IdentityRegistry::open(tmp.path().to_path_buf()).unwrap();
        let unknown = IdentityId::new().unwrap();

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = WhoisTool::new(tmp.path().to_path_buf()).start();
            let (reply_to, rx) = capture_pair();

            tool.do_send(InvokeTool {
                tool_use_id: "tu_wi3".to_owned(),
                name: "whois".to_owned(),
                input: serde_json::json!({ "identity_id": unknown.to_string() }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(!result.is_error);
            let parsed: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(parsed["identity_id"], unknown.to_string());
            assert!(parsed["kind"].is_null());
            assert!(parsed["display_name"].is_null());

            actix::System::current().stop();
        });
    }

    // T_WI4: a missing identity_id arg surfaces as InvalidArgs.
    #[test]
    fn whois_missing_arg_is_invalid_args() {
        let tmp = secure_dir();
        IdentityRegistry::open(tmp.path().to_path_buf()).unwrap();

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = WhoisTool::new(tmp.path().to_path_buf()).start();
            let (reply_to, rx) = capture_pair();

            tool.do_send(InvokeTool {
                tool_use_id: "tu_wi4".to_owned(),
                name: "whois".to_owned(),
                input: serde_json::json!({}),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(result.is_error);
            assert_eq!(result.content, "whois: InvalidArgs");

            actix::System::current().stop();
        });
    }

    // T_WI5: a malformed UUID surfaces as InvalidIdentityId, not as a panic
    // or generic Io error.
    #[test]
    fn whois_malformed_uuid_is_invalid_identity_id() {
        let tmp = secure_dir();
        IdentityRegistry::open(tmp.path().to_path_buf()).unwrap();

        actix::System::new().block_on(async move {
            use actix::Actor as _;
            let tool = WhoisTool::new(tmp.path().to_path_buf()).start();
            let (reply_to, rx) = capture_pair();

            tool.do_send(InvokeTool {
                tool_use_id: "tu_wi5".to_owned(),
                name: "whois".to_owned(),
                input: serde_json::json!({ "identity_id": "not-a-uuid" }),
                sender_id: IdentityId::new().unwrap(),
                reply_to,
            });

            let result = tokio::time::timeout(RESULT_TIMEOUT, rx)
                .await
                .expect("ToolResult did not arrive within timeout")
                .expect("sender dropped");

            assert!(result.is_error);
            assert_eq!(result.content, "whois: InvalidIdentityId");

            actix::System::current().stop();
        });
    }
}
