//! Process-local in-memory implementation of [`OperatorKeyStore`].
//!
//! Used by trait-contract tests in this crate and as a dependency-injection
//! seam for higher-layer tests. Not suitable for production: seeds are lost
//! on process exit and never reach durable storage. Production code paths
//! must use [`crate::keychain::macos::MacOsKeyStore`] on macOS or
//! [`crate::keychain::linux::SecretServiceKeyStore`] on Linux.

use std::collections::HashMap;
use std::sync::Mutex;

use reeve_types::IdentityId;
use zeroize::Zeroizing;

use super::{KeychainError, OperatorKeyStore, SEED_LEN};

/// In-memory operator key store. Seeds are wrapped in [`Zeroizing`] so the
/// backing buffer is wiped when an entry is removed or the store is dropped.
///
/// The store is `Send + Sync`: the inner map sits behind a [`Mutex`] so the
/// trait's `&self` methods can mutate the underlying map.
#[derive(Debug, Default)]
pub struct MemoryKeyStore {
    entries: Mutex<HashMap<IdentityId, Zeroizing<[u8; SEED_LEN]>>>,
}

impl MemoryKeyStore {
    /// Construct an empty in-memory store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl OperatorKeyStore for MemoryKeyStore {
    fn store(
        &self,
        identity_id: IdentityId,
        seed: &Zeroizing<[u8; SEED_LEN]>,
    ) -> Result<(), KeychainError> {
        // Copy into a fresh Zeroizing so the stored buffer is independently
        // wiped on drop.
        let mut owned = Zeroizing::new([0_u8; SEED_LEN]);
        owned.copy_from_slice(seed.as_slice());
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(identity_id, owned);
        Ok(())
    }

    fn retrieve(
        &self,
        identity_id: IdentityId,
    ) -> Result<Zeroizing<[u8; SEED_LEN]>, KeychainError> {
        let guard = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stored = guard
            .get(&identity_id)
            .ok_or(KeychainError::NotFound { identity_id })?;
        let mut out = Zeroizing::new([0_u8; SEED_LEN]);
        out.copy_from_slice(stored.as_slice());
        Ok(out)
    }

    fn delete(&self, identity_id: IdentityId) -> Result<(), KeychainError> {
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .remove(&identity_id)
            .ok_or(KeychainError::NotFound { identity_id })?;
        Ok(())
    }
}
