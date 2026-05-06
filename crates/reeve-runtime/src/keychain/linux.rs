//! Linux implementation of [`OperatorKeyStore`] and [`OperatorSecretStore`]
//! backed by the freedesktop Secret Service via `secret-service`'s blocking
//! interface.
//!
//! The Secret Service API is provided on the session bus by
//! `gnome-keyring-daemon`, `KWallet`, or another compatible daemon. If no such
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
//! Label-keyed secret items use `("service", "reeve")` plus `("label", "<label>")`.
//! Because identity items use `ATTR_IDENTITY_ID` and secret items use
//! `ATTR_LABEL`, the two namespaces cannot collide via attribute search even
//! when a label string looks like a UUID.
//!
//! [`MemoryKeyStore`]: crate::keychain::memory::MemoryKeyStore

#![cfg(all(unix, not(target_os = "macos")))]

use std::collections::HashMap;

use reeve_types::IdentityId;
use secrecy::SecretString;
use secret_service::blocking::{Collection, Item, SecretService};
use secret_service::EncryptionType;
use zeroize::Zeroizing;

use super::{KeychainError, OperatorKeyStore, OperatorSecretStore, KEYCHAIN_SERVICE, SEED_LEN};

/// Disambiguates Reeve entries from anything else in the operator's default
/// keyring when paired with [`ATTR_IDENTITY_ID`].
const ATTR_SERVICE: &str = "service";
/// Attribute key naming the per-identity disambiguator.
const ATTR_IDENTITY_ID: &str = "identity_id";
/// Attribute key naming the label for secret-store items.
const ATTR_LABEL: &str = "label";
/// The seed is raw bytes, not text.
const CONTENT_TYPE: &str = "application/octet-stream";

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
        let mut attrs = HashMap::with_capacity(2);
        attrs.insert(ATTR_SERVICE, self.service_attr.as_str());
        attrs.insert(ATTR_IDENTITY_ID, id_str);
        attrs
    }

    fn secret_attributes<'a>(&'a self, label: &'a str) -> HashMap<&'a str, &'a str> {
        let mut attrs = HashMap::with_capacity(2);
        attrs.insert(ATTR_SERVICE, self.service_attr.as_str());
        attrs.insert(ATTR_LABEL, label);
        attrs
    }

    /// Resolve and unlock the default collection, mapping any Secret Service
    /// error through `map_err`. All store operations route through here so
    /// the unlock dance lives in one place.
    fn unlocked_default_collection_with_error<F>(
        &self,
        map_err: F,
    ) -> Result<Collection<'_>, KeychainError>
    where
        F: Fn(secret_service::Error) -> KeychainError,
    {
        let collection = self
            .secret_service
            .get_default_collection()
            .map_err(&map_err)?;
        if collection.is_locked().map_err(&map_err)? {
            collection.unlock().map_err(&map_err)?;
        }
        Ok(collection)
    }

    /// Resolve and unlock the default collection for identity-keyed operations.
    fn unlocked_default_collection(
        &self,
        identity_id: IdentityId,
    ) -> Result<Collection<'_>, KeychainError> {
        self.unlocked_default_collection_with_error(|source| KeychainError::SecretService {
            identity_id,
            source,
        })
    }

    /// Resolve and unlock the default collection for label-keyed operations.
    fn unlocked_default_collection_for_label(
        &self,
        label: &str,
    ) -> Result<Collection<'_>, KeychainError> {
        let label = label.to_owned();
        self.unlocked_default_collection_with_error(move |source| {
            KeychainError::SecretServiceForLabel {
                label: label.clone(),
                source,
            }
        })
    }

    /// Find the existing item for `identity_id` if any. Returns `Ok(None)`
    /// when no entry exists.
    ///
    /// Returns [`KeychainError::DuplicateEntry`] when more than one item matches
    /// `(service, identity_id)`. Not all Secret Service implementations honour
    /// `replace=true` as atomic across daemon restarts or concurrent writers —
    /// this guard surfaces the corruption rather than silently returning the
    /// wrong key.
    ///
    /// The caller owns the [`Collection`] so that the returned [`Item`] (which
    /// borrows from it) outlives this call — the borrow checker cannot infer
    /// that the inner [`SecretService<'static>`] would keep both alive.
    fn find_item<'c>(
        &self,
        collection: &'c Collection<'_>,
        identity_id: IdentityId,
    ) -> Result<Option<Item<'c>>, KeychainError> {
        let id_str = identity_id.to_string();
        let attrs = self.attributes(&id_str);
        let items =
            collection
                .search_items(attrs)
                .map_err(|source| KeychainError::SecretService {
                    identity_id,
                    source,
                })?;
        match items.len() {
            0 => Ok(None),
            // len() == 1 invariant: into_iter().next() yields Some(item).
            1 => Ok(items.into_iter().next()),
            n => Err(KeychainError::DuplicateEntry {
                identity_id,
                count: n,
            }),
        }
    }

    /// Find the existing item for `label` if any. Returns `Ok(None)` when no
    /// entry exists.
    ///
    /// `replace=true` is passed on `create_item`, which is best-effort on
    /// most Secret Service implementations. Conformant daemons (gnome-keyring,
    /// `KWallet`) honour it, but the guarantee is not part of the specification.
    /// Unlike `find_item` (which guards identity items against duplicates),
    /// this method returns the first matching item; a future hardening pass
    /// may add a `DuplicateSecretEntry` guard here.
    ///
    /// Caller owns the [`Collection`] for the same borrow-lifetime reason as
    /// [`find_item`].
    fn find_secret_item<'c>(
        &self,
        collection: &'c Collection<'_>,
        label: &str,
    ) -> Result<Option<Item<'c>>, KeychainError> {
        let attrs = self.secret_attributes(label);
        let items = collection.search_items(attrs).map_err(|source| {
            KeychainError::SecretServiceForLabel {
                label: label.to_owned(),
                source,
            }
        })?;
        // TODO(phase-5+): Add a duplicate-entry guard for the secret path,
        // parallel to find_item's DuplicateEntry guard. The replace=true flag
        // on create_item is best-effort, not specification-mandated.
        Ok(items.into_iter().next())
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
        let collection = self.unlocked_default_collection(identity_id)?;
        let item = self
            .find_item(&collection, identity_id)?
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
        let collection = self.unlocked_default_collection(identity_id)?;
        let item = self
            .find_item(&collection, identity_id)?
            .ok_or(KeychainError::NotFound { identity_id })?;
        item.delete()
            .map_err(|source| KeychainError::SecretService {
                identity_id,
                source,
            })
    }
}

