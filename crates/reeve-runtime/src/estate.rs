//! The estate coordinator: operator-tier organizational operations.
//!
//! Estate operations — opening, closing, and reopening engagements — arrive
//! as signed envelopes in the coordinator's maildir inbox, exactly the path
//! any agent message takes (decision A1 in
//! `specs/reeve-organization.ladder.md`: the filesystem is the protocol; no
//! socket, no RPC). The CLI and the TUI slash-command both sign as the
//! operator and deposit to `agents/estate/inbox/new/`; the watcher verifies
//! the signature and delivers [`ProcessInbound`] here like anywhere else.
//!
//! Authority is the operator tier: an envelope whose verified sender is not
//! the enrolled operator is refused, and — like every refusal on this actor —
//! audited as `engagement.op_refused`. There is no reply channel; the
//! durable engagement record is the operation's observable effect, and the
//! audit log is its receipt.
//!
//! The coordinator occupies the reserved agent name `estate` in the agent
//! registry so name→(identity, inbox) resolution works with the same lookup
//! every other sender uses. It is not a model-backed agent: the daemon's
//! resume pass skips it, and it never makes model calls.

use std::path::PathBuf;
use std::sync::Arc;

use actix::{Actor, Context, Handler, Supervised};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tracing::{info, warn};

use crate::agent::ProcessInbound;
use crate::audit::{AuditEvent, AuditLog};
use crate::engagement::{EngagementError, EngagementRegistry};

/// Reserved agent-registry name for the estate coordinator.
pub const ESTATE_AGENT_NAME: &str = "estate";

/// An estate operation, carried as the JSON payload of a signed envelope.
///
/// The `op` tag values are the operations vocabulary of
/// `specs/reeve-organization.md` § Operations Vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case", deny_unknown_fields)]
pub enum EstateOp {
    /// Open a top-level engagement. `root` must be an absolute path when
    /// present; the coordinator canonicalizes it before recording.
    OpenEngagement {
        name: String,
        purpose: String,
        #[serde(default)]
        root: Option<PathBuf>,
    },
    /// Close an open engagement.
    CloseEngagement { name: String },
    /// Reopen a closed engagement with its recorded context intact.
    ReopenEngagement { name: String },
}

impl EstateOp {
    /// The operation verb as it appears in audit events and payloads.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::OpenEngagement { .. } => "open-engagement",
            Self::CloseEngagement { .. } => "close-engagement",
            Self::ReopenEngagement { .. } => "reopen-engagement",
        }
    }

    /// The engagement name the operation targets.
    pub fn name(&self) -> &str {
        match self {
            Self::OpenEngagement { name, .. }
            | Self::CloseEngagement { name }
            | Self::ReopenEngagement { name } => name,
        }
    }
}

fn refusal_reason(err: &EngagementError) -> &'static str {
    match err {
        EngagementError::InvalidName { .. } => "invalid_name",
        EngagementError::NameTaken { .. } => "name_taken",
        EngagementError::NotFound { .. } => "not_found",
        EngagementError::WrongState { .. } => "wrong_state",
        EngagementError::RelativeRoot { .. } => "relative_root",
        EngagementError::Io { .. } => "io_error",
        EngagementError::Toml { .. } => "record_corrupt",
    }
}

/// Actor handling estate operations delivered through the signed-envelope
/// transport.
pub struct EstateCoordinator {
    operator_id: reeve_types::IdentityId,
    engagements: EngagementRegistry,
    audit: Arc<AuditLog>,
}

impl EstateCoordinator {
    pub fn new(
        operator_id: reeve_types::IdentityId,
        engagements: EngagementRegistry,
        audit: Arc<AuditLog>,
    ) -> Self {
        Self {
            operator_id,
            engagements,
            audit,
        }
    }

    fn audit_event(&self, event: &AuditEvent) {
        if let Err(err) = self.audit.append(event) {
            warn!(err = %err, "estate coordinator failed to append audit event");
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the parameters mirror the fields of the EngagementOpRefused \
                  audit event one-to-one; a bundling struct would duplicate \
                  that event's shape for no reader benefit"
    )]
    fn refuse(
        &self,
        sender_id: Option<reeve_types::IdentityId>,
        op: &str,
        name: Option<String>,
        reason: &str,
        at: OffsetDateTime,
    ) {
        warn!(op, ?name, reason, "estate operation refused");
        self.audit_event(&AuditEvent::EngagementOpRefused {
            sender_id,
            op: op.to_owned(),
            name,
            reason: reason.to_owned(),
            at,
        });
    }

