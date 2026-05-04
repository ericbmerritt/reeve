//! macOS implementation of [`OperatorKeyStore`] backed by Apple's
//! `Security.framework` generic-password API.
//!
//! On macOS the operator's per-user login keychain is the credential vault.
//! Each entry is a generic-password keyed on `(service, account)` where
//! `service` is [`KEYCHAIN_SERVICE`] (`"reeve"`) and `account` is the
//! hyphenated UUID string for the identity. The 32-byte ed25519 seed is
//! stored as the raw password bytes so no encoding round-trip is required.
//!
//! The `security-framework` crate's password APIs return owned `Vec<u8>` for
//! retrieved data; this module wraps the returned vec in [`Zeroizing`] so the
//! buffer is wiped on drop. The crate may copy bytes internally — that is
//! outside this module's control — but every buffer this module owns is
//! zeroed.

#![cfg(target_os = "macos")]

use reeve_types::IdentityId;
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use zeroize::Zeroizing;

use super::{KeychainError, OperatorKeyStore, KEYCHAIN_SERVICE, SEED_LEN};

/// `ERR_SEC_ITEM_NOT_FOUND` from `Security/SecBase.h`. Hardcoded here so this
/// crate does not need a direct dependency on `security-framework-sys` for
/// a single constant. The value is part of Apple's public `OSStatus` error
/// code namespace and is stable across macOS releases.
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25_300;

/// macOS Security.framework-backed operator key store.
///
/// Stateless: every method call goes straight to Security.framework.
/// `Send + Sync` because the underlying API is thread-safe.
/// `Copy` is not derived: `String` is not `Copy`, and keeping the asymmetry
/// with `SecretServiceKeyStore` (which holds a connection handle) explicit
/// prevents surprising behavior if state is added later.
#[derive(Debug)]
pub struct MacOsKeyStore {
    service: String,
}

impl MacOsKeyStore {
    /// Construct a production store using `KEYCHAIN_SERVICE`.
    pub fn new() -> Self {
        Self {
            service: KEYCHAIN_SERVICE.to_owned(),
        }
    }

    /// Construct a store using a caller-provided service name. Intended
    /// solely for live keychain tests that need entry isolation; production
    /// callers must use [`MacOsKeyStore::new`].
    #[doc(hidden)]
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }
}

// Cannot use #[derive(Default)]: the empty-string default for String is not
// KEYCHAIN_SERVICE.
impl Default for MacOsKeyStore {
    fn default() -> Self {
        Self::new()
    }
}

impl OperatorKeyStore for MacOsKeyStore {
    fn store(
        &self,
        identity_id: IdentityId,
        seed: &Zeroizing<[u8; SEED_LEN]>,
    ) -> Result<(), KeychainError> {
        set_generic_password(&self.service, &identity_id.to_string(), seed.as_slice()).map_err(
            |source| KeychainError::MacOsKeychain {
                identity_id,
                source,
            },
        )
    }

    fn retrieve(
        &self,
        identity_id: IdentityId,
    ) -> Result<Zeroizing<[u8; SEED_LEN]>, KeychainError> {
        match get_generic_password(&self.service, &identity_id.to_string()) {
            Ok(bytes) => {
                // Wiped on drop even if subsequent decoding fails.
                let bytes = Zeroizing::new(bytes);
                super::decode_seed_bytes(identity_id, &bytes)
            }
            Err(source) if source.code() == ERR_SEC_ITEM_NOT_FOUND => {
                Err(KeychainError::NotFound { identity_id })
            }
            Err(source) => Err(KeychainError::MacOsKeychain {
                identity_id,
                source,
            }),
        }
    }

    fn delete(&self, identity_id: IdentityId) -> Result<(), KeychainError> {
        match delete_generic_password(&self.service, &identity_id.to_string()) {
            Ok(()) => Ok(()),
            Err(source) if source.code() == ERR_SEC_ITEM_NOT_FOUND => {
                Err(KeychainError::NotFound { identity_id })
            }
            Err(source) => Err(KeychainError::MacOsKeychain {
                identity_id,
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Live keychain tests. Each test uses a unique service name so it never
    //! touches the operator's real Reeve entry, and cleans up via `delete`
    //! at the end of the test (and on early-return paths via a scope guard).
    //!
    //! Gated behind `REEVE_KEYCHAIN_LIVE_TESTS=1` so standard `cargo test`
    //! does not trigger the macOS keychain prompt or leave orphan entries
    //! when run on a workstation that has not granted Reeve trust. CI runs
    //! the trait-contract tests against `MemoryKeyStore` only.

    use super::*;

    use crate::keychain::test_helpers::{live_tests_enabled, unique_service, DeleteOnDrop};
    use crate::keychain::tests::run_contract_suite;

    #[test]
    fn live_contract_suite_when_enabled() {
        if !live_tests_enabled() {
            return;
        }
        let store = MacOsKeyStore::with_service(unique_service("contract"));
        run_contract_suite(&store);
    }

    #[test]
    fn live_round_trip_when_enabled() {
        if !live_tests_enabled() {
            return;
        }
        let store = MacOsKeyStore::with_service(unique_service("round_trip"));
        let id = IdentityId::new().unwrap();
        let _guard = DeleteOnDrop {
            store: &store,
            identity_id: id,
        };

        let seed = Zeroizing::new([0x42_u8; SEED_LEN]);
        store.store(id, &seed).unwrap();
        let loaded = store.retrieve(id).unwrap();
        assert_eq!(*loaded, *seed);
    }

    #[test]
    fn live_retrieve_missing_is_not_found_when_enabled() {
        if !live_tests_enabled() {
            return;
        }
        let store = MacOsKeyStore::with_service(unique_service("not_found"));
        let id = IdentityId::new().unwrap();
        let err = store.retrieve(id).unwrap_err();
        assert!(matches!(err, KeychainError::NotFound { .. }));
    }

    #[test]
    fn live_delete_then_retrieve_when_enabled() {
        if !live_tests_enabled() {
            return;
        }
        let store = MacOsKeyStore::with_service(unique_service("delete"));
        let id = IdentityId::new().unwrap();
        let _guard = DeleteOnDrop {
            store: &store,
            identity_id: id,
        };

        store
            .store(id, &Zeroizing::new([0x11_u8; SEED_LEN]))
            .unwrap();
        store.delete(id).unwrap();
        assert!(matches!(
            store.retrieve(id).unwrap_err(),
            KeychainError::NotFound { .. },
        ));
    }

    #[test]
    fn live_invalid_seed_length_when_enabled() {
        if !live_tests_enabled() {
            return;
        }
        let svc = unique_service("invalid_len");
        let store = MacOsKeyStore::with_service(&svc);
        let id = IdentityId::new().unwrap();
        let account = id.to_string();

        // Bypass the store API to plant a short secret.
        set_generic_password(&svc, &account, &[0u8; 7])
            .expect("set_generic_password (7 bytes) failed");

        let _guard = DeleteOnDrop {
            store: &store,
            identity_id: id,
        };

        let err = store.retrieve(id).unwrap_err();
        assert!(
            matches!(
                err,
                KeychainError::InvalidSeedLength {
                    identity_id: _,
                    len: 7
                }
            ),
            "expected InvalidSeedLength {{ len: 7 }}, got {err:?}",
        );
    }
}
