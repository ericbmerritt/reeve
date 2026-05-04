//! Linux implementation of [`OperatorKeyStore`] backed by the freedesktop
//! Secret Service via `secret-service`'s blocking interface.
//!
//! The Secret Service API is provided on the session bus by
//! `gnome-keyring-daemon`, KWallet, or another compatible daemon. If no such
//! daemon is reachable, every method returns
//! [`KeychainError::SecretServiceUnavailable`] — typical headless CI does not
//! run one, which is why higher-layer tests inject [`MemoryKeyStore`].
//!
//! Items are stored in the operator's default collection (alias `default`,
//! which is the unlocked login keyring on most setups). Each item is keyed
//! by a `("service", "reeve")` plus `("identity_id", "<uuid>")` attribute
//! pair. The item label is human-readable (`"reeve operator key for
//! <identity-id>"`) but identity is matched by attributes, never by label.
//!
//! [`MemoryKeyStore`]: crate::keychain::memory::MemoryKeyStore

#![cfg(all(unix, not(target_os = "macos")))]

use std::collections::HashMap;

use reeve_types::IdentityId;
use secret_service::blocking::{Collection, Item, SecretService};
use secret_service::EncryptionType;
use zeroize::Zeroizing;

use super::{KeychainError, OperatorKeyStore, KEYCHAIN_SERVICE, SEED_LEN};

/// Disambiguates Reeve entries from anything else in the operator's default
/// keyring when paired with [`ATTR_IDENTITY_ID`].
const ATTR_SERVICE: &str = "service";
/// Attribute key naming the per-identity disambiguator.
const ATTR_IDENTITY_ID: &str = "identity_id";
/// The seed is raw bytes, not text.
const CONTENT_TYPE: &str = "application/octet-stream";
/// Number of attributes stored per keychain item. Used to pre-size the map
/// without a magic literal at each call site.
const ATTR_COUNT: usize = 2;

/// Secret Service-backed operator key store.
///
/// Connects to the session bus on construction. The underlying connection
/// is reused across method calls.
pub struct SecretServiceKeyStore {
    service_attr: String,
    secret_service: SecretService<'static>,
}

impl std::fmt::Debug for SecretServiceKeyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretServiceKeyStore")
            .field("service_attr", &self.service_attr)
            .finish_non_exhaustive()
    }
}

impl SecretServiceKeyStore {
    /// Connect to the Secret Service on the session bus, using
    /// `KEYCHAIN_SERVICE` as the service attribute.
    pub fn connect() -> Result<Self, KeychainError> {
        Self::connect_with_service(KEYCHAIN_SERVICE.to_owned())
    }

    /// Connect using a caller-provided service attribute. Intended solely
    /// for live tests that need entry isolation; production callers must
    /// use [`SecretServiceKeyStore::connect`].
    #[doc(hidden)]
    pub fn connect_with_service(service: impl Into<String>) -> Result<Self, KeychainError> {
        let secret_service = SecretService::connect(EncryptionType::Dh)
            .map_err(|source| KeychainError::SecretServiceUnavailable { source })?;
        Ok(Self {
            service_attr: service.into(),
            secret_service,
        })
    }

