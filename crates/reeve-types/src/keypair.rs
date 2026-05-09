//! Ed25519 keypair primitives.
//!
//! [`PrivateKey`] wraps the 32-byte ed25519 signing seed. It deliberately
//! does not implement [`serde::Serialize`] or [`serde::Deserialize`] —
//! domain-model invariant 5 forbids storing operator and external private
//! keys on disk, and the type system is the cheapest place to enforce that.
//! Private key material reaches durable storage only through the OS keychain
//! integration introduced in a later task; serde-based config writers cannot
//! accidentally include one.
//!
//! [`Keypair::generate`] is the only sanctioned constructor for fresh
//! keypairs.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, SECRET_KEY_LENGTH};
use rand_core::OsRng;
use zeroize::Zeroizing;

use crate::key::PublicKey;

/// 32-byte ed25519 signing seed. Held in memory only; never serialized.
///
/// `PrivateKey` deliberately does not implement [`serde::Serialize`], which
/// the following doctest demonstrates by failing to compile:
///
/// ```compile_fail
/// use reeve_types::PrivateKey;
/// fn requires_serialize<T: serde::Serialize>(_: &T) {}
/// let seed = zeroize::Zeroizing::new([0_u8; 32]);
/// let key = PrivateKey::from_seed_bytes(&seed);
/// requires_serialize(&key);
/// ```
///
/// Nor does it implement [`serde::Deserialize`]:
///
/// ```compile_fail
/// use reeve_types::PrivateKey;
/// fn requires_deserialize<T: for<'a> serde::Deserialize<'a>>() {}
/// requires_deserialize::<PrivateKey>();
/// ```
///
/// Nor [`Clone`] — the seed has only one in-memory home at a time:
///
/// ```compile_fail
/// use reeve_types::PrivateKey;
/// fn requires_clone<T: Clone>(_: &T) {}
/// let seed = zeroize::Zeroizing::new([0_u8; 32]);
/// let key = PrivateKey::from_seed_bytes(&seed);
/// requires_clone(&key);
/// ```
pub struct PrivateKey {
    signing_key: SigningKey,
}

impl PrivateKey {
    /// Sign a message with this private key.
    pub fn sign(&self, message: &[u8]) -> Signature {
        self.signing_key.sign(message)
    }

    /// Borrow the underlying `ed25519_dalek::SigningKey`.
    ///
    /// Crate-private to limit the secret-extraction surface. Envelope
    /// signing in `reeve-transport` will reach in here through a sibling
    /// path in a later phase; external callers must use [`PrivateKey::sign`]
    /// until then.
    #[expect(
        dead_code,
        reason = "consumed by reeve-transport envelope signing in a later phase; kept here so the surface is committed and crate-private"
    )]
    pub(crate) fn as_signing_key(&self) -> &SigningKey {
        &self.signing_key
    }

    /// Expose the 32-byte signing seed wrapped in [`Zeroizing`] so the bytes
    /// are wiped from memory when the wrapper is dropped. Callers handing the
    /// bytes onward are responsible for keeping them out of any durable
    /// medium other than the OS keychain.
    pub fn to_seed_bytes(&self) -> Zeroizing<[u8; SECRET_KEY_LENGTH]> {
        Zeroizing::new(self.signing_key.to_bytes())
    }

    /// Reconstruct from a 32-byte signing seed (e.g. on retrieval from the
    /// OS keychain). The caller passes the seed by reference inside a
    /// [`Zeroizing`] wrapper so the original copy is wiped on drop.
    pub fn from_seed_bytes(seed: &Zeroizing<[u8; SECRET_KEY_LENGTH]>) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(seed),
        }
    }
}

impl std::fmt::Debug for PrivateKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrivateKey").finish_non_exhaustive()
    }
}

/// Matched ed25519 keypair. Produced by [`Keypair::generate`] and consumed
/// by the identity registry / OS keychain integration in later tasks.
#[derive(Debug)]
pub struct Keypair {
    private: PrivateKey,
    public: PublicKey,
}

impl Keypair {
    /// Generate a fresh ed25519 keypair using the OS RNG.
    pub fn generate() -> Self {
        Self::from_signing_key(SigningKey::generate(&mut OsRng))
    }

