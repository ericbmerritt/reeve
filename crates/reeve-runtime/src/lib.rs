//! Reeve runtime.
//!
//! Long-lived background process: actix supervisor tree, agent actors,
//! maildir watcher, audit log writer, model resolution, cost meters. See
//! `specs/reeve-domain-model.md` § Runtime for owned state and lifecycle.

pub mod agent;
pub mod agent_fs;
pub mod audit;
pub mod config;
pub mod daemon;
pub(crate) mod fs_util;
pub mod identity_registry;
pub mod inbox;
pub mod keychain;
pub mod ledger;
pub mod model_resolution;
pub mod runtime_lock;
pub mod supervisor;
#[cfg(test)]
pub(crate) mod test_support;
pub mod verify;
pub mod watcher;

pub use agent::{Agent, AgentError, ProcessInbound, QuarantineEvent};
pub use agent_fs::{
    AgentDirs, AgentFsError, AtomicFileWriter, ConversationEntry, ConversationThread,
};
pub use audit::{AuditError, AuditEvent, AuditLog};
pub use config::{
    install_defaults, load_persona_config, load_team_config, ConfigError, PersonaConfig,
    TeamConfig, TeamMember,
};
pub use daemon::{
    daemon_run, daemon_spawn, daemon_status, daemon_stop, heartbeat_fresh, DaemonError,
    DaemonStatus,
};
pub use identity_registry::{IdentityRegistry, RegistryError, StoredIdentity};
pub use inbox::{AgentInbox, InboxError, InboxLayout};
pub use keychain::{
    labels, KeychainError, OperatorKeyStore, OperatorSecretStore, KEYCHAIN_SERVICE, SEED_LEN,
};
pub use ledger::{
    DeliveryKey, DeliveryLedger, DeliveryRecord, LedgerError, ReplayKey, ReplayLedger, ReplayRecord,
};
pub use model_resolution::{resolve_model, write_spawn_snapshot, ModelResolveError, SpawnSnapshot};
pub use runtime_lock::{default_state_dir, RuntimeLock, RuntimeLockError};
pub use supervisor::{HeartbeatActor, WatchInbox, WatcherActor};
pub use verify::{
    emit_quarantine_audit, EnvelopeIds, QuarantineReason, Verdict, VerificationError,
    VerificationPipeline, DEFAULT_CLOCK_SKEW, MAX_ENVELOPE_BYTES,
};
pub use watcher::{FilenameError, ProcessOutcome, RotationOutcome, Watcher, WatcherError};
