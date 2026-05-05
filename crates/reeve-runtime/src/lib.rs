//! Reeve runtime.
//!
//! Long-lived background process: actix supervisor tree, agent actors,
//! maildir watcher, audit log writer, model resolution, cost meters. See
//! `specs/reeve-domain-model.md` § Runtime for owned state and lifecycle.

pub mod audit;
pub(crate) mod fs_util;
pub mod identity_registry;
pub mod inbox;
pub mod keychain;

pub use audit::{AuditError, AuditEvent, AuditLog};
pub use identity_registry::{IdentityRegistry, RegistryError, StoredIdentity};
pub use inbox::{AgentInbox, InboxError, InboxLayout};
pub use keychain::{KeychainError, OperatorKeyStore, KEYCHAIN_SERVICE, SEED_LEN};