    /// Borrow the private half.
    pub fn private(&self) -> &PrivateKey {
        &self.private
    }

    /// Borrow the public half.
    pub fn public(&self) -> &PublicKey {
        &self.public
    }

    /// Reconstruct a keypair from a 32-byte signing seed retrieved from durable
    /// storage (e.g., a key file or the OS keychain). The public key is derived
    /// deterministically from the seed; no separate storage is required.
    ///
    /// The caller passes the seed inside a [`Zeroizing`] wrapper so the
    /// in-memory copy is wiped on drop.
    pub fn from_seed_bytes(seed: &Zeroizing<[u8; SECRET_KEY_LENGTH]>) -> Self {
        Self::from_signing_key(SigningKey::from_bytes(seed))
    }

    fn from_signing_key(signing_key: SigningKey) -> Self {
        let verifying_key = signing_key.verifying_key();
        Self {
            private: PrivateKey { signing_key },
            public: PublicKey::from_verifying_key(verifying_key),
        }
    }

    /// Decompose the keypair into its private and public halves.
    pub fn into_parts(self) -> (PrivateKey, PublicKey) {
        (self.private, self.public)
    }

    /// Verify a signature against this keypair's public key.
    pub fn verify(
        &self,
        message: &[u8],
        signature: &Signature,
    ) -> Result<(), ed25519_dalek::SignatureError> {
        self.public.as_verifying_key().verify(message, signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ed25519_dalek::{PUBLIC_KEY_LENGTH, SIGNATURE_LENGTH};

    #[test]
    fn generate_produces_32_byte_public_key() {
        let keypair = Keypair::generate();
        assert_eq!(keypair.public().to_bytes().len(), PUBLIC_KEY_LENGTH);
    }

    #[test]
    fn generated_keypair_signs_and_verifies_a_known_message() {
        let keypair = Keypair::generate();
        let message = b"reeve walking skeleton";
        let signature = keypair.private().sign(message);
        assert_eq!(signature.to_bytes().len(), SIGNATURE_LENGTH);
        keypair.verify(message, &signature).unwrap();
    }

    #[test]
    fn verification_fails_on_tampered_message() {
        let keypair = Keypair::generate();
        let signature = keypair.private().sign(b"original");
        assert!(keypair.verify(b"tampered", &signature).is_err());
    }

    #[test]
    fn verification_fails_with_a_different_keypair() {
        let signer = Keypair::generate();
        let other = Keypair::generate();
        let message = b"cross-keypair";
        let signature = signer.private().sign(message);
        assert!(other.verify(message, &signature).is_err());
    }

    #[test]
    fn two_consecutive_generations_produce_distinct_keys() {
        let a = Keypair::generate();
        let b = Keypair::generate();
        assert_ne!(a.public().to_bytes(), b.public().to_bytes());
        assert_ne!(*a.private().to_seed_bytes(), *b.private().to_seed_bytes());
    }

    #[test]
    fn private_key_round_trips_through_seed_bytes() {
        let keypair = Keypair::generate();
        let seed = keypair.private().to_seed_bytes();
        let restored = PrivateKey::from_seed_bytes(&seed);

        let message = b"round trip";
        let original = keypair.private().sign(message);
        let restored_sig = restored.sign(message);
        assert_eq!(original.to_bytes(), restored_sig.to_bytes());
    }

    #[test]
    fn private_key_debug_does_not_leak_seed() {
        let keypair = Keypair::generate();
        let rendered = format!("{:?}", keypair.private());
        let seed = keypair.private().to_seed_bytes();
        let hex_seed = seed
            .iter()
            .fold(String::with_capacity(seed.len() * 2), |mut acc, byte| {
                use std::fmt::Write as _;
                let _ = write!(acc, "{byte:02x}");
                acc
            });
        assert!(!rendered.contains(&hex_seed));
    }

    #[test]
    fn into_parts_yields_private_and_public() {
        let keypair = Keypair::generate();
        let public_bytes = keypair.public().to_bytes();
        let (private, public) = keypair.into_parts();
        assert_eq!(public.to_bytes(), public_bytes);
        let signature = private.sign(b"into_parts");
        public
            .as_verifying_key()
            .verify(b"into_parts", &signature)
            .unwrap();
    }
}
