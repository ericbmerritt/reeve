//! Reeve domain types.
//!
//! Identity / key records and ed25519 keypair primitives — pure data with no
//! I/O. See `specs/reeve-domain-model.md` for canonical definitions.

mod id_newtype;
mod identity;
mod key;
mod keypair;
mod uuid_v7;

pub use identity::{
    Identity, IdentityId, IdentityIdError, IdentityLifecycle, IdentityLifecycleError, IdentityType,
};
pub use key::{
    KeyId, KeyIdError, KeyRecord, KeyState, KeyStateError, PublicKey, PublicKeyDecodeError,
};
pub use keypair::{Keypair, PrivateKey};
