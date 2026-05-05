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
use secrecy::SecretString;
use zeroize::Zeroizing;

use super::{KeychainError, OperatorKeyStore, OperatorSecretStore, SEED_LEN};

/// In-memory operator key store. Seeds are wrapped in [`Zeroizing`] so the
/// backing buffer is wiped when an entry is removed or the store is dropped.
///
/// The store is `Send + Sync`: the inner map sits behind a [`Mutex`] so the
/// trait's `&self` methods can mutate the underlying map.
///
/// The identity seed store and the labeled secret store are fully independent:
/// they use separate maps and cannot cross-contaminate.
///
/// **Test-only.** `MemoryKeyStore` is intended for unit tests that need
/// [`OperatorSecretStore`](super::OperatorSecretStore) semantics without
/// touching the OS credential store. **Do NOT use in any process that holds
/// real secrets in other stores during the same session** — the intermediate
/// `String` from `expose_secret().to_owned()` in `retrieve_secret` is not
/// zeroized before being moved into the returned `SecretString`. A debugger
/// or crash dump could recover that allocation. Production code paths MUST
/// use `MacOsKeyStore` / `SecretServiceKeyStore` instead.
#[derive(Debug, Default)]
pub struct MemoryKeyStore {
    entries: Mutex<HashMap<IdentityId, Zeroizing<[u8; SEED_LEN]>>>,
    secrets: Mutex<HashMap<String, SecretString>>,
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

impl OperatorSecretStore for MemoryKeyStore {
    fn store_secret(&self, label: &str, secret: SecretString) -> Result<(), KeychainError> {
        let mut guard = self
            .secrets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.insert(label.to_owned(), secret);
        Ok(())
    }

    fn retrieve_secret(&self, label: &str) -> Result<SecretString, KeychainError> {
        use secrecy::ExposeSecret as _;
        let guard = self
            .secrets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The intermediate String from `to_owned()` is not independently
        // zeroized before being consumed by `SecretString::from`. This is
        // consistent with how `secrecy` itself works internally and is
        // acceptable for a test-only backend whose secrets never touch the OS
        // credential store.
        guard
            .get(label)
            .map(|s| SecretString::from(s.expose_secret().to_owned()))
            .ok_or_else(|| KeychainError::SecretNotFound {
                label: label.to_owned(),
            })
    }