impl OperatorSecretStore for SecretServiceKeyStore {
    fn store_secret(&self, label: &str, secret: SecretString) -> Result<(), KeychainError> {
        use secrecy::ExposeSecret as _;
        let attrs = self.secret_attributes(label);
        let item_label = format!("reeve secret for {label}");
        let collection = self.unlocked_default_collection_for_label(label)?;
        collection
            .create_item(
                &item_label,
                attrs,
                secret.expose_secret().as_bytes(),
                true,
                "text/plain",
            )
            .map(|_| ())
            .map_err(|source| KeychainError::SecretServiceForLabel {
                label: label.to_owned(),
                source,
            })
    }

    fn retrieve_secret(&self, label: &str) -> Result<SecretString, KeychainError> {
        let collection = self.unlocked_default_collection_for_label(label)?;
        let item = self.find_secret_item(&collection, label)?.ok_or_else(|| {
            KeychainError::SecretNotFound {
                label: label.to_owned(),
            }
        })?;
        if item
            .is_locked()
            .map_err(|source| KeychainError::SecretServiceForLabel {
                label: label.to_owned(),
                source,
            })?
        {
            item.unlock()
                .map_err(|source| KeychainError::SecretServiceForLabel {
                    label: label.to_owned(),
                    source,
                })?;
        }
        // Wrap raw bytes in Zeroizing so the buffer is wiped on drop even
        // if the UTF-8 check fails. Use from_utf8 (borrow) rather than
        // String::from_utf8 (consume) so `bytes` stays alive for the drop.
        let bytes = Zeroizing::new(item.get_secret().map_err(|source| {
            KeychainError::SecretServiceForLabel {
                label: label.to_owned(),
                source,
            }
        })?);
        let s = std::str::from_utf8(&bytes).map_err(|_| KeychainError::InvalidSecretEncoding {
            label: label.to_owned(),
        })?;
        // One small intermediate String allocation between the Zeroizing drop
        // and the SecretString wrap is unavoidable without unsafe; this is the
        // same trade-off secrecy uses internally.
        Ok(SecretString::from(s.to_owned()))
    }

    fn delete_secret(&self, label: &str) -> Result<(), KeychainError> {
        let collection = self.unlocked_default_collection_for_label(label)?;
        let item = self.find_secret_item(&collection, label)?.ok_or_else(|| {
            KeychainError::SecretNotFound {
                label: label.to_owned(),
            }
        })?;
        item.delete()
            .map_err(|source| KeychainError::SecretServiceForLabel {
                label: label.to_owned(),
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

    use crate::keychain::test_helpers::run_secret_contract_suite;
    use crate::keychain::test_helpers::{live_tests_enabled, unique_service, DeleteOnDrop};
    use crate::keychain::tests::run_contract_suite;

    /// Cleanup guard for tests that create multiple items with the same
    /// `(service, identity_id)` attributes — `DeleteOnDrop` cannot be used
    /// because it calls `store.delete` -> `find_item`, which errors on
    /// duplicates.
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

    #[test]
    fn live_secret_contract_suite_when_enabled() {
        if !live_tests_enabled() {
            return;
        }
        let store =
            SecretServiceKeyStore::connect_with_service(unique_service("secret_contract")).unwrap();
        run_secret_contract_suite(&store);
    }
}