    fn execute(&self, op: &EstateOp, sender_id: reeve_types::IdentityId, at: OffsetDateTime) {
        match op {
            EstateOp::OpenEngagement {
                name,
                purpose,
                root,
            } => {
                // Canonicalize so the recorded context — the path the
                // effectors ladder will enforce as the file jail — is the
                // resolved directory, not whatever spelling the operator
                // typed. A root that does not resolve is a refusal, not a
                // best-effort record.
                let canonical_root = match root {
                    Some(raw) => match std::fs::canonicalize(raw) {
                        Ok(resolved) => Some(resolved),
                        Err(err) => {
                            warn!(root = %raw.display(), err = %err, "engagement root does not resolve");
                            self.refuse(
                                Some(sender_id),
                                op.verb(),
                                Some(name.clone()),
                                "root_unresolvable",
                                at,
                            );
                            return;
                        }
                    },
                    None => None,
                };
                match self
                    .engagements
                    .open_engagement(name, purpose, canonical_root, at)
                {
                    Ok(record) => {
                        info!(name, root = ?record.root, "engagement opened");
                        self.audit_event(&AuditEvent::EngagementOpened {
                            sender_id,
                            name: record.name,
                            root: record.root,
                            at,
                        });
                    }
                    Err(err) => self.refuse(
                        Some(sender_id),
                        op.verb(),
                        Some(name.clone()),
                        refusal_reason(&err),
                        at,
                    ),
                }
            }
            EstateOp::CloseEngagement { name } => match self.engagements.close(name) {
                Ok(_) => {
                    info!(name, "engagement closed");
                    self.audit_event(&AuditEvent::EngagementClosed {
                        sender_id,
                        name: name.clone(),
                        at,
                    });
                }
                Err(err) => self.refuse(
                    Some(sender_id),
                    op.verb(),
                    Some(name.clone()),
                    refusal_reason(&err),
                    at,
                ),
            },
            EstateOp::ReopenEngagement { name } => match self.engagements.reopen(name) {
                Ok(_) => {
                    info!(name, "engagement reopened");
                    self.audit_event(&AuditEvent::EngagementReopened {
                        sender_id,
                        name: name.clone(),
                        at,
                    });
                }
                Err(err) => self.refuse(
                    Some(sender_id),
                    op.verb(),
                    Some(name.clone()),
                    refusal_reason(&err),
                    at,
                ),
            },
        }
    }
}

impl Actor for EstateCoordinator {
    type Context = Context<Self>;
}

impl Supervised for EstateCoordinator {}

impl Handler<ProcessInbound> for EstateCoordinator {
    type Result = ();

    fn handle(&mut self, msg: ProcessInbound, _ctx: &mut Context<Self>) {
        let at = OffsetDateTime::now_utc();
        let op: EstateOp = match serde_json::from_str(&msg.payload) {
            Ok(op) => op,
            Err(err) => {
                warn!(err = %err, "estate payload is not a valid operation");
                self.refuse(Some(msg.sender_id), "unknown", None, "invalid_payload", at);
                return;
            }
        };
        // The watcher verified the signature; this check is the authority
        // tier: only the enrolled operator commands the estate. Parsing
        // before the tier check lets the refusal audit name the attempted
        // operation.
        if msg.sender_id != self.operator_id {
            self.refuse(
                Some(msg.sender_id),
                op.verb(),
                Some(op.name().to_owned()),
                "not_operator",
                at,
            );
            return;
        }
        self.execute(&op, msg.sender_id, at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use crate::engagement::EngagementState;
    use crate::test_support::secure_dir;

    fn read_audit_kinds(data_dir: &Path) -> Vec<(String, serde_json::Value)> {
        let path = crate::audit::audit_log_path(data_dir);
        let body = fs::read_to_string(path).unwrap_or_default();
        body.lines()
            .map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                (v["kind"].as_str().unwrap().to_owned(), v)
            })
            .collect()
    }

    struct TestEstate {
        data_dir: tempfile::TempDir,
        operator_id: reeve_types::IdentityId,
        addr: actix::Addr<EstateCoordinator>,
    }

    fn start_estate() -> TestEstate {
        let data_dir = secure_dir();
        let operator_id = reeve_types::IdentityId::new().unwrap();
        let engagements = EngagementRegistry::open(data_dir.path().join("engagements")).unwrap();
        let audit = Arc::new(AuditLog::open(data_dir.path().to_path_buf()).unwrap());
        let addr = EstateCoordinator::new(operator_id, engagements, audit).start();
        TestEstate {
            data_dir,
            operator_id,
            addr,
        }
    }

    fn inbound(sender_id: reeve_types::IdentityId, op: &EstateOp) -> ProcessInbound {
        ProcessInbound {
            payload: serde_json::to_string(op).unwrap(),
            message_id: "test-message".to_owned(),
            sender_id,
        }
    }