    fn delete_secret(&self, label: &str) -> Result<(), KeychainError> {
        let mut guard = self
            .secrets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard
            .remove(label)
            .ok_or_else(|| KeychainError::SecretNotFound {
                label: label.to_owned(),
            })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret as _;

    use super::*;
    use crate::keychain::labels;

    /// Sentinel value used in secret-store tests. Deliberately not shaped like
    /// a real key so test output cannot be mistaken for a live credential.
    const SENTINEL: &str = "test-secret-sentinel-value-not-a-real-key";

    // K_ prefix marks keychain contract tests for secret-store operations,
    // mirroring the run_contract_suite naming convention used for key-store
    // tests. The prefix makes them easy to grep together across modules.

    /// `K_secret_store_round_trip`: store then retrieve yields the same value.
    #[test]
    fn k_secret_store_round_trip() {
        let store = MemoryKeyStore::new();
        store
            .store_secret("test-label", SecretString::from(SENTINEL.to_owned()))
            .expect("store_secret failed");
        let retrieved = store
            .retrieve_secret("test-label")
            .expect("retrieve_secret failed");
        assert_eq!(retrieved.expose_secret(), SENTINEL);
    }

    /// `K_secret_retrieve_not_found`: retrieve on an absent label returns
    /// `SecretNotFound`.
    #[test]
    fn k_secret_retrieve_not_found() {
        let store = MemoryKeyStore::new();
        let err = store.retrieve_secret("nonexistent").unwrap_err();
        assert!(
            matches!(err, KeychainError::SecretNotFound { ref label } if label == "nonexistent"),
            "expected SecretNotFound {{ label: \"nonexistent\" }}, got {err:?}",
        );
    }

    /// `K_secret_delete_not_found`: delete on an absent label returns
    /// `SecretNotFound`.
    #[test]
    fn k_secret_delete_not_found() {
        let store = MemoryKeyStore::new();
        let err = store.delete_secret("nonexistent").unwrap_err();
        assert!(
            matches!(err, KeychainError::SecretNotFound { ref label } if label == "nonexistent"),
            "expected SecretNotFound {{ label: \"nonexistent\" }}, got {err:?}",
        );
    }

    /// `K_secret_store_delete_retrieve`: store then delete, then retrieve
    /// returns `SecretNotFound` — not a stale value or a panic.
    #[test]
    fn k_secret_store_delete_retrieve() {
        let store = MemoryKeyStore::new();
        store
            .store_secret("ephemeral", SecretString::from(SENTINEL.to_owned()))
            .expect("store_secret failed");
        store
            .delete_secret("ephemeral")
            .expect("delete_secret failed");
        let err = store
            .retrieve_secret("ephemeral")
            .expect_err("expected SecretNotFound after delete");
        assert!(
            matches!(err, KeychainError::SecretNotFound { ref label } if label == "ephemeral"),
            "expected SecretNotFound {{ label: \"ephemeral\" }}, got {err:?}",
        );
    }

    /// `K_secret_store_replaces_existing`: the second store under the same label
    /// wins.
    #[test]
    fn k_secret_store_replaces_existing() {
        let store = MemoryKeyStore::new();
        store
            .store_secret("test-label", SecretString::from("first".to_owned()))
            .expect("first store_secret failed");
        store
            .store_secret("test-label", SecretString::from("second".to_owned()))
            .expect("second store_secret failed");
        let retrieved = store
            .retrieve_secret("test-label")
            .expect("retrieve_secret failed");
        assert_eq!(retrieved.expose_secret(), "second");
    }

    /// `K_secret_isolated_from_identity_store`: a labeled secret and an identity
    /// seed stored under overlapping names do not cross-contaminate.
    #[test]
    fn k_secret_isolated_from_identity_store() {
        use reeve_types::IdentityId;
        use zeroize::Zeroizing;

        use crate::keychain::SEED_LEN;

        let store = MemoryKeyStore::new();

        // Store a secret under the label "foo".
        store
            .store_secret("foo", SecretString::from(SENTINEL.to_owned()))
            .expect("store_secret failed");

        // Store an identity seed for a fresh identity.
        let id = IdentityId::new().expect("IdentityId::new failed");
        let seed = Zeroizing::new([0xAB_u8; SEED_LEN]);
        store.store(id, &seed).expect("store identity seed failed");

        // The secret is still retrievable and unchanged.
        let retrieved_secret = store
            .retrieve_secret("foo")
            .expect("retrieve_secret failed");
        assert_eq!(retrieved_secret.expose_secret(), SENTINEL);

        // The identity seed is still retrievable and unchanged.
        let retrieved_seed = store.retrieve(id).expect("retrieve identity seed failed");
        assert_eq!(*retrieved_seed, *seed);

        // Deleting the secret does not touch the identity store.
        store.delete_secret("foo").expect("delete_secret failed");
        assert!(
            store.retrieve(id).is_ok(),
            "identity seed was unexpectedly removed after delete_secret",
        );

        // Deleting the identity does not touch the secret store (already gone;
        // verify that retrieve_secret returns SecretNotFound, not a panic).
        store.delete(id).expect("delete identity seed failed");
        let secret_err = store.retrieve_secret("foo").unwrap_err();
        assert!(
            matches!(secret_err, KeychainError::SecretNotFound { .. }),
            "expected SecretNotFound after delete, got {secret_err:?}",
        );
    }

    /// `K_secret_no_log_leak`: the `Debug` and `Display` representations of
    /// `SecretNotFound` contain the label (non-secret) but NOT the sentinel
    /// secret value.
    #[test]
    fn k_secret_no_log_leak() {
        let err = KeychainError::SecretNotFound {
            label: "test-label".to_owned(),
        };
        let debug_output = format!("{err:?}");
        let display_output = format!("{err}");

        // The label (non-secret service identifier) appears in output.
        assert!(
            debug_output.contains("test-label"),
            "Debug output missing label: {debug_output}",
        );
        assert!(
            display_output.contains("test-label"),
            "Display output missing label: {display_output}",
        );

        // The sentinel secret value must NOT appear in error output.
        assert!(
            !debug_output.contains(SENTINEL),
            "Debug output leaks secret value: {debug_output}",
        );
        assert!(
            !display_output.contains(SENTINEL),
            "Display output leaks secret value: {display_output}",
        );
    }

    /// `K_anthropic_label_constant`: pin the wire-level label name so it
    /// cannot drift silently across refactors.
    #[test]
    fn k_anthropic_label_constant() {
        assert_eq!(labels::ANTHROPIC_API_KEY, "reeve-anthropic-api-key");
    }
}
