//! Reeve transport.
//!
//! Canonical JSON serialization, ed25519 sign/verify, maildir state machine,
//! replay and delivery ledgers. See `specs/reeve-transport-security.md` for
//! the trust contract this layer implements.

pub mod canonical;
pub mod sign;
pub mod util;

pub use canonical::{canonical_bytes, CanonicalError};
pub use sign::{sign_envelope, verify_envelope, SignError, VerifyError};
pub use util::{fresh_nonce, sha256_payload_hash};