    fn attributes<'a>(&'a self, id_str: &'a str) -> HashMap<&'a str, &'a str> {
        let mut attrs = HashMap::with_capacity(ATTR_COUNT);
        attrs.insert(ATTR_SERVICE, self.service_attr.as_str());
        attrs.insert(ATTR_IDENTITY_ID, id_str);
        attrs
    }

    /// Resolve and unlock the default collection. Every operation on the
    /// store routes through here so the unlock dance is in one place.
    fn unlocked_default_collection(
        &self,
        identity_id: IdentityId,
    ) -> Result<Collection<'_>, KeychainError> {
        let collection = self
            .secret_service
            .get_default_collection()
            .map_err(|source| KeychainError::SecretService {
                identity_id,
                source,
            })?;
        if collection
            .is_locked()
            .map_err(|source| KeychainError::SecretService {
                identity_id,
                source,
            })?
        {
            collection
                .unlock()
                .map_err(|source| KeychainError::SecretService {
                    identity_id,
                    source,
                })?;
        }
        Ok(collection)
    }

    /// Find the existing item for `identity_id` if any. Returns `Ok(None)`
    /// when no entry exists.
    ///
    /// Returns [`KeychainError::DuplicateEntry`] when more than one item matches
    /// `(service, identity_id)`. Not all Secret Service implementations honour
    /// `replace=true` as atomic across daemon restarts or concurrent writers —
    /// this guard surfaces the corruption rather than silently returning the
    /// wrong key.
    fn find_item(&self, identity_id: IdentityId) -> Result<Option<Item<'_>>, KeychainError> {
        let id_str = identity_id.to_string();
        let attrs = self.attributes(&id_str);
        let collection = self.unlocked_default_collection(identity_id)?;
        let items =
            collection
                .search_items(attrs)
                .map_err(|source| KeychainError::SecretService {
                    identity_id,
                    source,
                })?;
        match items.len() {
            0 => Ok(None),
            1 => Ok(Some(items.into_iter().next().unwrap())),
            n => Err(KeychainError::DuplicateEntry {
                identity_id,
                count: n,
            }),
        }
    }
}

impl OperatorKeyStore for SecretServiceKeyStore {
    fn store(
        &self,
        identity_id: IdentityId,
        seed: &Zeroizing<[u8; SEED_LEN]>,
    ) -> Result<(), KeychainError> {
        let id_str = identity_id.to_string();
        let attrs = self.attributes(&id_str);
        let label = format!("reeve operator key for {identity_id}");
        let collection = self.unlocked_default_collection(identity_id)?;
        // `replace = true` makes create_item overwrite an existing entry
        // with matching attributes, which gives us idempotent store
        // semantics across rotations.
        collection
            .create_item(&label, attrs, seed.as_slice(), true, CONTENT_TYPE)
            .map(|_| ())
            .map_err(|source| KeychainError::SecretService {
                identity_id,
                source,
            })
    }

    fn retrieve(
        &self,
        identity_id: IdentityId,
    ) -> Result<Zeroizing<[u8; SEED_LEN]>, KeychainError> {
        let item = self
            .find_item(identity_id)?
            .ok_or(KeychainError::NotFound { identity_id })?;
        if item
            .is_locked()
            .map_err(|source| KeychainError::SecretService {
                identity_id,
                source,
            })?
        {
            item.unlock()
                .map_err(|source| KeychainError::SecretService {
                    identity_id,
                    source,
                })?;
        }
        let bytes =
            Zeroizing::new(
                item.get_secret()
                    .map_err(|source| KeychainError::SecretService {
                        identity_id,
                        source,
                    })?,
            );
        super::decode_seed_bytes(identity_id, &bytes)
    }

    fn delete(&self, identity_id: IdentityId) -> Result<(), KeychainError> {
        let item = self
            .find_item(identity_id)?
            .ok_or(KeychainError::NotFound { identity_id })?;
        item.delete()
            .map_err(|source| KeychainError::SecretService {
                identity_id,
                source,
            })
    }
}

#[cfg(test)]
mod tests {
    //! Live Secret Service tests. Each test uses a unique service attribute
    //! so concurrent runs and previous-run residue do not collide, and
    //! cleans up via `delete` at the end of the test (and on early-return
    //! paths via a scope guard).
    //!
    //! Gated behind `REEVE_KEYCHAIN_LIVE_TESTS=1` so standard `cargo test`
    //! does not require a running gnome-keyring-daemon. CI runs the
    //! trait-contract tests against `MemoryKeyStore` only.

    use super::*;

    use crate::keychain::test_helpers::{live_tests_enabled, unique_service, DeleteOnDrop};
    use crate::keychain::tests::run_contract_suite;

    #[test]
    fn live_contract_suite_when_enabled() {
        if !live_tests_enabled() {
            return;
        }
        let store =
            SecretServiceKeyStore::connect_with_service(unique_service("contract")).unwrap();
        run_contract_suite(&store);
    }

