//! macOS implementation of [`OperatorKeyStore`] and [`OperatorSecretStore`]
//! backed by Apple's `Security.framework` generic-password API.
//!
//! On macOS the operator's per-user login keychain is the credential vault.
//! Each entry is a generic-password keyed on `(service, account)` where
//! `service` is [`KEYCHAIN_SERVICE`] (`"reeve"`).
//!
//! Identity entries use `account = <uuid>` (the hyphenated `IdentityId` string).
//! The 32-byte ed25519 seed is stored as the raw password bytes so no encoding
//! round-trip is required.
//!
//! ## Namespace separation
//!
//! macOS uses a `(service, account)` flat namespace. Identity seeds use
//! `account = <uuid>`; labeled secrets use `account = secret:<label>`.
//! The `"secret:"` prefix prevents a label that happens to be a UUID-shaped
//! string from colliding with an identity entry.
//!
//! ## Zeroize discipline
//!
//! The `security-framework` crate's password APIs return owned `Vec<u8>` for
//! retrieved data; this module wraps the returned vec in [`Zeroizing`] so the
//! buffer is wiped on drop. The crate may copy bytes internally — that is
//! outside this module's control — but every buffer this module owns is
//! zeroed.

#![cfg(target_os = "macos")]

use reeve_types::IdentityId;
use secrecy::SecretString;
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use zeroize::Zeroizing;

