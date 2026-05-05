//! Reeve transport.
//!
//! Canonical JSON serialization, ed25519 sign/verify, maildir state machine,
//! replay and delivery ledgers. See `specs/reeve-transport-security.md` for
//! the trust contract this layer implements.

pub mod canonical;

pub use canonical::{canonical_bytes, CanonicalError};