    #[test]
    fn live_round_trip_when_enabled() {
        if !live_tests_enabled() {
            return;
        }
        let store =
            SecretServiceKeyStore::connect_with_service(unique_service("round_trip")).unwrap();
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
        let store =
            SecretServiceKeyStore::connect_with_service(unique_service("not_found")).unwrap();
        let id = IdentityId::new().unwrap();
        let err = store.retrieve(id).unwrap_err();
        assert!(matches!(err, KeychainError::NotFound { .. }));
    }

    #[test]
    fn live_delete_then_retrieve_when_enabled() {
        if !live_tests_enabled() {
            return;
        }
        let store = SecretServiceKeyStore::connect_with_service(unique_service("delete")).unwrap();
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
        let store =
            SecretServiceKeyStore::connect_with_service(unique_service("invalid_len")).unwrap();
        let id = IdentityId::new().unwrap();
        let id_str = id.to_string();

        // Build the same attribute map the store would use.
        let mut attrs = HashMap::new();
        attrs.insert(ATTR_SERVICE, store.service_attr.as_str());
        attrs.insert(ATTR_IDENTITY_ID, id_str.as_str());
        let label = format!("reeve test invalid_len for {id}");

        let collection = store.unlocked_default_collection(id).unwrap();
        collection
            .create_item(&label, attrs, &[0u8; 7], true, CONTENT_TYPE)
            .expect("create_item (7 bytes) failed");

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

    /// Verifies the `DuplicateEntry` guard in `find_item` — triggers duplicates
    /// via `replace=false` because conformant daemons may enforce uniqueness
    /// regardless.
    #[test]
    fn live_duplicate_entry_detected_when_enabled() {
        if !live_tests_enabled() {
            return;
        }
        let store =
            SecretServiceKeyStore::connect_with_service(unique_service("duplicate")).unwrap();
        let id = IdentityId::new().unwrap();
        let id_str = id.to_string();

        let mut attrs = HashMap::new();
        attrs.insert(ATTR_SERVICE, store.service_attr.as_str());
        attrs.insert(ATTR_IDENTITY_ID, id_str.as_str());

        let collection = store.unlocked_default_collection(id).unwrap();
        let label = format!("reeve test duplicate for {id}");

        // DeleteOnDrop calls store.delete → find_item → errors on duplicates.
        // Bypass: search the collection directly and delete each item.
        struct AllItemsGuard<'a> {
            collection: &'a Collection<'a>,
            service_attr: &'a str,
            id_str: &'a str,
        }
        impl Drop for AllItemsGuard<'_> {
            fn drop(&mut self) {
                let mut attrs = HashMap::new();
                attrs.insert(ATTR_SERVICE, self.service_attr);
                attrs.insert(ATTR_IDENTITY_ID, self.id_str);
                if let Ok(items) = self.collection.search_items(attrs) {
                    for item in items {
                        let _ = item.delete();
                    }
                }
            }
        }

        // First item (replace=true — same as the public API).
        collection
            .create_item(
                &label,
                attrs.clone(),
                &[0xAA_u8; SEED_LEN],
                true,
                CONTENT_TYPE,
            )
            .expect("first create_item failed");

        let _cleanup = AllItemsGuard {
            collection: &collection,
            service_attr: store.service_attr.as_str(),
            id_str: id_str.as_str(),
        };

        // Second item with replace=false to force a duplicate.
        let second = collection.create_item(
            &label,
            attrs.clone(),
            &[0xBB_u8; SEED_LEN],
            false,
            CONTENT_TYPE,
        );
        if second.is_err() {
            // The daemon enforces uniqueness regardless of the flag —
            // DuplicateEntry guard is defensive only on this Secret Service
            // implementation.
            eprintln!(
                "live_duplicate_entry_detected_when_enabled: daemon \
                 rejected second create_item — DuplicateEntry guard is \
                 defensive only on this Secret Service implementation"
            );
            return;
        }

        let result = store.retrieve(id);
        assert!(
            matches!(result, Err(KeychainError::DuplicateEntry { count: 2, .. })),
            "expected DuplicateEntry {{ count: 2 }}, got {result:?}",
        );
    }
}
