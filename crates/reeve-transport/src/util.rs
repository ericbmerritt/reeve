//! Shared cryptographic primitives for envelope construction.
//!
//! [`fresh_nonce`] and [`sha256_payload_hash`] are used by every crate that
//! builds a signed envelope (dispatcher, CLI, TUI). Centralizing them here
//! prevents per-crate duplication of the same four-line functions and ensures
//! all callers use identical algorithms.

use rand_core::{OsRng, RngCore as _};
use sha2::{Digest, Sha256};

use reeve_types::{Nonce, PayloadHash, NONCE_LEN, PAYLOAD_HASH_LEN};

/// Generate a fresh 16-byte cryptographic nonce using the OS RNG.
pub fn fresh_nonce() -> Nonce {
    let mut bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut bytes);
    Nonce::from_bytes(bytes)
}

/// Compute the SHA-256 hash of `data` and return it as a [`PayloadHash`].
pub fn sha256_payload_hash(data: &[u8]) -> PayloadHash {
    let digest = Sha256::digest(data);
    let mut bytes = [0u8; PAYLOAD_HASH_LEN];
    bytes.copy_from_slice(&digest);
    PayloadHash::from_bytes(bytes)
}