use super::{KeychainError, OperatorKeyStore, OperatorSecretStore, KEYCHAIN_SERVICE, SEED_LEN};

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

    /// Map the label to its namespaced keychain account string.
    ///
    /// macOS uses a `(service, account)` flat namespace. Identity seeds
    /// use `account = <uuid>`; labeled secrets use `account = secret:<label>`.
    /// The `"secret:"` prefix prevents a label that happens to be a UUID-shape
    /// string from colliding with an identity entry.
    fn account_for_label(label: &str) -> String {
        format!("secret:{label}")
    }

    /// Thin wrapper so identity and secret paths share the same
    /// `set_generic_password` call site without duplicating error mapping.
    fn store_password_raw(
        &self,
        account: &str,
        bytes: &[u8],
    ) -> Result<(), security_framework::base::Error> {
        set_generic_password(&self.service, account, bytes)
    }

    /// Retrieve raw bytes for `account`. Returns `Ok(None)` when
    /// `ERR_SEC_ITEM_NOT_FOUND` is returned by the framework.
    fn get_password_raw(
        &self,
        account: &str,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, security_framework::base::Error> {
        match get_generic_password(&self.service, account) {
            Ok(bytes) => Ok(Some(Zeroizing::new(bytes))),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Delete `account` from the keychain. Returns `Ok(true)` if an entry
    /// was deleted, `Ok(false)` if no entry existed (mapping
    /// `ERR_SEC_ITEM_NOT_FOUND` to a successful absence rather than an
    /// error so callers can distinguish "deleted" from "wasn't there"
    /// without a nested `match`).
    fn delete_password_raw(&self, account: &str) -> Result<bool, security_framework::base::Error> {
        match delete_generic_password(&self.service, account) {
            Ok(()) => Ok(true),
            Err(e) if e.code() == ERR_SEC_ITEM_NOT_FOUND => Ok(false),
            Err(e) => Err(e),
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
        self.store_password_raw(&identity_id.to_string(), seed.as_slice())
            .map_err(|source| KeychainError::MacOsKeychain {
                identity_id,
                source,
            })
    }

    fn retrieve(
        &self,
        identity_id: IdentityId,
    ) -> Result<Zeroizing<[u8; SEED_LEN]>, KeychainError> {
        match self
            .get_password_raw(&identity_id.to_string())
            .map_err(|source| KeychainError::MacOsKeychain {
                identity_id,
                source,
            })? {
            Some(bytes) => super::decode_seed_bytes(identity_id, &bytes),
            None => Err(KeychainError::NotFound { identity_id }),
        }
    }

    fn delete(&self, identity_id: IdentityId) -> Result<(), KeychainError> {
        let deleted = self
            .delete_password_raw(&identity_id.to_string())
            .map_err(|source| KeychainError::MacOsKeychain {
                identity_id,
                source,
            })?;
        if deleted {
            Ok(())
        } else {
            Err(KeychainError::NotFound { identity_id })
        }
    }
}

impl OperatorSecretStore for MacOsKeyStore {
    fn store_secret(&self, label: &str, secret: SecretString) -> Result<(), KeychainError> {
        use secrecy::ExposeSecret as _;
        let account = Self::account_for_label(label);
        self.store_password_raw(&account, secret.expose_secret().as_bytes())
            .map_err(|source| KeychainError::MacOsKeychainForLabel {
                label: label.to_owned(),
                source,
            })
    }

    fn retrieve_secret(&self, label: &str) -> Result<SecretString, KeychainError> {
        let account = Self::account_for_label(label);
        match self.get_password_raw(&account).map_err(|source| {
            KeychainError::MacOsKeychainForLabel {
                label: label.to_owned(),
                source,
            }
        })? {
            None => Err(KeychainError::SecretNotFound {
                label: label.to_owned(),
            }),
            Some(bytes) => {
                // Use from_utf8 (borrow, not consume) so `bytes` stays in
                // scope for the Zeroizing drop to wipe after `to_owned()`
                // produces the heap String for SecretString. One small
                // intermediate String allocation is unavoidable without unsafe;
                // this is the same trade-off secrecy uses internally.
                let s = std::str::from_utf8(&bytes).map_err(|_| {
                    KeychainError::InvalidSecretEncoding {
                        label: label.to_owned(),
                    }
                })?;
                Ok(SecretString::from(s.to_owned()))
            }
        }
    }

    fn delete_secret(&self, label: &str) -> Result<(), KeychainError> {
        let account = Self::account_for_label(label);
        let deleted = self.delete_password_raw(&account).map_err(|source| {
            KeychainError::MacOsKeychainForLabel {
                label: label.to_owned(),
                source,
            }
        })?;
        if deleted {
            Ok(())
        } else {
            Err(KeychainError::SecretNotFound {
                label: label.to_owned(),
            })
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

    use crate::keychain::test_helpers::run_secret_contract_suite;
    use crate::keychain::test_helpers::{
        live_tests_enabled, unique_service, DeleteOnDrop, SecretDeleteOnDrop,
    };
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

    #[test]
    fn live_secret_contract_suite_when_enabled() {
        if !live_tests_enabled() {
            return;
        }
        let store = MacOsKeyStore::with_service(unique_service("secret_contract"));
        run_secret_contract_suite(&store);
    }

    /// Verifies that a label-keyed entry does not collide with an identity
    /// entry even when the label has the same textual shape as a UUID.
    #[test]
    fn live_secret_label_uuid_no_collision_when_enabled() {
        use secrecy::ExposeSecret as _;

        if !live_tests_enabled() {
            return;
        }
        let store = MacOsKeyStore::with_service(unique_service("namespace"));
        let id = IdentityId::new().unwrap();
        // Use the UUID string as the secret label — the worst-case collision.
        let uuid_label = id.to_string();

        let seed = Zeroizing::new([0x77_u8; SEED_LEN]);
        store.store(id, &seed).unwrap();
        let _id_guard = DeleteOnDrop {
            store: &store,
            identity_id: id,
        };

        store
            .store_secret(
                &uuid_label,
                SecretString::from("collision-probe".to_owned()),
            )
            .unwrap();
        let _secret_guard = SecretDeleteOnDrop {
            store: &store,
            label: &uuid_label,
        };

        // Identity entry unaffected.
        let loaded_seed = store.retrieve(id).unwrap();
        assert_eq!(*loaded_seed, *seed);

        // Secret entry is the string, not the seed bytes.
        let loaded_secret = store.retrieve_secret(&uuid_label).unwrap();
        assert_eq!(loaded_secret.expose_secret(), "collision-probe");
    }
}
