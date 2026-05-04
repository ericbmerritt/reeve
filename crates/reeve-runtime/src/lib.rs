//! Reeve runtime.
//!
//! Long-lived background process: actix supervisor tree, agent actors,
//! maildir watcher, audit log writer, model resolution, cost meters. See
//! `specs/reeve-domain-model.md` § Runtime for owned state and lifecycle.

pub mod identity_registry;
pub mod keychain;

pub use identity_registry::{IdentityRegistry, RegistryError, StoredIdentity};
pub use keychain::{KeychainError, OperatorKeyStore, KEYCHAIN_SERVICE, SEED_LEN};