    async fn send(estate: &TestEstate, sender: reeve_types::IdentityId, op: &EstateOp) {
        tokio::time::timeout(
            Duration::from_secs(5),
            estate.addr.send(inbound(sender, op)),
        )
        .await
        .expect("estate handler timed out")
        .expect("estate mailbox closed");
    }

    #[test]
    fn operator_open_close_reopen_round_trip_with_audit() {
        actix::System::new().block_on(async {
            let estate = start_estate();
            let root = estate.data_dir.path().join("work");
            fs::create_dir_all(&root).unwrap();
            let open = EstateOp::OpenEngagement {
                name: "reconciler".to_owned(),
                purpose: "modernize".to_owned(),
                root: Some(root.clone()),
            };
            send(&estate, estate.operator_id, &open).await;
            send(
                &estate,
                estate.operator_id,
                &EstateOp::CloseEngagement {
                    name: "reconciler".to_owned(),
                },
            )
            .await;
            send(
                &estate,
                estate.operator_id,
                &EstateOp::ReopenEngagement {
                    name: "reconciler".to_owned(),
                },
            )
            .await;

            let registry =
                EngagementRegistry::open(estate.data_dir.path().join("engagements")).unwrap();
            let record = registry.get("reconciler").unwrap();
            assert_eq!(record.state, EngagementState::Open);
            assert_eq!(record.root, Some(fs::canonicalize(&root).unwrap()));

            let kinds: Vec<String> = read_audit_kinds(estate.data_dir.path())
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            assert_eq!(
                kinds,
                vec![
                    "engagement.opened",
                    "engagement.closed",
                    "engagement.reopened"
                ],
            );
        });
    }

    #[test]
    fn non_operator_sender_is_refused_and_audited() {
        actix::System::new().block_on(async {
            let estate = start_estate();
            let stranger = reeve_types::IdentityId::new().unwrap();
            let op = EstateOp::OpenEngagement {
                name: "sneaky".to_owned(),
                purpose: "p".to_owned(),
                root: None,
            };
            send(&estate, stranger, &op).await;

            let registry =
                EngagementRegistry::open(estate.data_dir.path().join("engagements")).unwrap();
            assert!(matches!(
                registry.get("sneaky").unwrap_err(),
                EngagementError::NotFound { .. }
            ));
            let events = read_audit_kinds(estate.data_dir.path());
            assert_eq!(events.len(), 1);
            let (kind, v) = &events[0];
            assert_eq!(kind, "engagement.op_refused");
            assert_eq!(v["reason"], "not_operator");
            assert_eq!(v["op"], "open-engagement");
            assert_eq!(v["name"], "sneaky");
        });
    }

    #[test]
    fn invalid_payload_is_refused_and_audited() {
        actix::System::new().block_on(async {
            let estate = start_estate();
            tokio::time::timeout(
                Duration::from_secs(5),
                estate.addr.send(ProcessInbound {
                    payload: "not json at all".to_owned(),
                    message_id: "m".to_owned(),
                    sender_id: estate.operator_id,
                }),
            )
            .await
            .expect("estate handler timed out")
            .expect("estate mailbox closed");

            let events = read_audit_kinds(estate.data_dir.path());
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].0, "engagement.op_refused");
            assert_eq!(events[0].1["reason"], "invalid_payload");
        });
    }

    #[test]
    fn reused_name_is_refused_with_name_taken() {
        actix::System::new().block_on(async {
            let estate = start_estate();
            let open = |name: &str| EstateOp::OpenEngagement {
                name: name.to_owned(),
                purpose: "p".to_owned(),
                root: None,
            };
            send(&estate, estate.operator_id, &open("once")).await;
            send(
                &estate,
                estate.operator_id,
                &EstateOp::CloseEngagement {
                    name: "once".to_owned(),
                },
            )
            .await;
            send(&estate, estate.operator_id, &open("once")).await;

            let events = read_audit_kinds(estate.data_dir.path());
            let last = events.last().unwrap();
            assert_eq!(last.0, "engagement.op_refused");
            assert_eq!(last.1["reason"], "name_taken");
        });
    }

    #[test]
    fn unresolvable_root_is_refused() {
        actix::System::new().block_on(async {
            let estate = start_estate();
            let op = EstateOp::OpenEngagement {
                name: "ghost".to_owned(),
                purpose: "p".to_owned(),
                root: Some(estate.data_dir.path().join("does-not-exist")),
            };
            send(&estate, estate.operator_id, &op).await;

            let events = read_audit_kinds(estate.data_dir.path());
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].1["reason"], "root_unresolvable");
        });
    }
}
