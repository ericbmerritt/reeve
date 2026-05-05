//! Reeve domain types.
//!
//! Identity / key records, ed25519 keypair primitives, and the signed message
//! envelope — pure data with no I/O. See `specs/reeve-domain-model.md` for
//! canonical definitions.

mod envelope;
mod id_newtype;
mod identity;
mod key;
mod keypair;
mod uuid_v7;

pub use envelope::{
    Envelope, EnvelopeError, EnvelopeSignature, MessageId, MessageIdError, Nonce, PayloadHash,
    SchemaVersion, NONCE_LEN, PAYLOAD_HASH_LEN, SIGNATURE_LEN,
};
pub use identity::{
    Identity, IdentityId, IdentityIdError, IdentityLifecycle, IdentityLifecycleError, IdentityType,
};
pub use key::{
    KeyId, KeyIdError, KeyRecord, KeyState, KeyStateError, PublicKey, PublicKeyDecodeError,
};
pub use keypair::{Keypair, PrivateKey};
