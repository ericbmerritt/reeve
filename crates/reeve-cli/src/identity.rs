//! Business logic for `reeve identity enroll` and `reeve identity list`.
//!
//! Stdin interaction lives in `main` only.

use std::io::Write;
use std::path::PathBuf;

use reeve_runtime::{IdentityRegistry, OperatorKeyStore, StoredIdentity};
use reeve_types::{Identity, IdentityIdError, IdentityType, KeyIdError, KeyRecord, Keypair};

use crate::output::write_identity_table;

/// A flattened, borrowing view of one identity for table rendering.
pub(crate) struct IdentityRow<'a> {
    pub(crate) identity_type: IdentityType,
    pub(crate) display_name: &'a str,
    pub(crate) identity_id: &'a str,
    pub(crate) fingerprint: &'a str,
}

/// Error returned when `enroll` cannot proceed.
#[derive(Debug)]
pub(crate) enum EnrollError {
    /// Display name was empty or all-whitespace.
    EmptyDisplayName,
    /// An operator identity already exists on this machine.
    AlreadyEnrolled {
        display_name: String,
        identity_id: String,
    },
    /// Minting a fresh `UUIDv7` identity id failed.
    MintIdentityId(IdentityIdError),
    /// Minting a fresh `UUIDv7` key id failed.
    MintKeyId(KeyIdError),
    /// The registry could not be listed or written.
    Registry(reeve_runtime::RegistryError),
    /// The keychain write failed after the registry write succeeded.
    ///
    /// The registry entry is left in place — the operator must remove the
    /// half-enrolled TOML file at `toml_path` and re-enroll.
    Keychain {
        identity_id: reeve_types::IdentityId,
        toml_path: PathBuf,
        source: reeve_runtime::KeychainError,
    },
}

impl std::fmt::Display for EnrollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyDisplayName => f.write_str("display name must not be empty"),
            Self::AlreadyEnrolled {
                display_name,
                identity_id,
            } => write!(
                f,
                "operator identity already exists: \"{display_name}\" ({identity_id})\n\
                 reeve enforces one operator per machine",
            ),
            Self::MintIdentityId(err) => {
                write!(f, "failed to mint identity id: {err}")
            }
            Self::MintKeyId(err) => {
                write!(f, "failed to mint key id: {err}")
            }
            Self::Registry(err) => write!(f, "identity registry error: {err}"),
            Self::Keychain {
                identity_id,
                toml_path,
                source,
            } => write!(
                f,
                "keychain write failed for identity {identity_id}; \
                 the registry entry at {toml_path} has no corresponding private seed \
                 — remove it and re-enroll: {source}",
                toml_path = toml_path.display(),
            ),
        }
    }
}

impl std::error::Error for EnrollError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registry(err) => Some(err),
            Self::Keychain { source, .. } => Some(source),
            Self::MintIdentityId(err) => Some(err),
            Self::MintKeyId(err) => Some(err),
            Self::EmptyDisplayName | Self::AlreadyEnrolled { .. } => None,
        }
    }
}

impl From<reeve_runtime::RegistryError> for EnrollError {
    fn from(err: reeve_runtime::RegistryError) -> Self {
        Self::Registry(err)
    }
}

/// Scan `stored` for any identity of type [`IdentityType::Operator`] and
/// return it if found. Assumes at most one Operator entry exists; the
/// single-operator-per-machine invariant is enforced by the registry layer,
/// but a `debug_assert` fuses the failure mode in debug builds.
pub(crate) fn find_existing_operator(stored: &[StoredIdentity]) -> Option<&StoredIdentity> {
    let count = stored
        .iter()
        .filter(|s| s.identity().identity_type == IdentityType::Operator)
        .count();
    debug_assert!(
        count <= 1,
        "invariant violation: found {count} Operator entries; expected at most 1",
    );
    stored
        .iter()
        .find(|s| s.identity().identity_type == IdentityType::Operator)
}

/// Enrolls a new operator identity for this workstation.
///
/// Sequence:
///   1. Reject `display_name` if empty.
///   2. Reject if any Operator-type identity already exists in the registry
///      (single-operator-per-machine).
///   3. Generate a fresh ed25519 keypair.
///   4. Write the public-key record to the registry.
///   5. Write the private seed to the keychain.
///
/// If the keychain write fails after the registry write succeeds, returns
/// `EnrollError::Keychain`. The registry entry is left in place; no
/// rollback — `IdentityRegistry::delete` lands in a future task.
///
/// This function assumes a single `reeve identity enroll` invocation at a
/// time. Concurrent invocations can defeat the single-operator-per-machine
/// invariant; a workstation-wide lock will land in a future phase.
pub(crate) fn enroll(
    registry: &IdentityRegistry,
    keychain: &dyn OperatorKeyStore,
    display_name: &str,
) -> Result<StoredIdentity, EnrollError> {
    if display_name.trim().is_empty() {
        return Err(EnrollError::EmptyDisplayName);
    }

    let existing = registry.list()?;
    if let Some(op) = find_existing_operator(&existing) {
        return Err(EnrollError::AlreadyEnrolled {
            display_name: op.identity().display_name.clone(),
            identity_id: op.identity().identity_id.to_string(),
        });
    }

    let keypair = Keypair::generate();
    let (private, public) = keypair.into_parts();

    let identity =
        Identity::new_operator(display_name.to_owned()).map_err(EnrollError::MintIdentityId)?;
    let key_record =
        KeyRecord::new(identity.identity_id, public).map_err(EnrollError::MintKeyId)?;
    let stored = StoredIdentity::new(identity, key_record).map_err(EnrollError::Registry)?;

    registry.write(&stored)?;

    let seed = private.to_seed_bytes();
    if let Err(source) = keychain.store(stored.identity().identity_id, &seed) {
        return Err(EnrollError::Keychain {
            identity_id: stored.identity().identity_id,
            toml_path: registry.toml_path(stored.identity().identity_id),
            source,
        });
    }

    Ok(stored)
}

/// Render all identities in `registry` to `out` as a plain-text table.
pub(crate) fn list(registry: &IdentityRegistry, out: &mut impl Write) -> Result<(), ListError> {
    let stored = registry.list().map_err(ListError::Registry)?;

    let id_strings: Vec<String> = stored
        .iter()
        .map(|s| s.identity().identity_id.to_string())
        .collect();
    let fingerprints: Vec<String> = stored
        .iter()
        .map(|s| {
            s.key_records()
                .first()
                .map(|kr| kr.public_key.fingerprint())
                .unwrap_or_default()
        })
        .collect();

    let rows: Vec<IdentityRow<'_>> = stored
        .iter()
        .zip(id_strings.iter())
        .zip(fingerprints.iter())
        .map(|((s, id_str), fp)| IdentityRow {
            identity_type: s.identity().identity_type,
            display_name: &s.identity().display_name,
            identity_id: id_str.as_str(),
            fingerprint: fp.as_str(),
        })
        .collect();

    write_identity_table(out, &rows).map_err(ListError::Io)
}

/// Error returned when `list` cannot proceed.
#[derive(Debug)]
pub(crate) enum ListError {
    /// The registry could not be read.
    Registry(reeve_runtime::RegistryError),
    /// Writing to the output stream failed.
    Io(std::io::Error),
}

impl std::fmt::Display for ListError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Registry(err) => write!(f, "identity registry error: {err}"),
            Self::Io(err) => write!(f, "output error: {err}"),
        }
    }
}

impl std::error::Error for ListError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registry(err) => Some(err),
            Self::Io(err) => Some(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reeve_runtime::keychain::memory::MemoryKeyStore;
    use reeve_runtime::{KeychainError, OperatorKeyStore, SEED_LEN};
    use reeve_types::{IdentityId, IdentityIdError, IdentityType, KeyIdError};
    use tempfile::tempdir;
    use zeroize::Zeroizing;

    /// Chmod `path` to 0o700 so `IdentityRegistry::open` accepts it.
    ///
    /// `tempdir()` creates directories with mode 0o700 on some platforms and
    /// 0o755 on others (e.g. macOS inside a Nix shell). The registry enforces
    /// 0o700 per the filesystem-safety spec, so tests must explicitly set the
    /// mode before opening. The duplication vs. `reeve-runtime`'s `chmod_secure`
    /// helper is intentional: sharing a `#[cfg(test)]` helper across crate
    /// boundaries requires a feature flag or public API, and the volume of
    /// cross-crate test helpers is not worth that complexity here.
    #[cfg(unix)]
    fn secure_dir(path: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("chmod 0o700 must succeed in tests");
    }

    #[cfg(not(unix))]
    fn secure_dir(_path: &std::path::Path) {}

    fn open_registry(dir: &std::path::Path) -> IdentityRegistry {
        secure_dir(dir);
        IdentityRegistry::open(dir.to_path_buf()).unwrap()
    }

    /// A keychain that fails every operation.
    struct AlwaysFailKeyStore;

    impl OperatorKeyStore for AlwaysFailKeyStore {
        fn store(
            &self,
            identity_id: IdentityId,
            _seed: &Zeroizing<[u8; SEED_LEN]>,
        ) -> Result<(), KeychainError> {
            Err(KeychainError::NotFound { identity_id })
        }

        fn retrieve(
            &self,
            identity_id: IdentityId,
        ) -> Result<Zeroizing<[u8; SEED_LEN]>, KeychainError> {
            Err(KeychainError::NotFound { identity_id })
        }

        fn delete(&self, identity_id: IdentityId) -> Result<(), KeychainError> {
            Err(KeychainError::NotFound { identity_id })
        }
    }

    #[test]
    fn find_existing_operator_empty_returns_none() {
        assert!(find_existing_operator(&[]).is_none());
    }

    #[test]
    fn find_existing_operator_returns_none_when_slice_has_no_operator_type() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        enroll(&registry, &keychain, "agent-test").unwrap();
        let stored = registry.list().unwrap();

        assert!(find_existing_operator(
            &stored
                .iter()
                .filter(|s| s.identity().identity_type != IdentityType::Operator)
                .cloned()
                .collect::<Vec<_>>()
        )
        .is_none());
    }

    #[test]
    fn find_existing_operator_with_operator_returns_some() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        enroll(&registry, &keychain, "Ada").unwrap();
        let stored = registry.list().unwrap();
        let found = find_existing_operator(&stored);
        assert!(found.is_some());
        assert_eq!(found.unwrap().identity().display_name, "Ada");
    }

    #[test]
    fn enroll_empty_display_name_is_rejected() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        let err = enroll(&registry, &keychain, "").unwrap_err();
        assert!(
            matches!(err, EnrollError::EmptyDisplayName),
            "expected EmptyDisplayName, got {err}"
        );

        let err = enroll(&registry, &keychain, "   ").unwrap_err();
        assert!(
            matches!(err, EnrollError::EmptyDisplayName),
            "expected EmptyDisplayName for whitespace-only, got {err}"
        );
    }

    #[test]
    fn enroll_empty_registry_creates_operator_identity() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        let result = enroll(&registry, &keychain, "Ada").unwrap();

        assert_eq!(result.identity().identity_type, IdentityType::Operator);
        assert_eq!(result.identity().display_name, "Ada");

        let listed = registry.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].identity().identity_type, IdentityType::Operator);

        let seed = keychain.retrieve(result.identity().identity_id);
        assert!(seed.is_ok(), "seed must be in keychain after enroll");
    }

    #[test]
    fn enroll_twice_fails_with_already_enrolled() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        enroll(&registry, &keychain, "Ada").unwrap();
        let err = enroll(&registry, &keychain, "Second").unwrap_err();

        assert!(
            matches!(err, EnrollError::AlreadyEnrolled { .. }),
            "expected AlreadyEnrolled, got {err}"
        );
        if let EnrollError::AlreadyEnrolled { display_name, .. } = &err {
            assert_eq!(display_name, "Ada");
        }
    }

    #[test]
    fn enroll_stores_seed_in_keychain() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        let stored = enroll(&registry, &keychain, "Ada").unwrap();
        let identity_id = stored.identity().identity_id;

        let seed = keychain.retrieve(identity_id).unwrap();
        assert_eq!(seed.len(), SEED_LEN);
    }

    #[test]
    fn enroll_keychain_failure_leaves_registry_entry() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = AlwaysFailKeyStore;

        let err = enroll(&registry, &keychain, "Ada").unwrap_err();
        assert!(
            matches!(err, EnrollError::Keychain { .. }),
            "expected Keychain error, got {err}"
        );

        let listed = registry.list().unwrap();
        assert_eq!(
            listed.len(),
            1,
            "registry entry must remain after keychain failure"
        );
    }

    #[test]
    fn enroll_keychain_failure_surfaces_recovery_path() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = AlwaysFailKeyStore;

        let err = enroll(&registry, &keychain, "Ada").unwrap_err();

        let rendered = err.to_string();
        assert!(
            rendered.contains(&dir.path().display().to_string()),
            "error must contain toml_path; got: {rendered}",
        );
        if let EnrollError::Keychain { identity_id, .. } = &err {
            assert!(
                rendered.contains(&identity_id.to_string()),
                "error must contain identity_id; got: {rendered}",
            );
        } else {
            panic!("expected Keychain error");
        }
    }

    #[test]
    fn mint_identity_id_error_propagates_source() {
        use std::error::Error;

        let err = EnrollError::MintIdentityId(IdentityIdError::ClockBeforeEpoch);
        assert!(
            err.source().is_some(),
            "MintIdentityId must surface source via Error::source()"
        );
    }

    #[test]
    fn mint_key_id_error_propagates_source() {
        use std::error::Error;

        let err = EnrollError::MintKeyId(KeyIdError::ClockBeforeEpoch);
        assert!(
            err.source().is_some(),
            "MintKeyId must surface source via Error::source()"
        );
    }

    #[test]
    fn list_empty_registry_outputs_header_only() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let mut buf = Vec::new();

        list(&registry, &mut buf).unwrap();

        let output = String::from_utf8(buf).unwrap();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 1, "empty registry: only header row expected");
        assert!(lines[0].contains("TYPE"));
    }

    #[test]
    fn list_seeded_registry_outputs_all_identities() {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();

        let a = enroll(&registry, &keychain, "Ada").unwrap();
        let mut buf = Vec::new();

        list(&registry, &mut buf).unwrap();

        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("Ada"), "name must appear in list output");
        assert!(
            output.contains(&a.identity().identity_id.to_string()),
            "identity_id must appear in list output"
        );
        assert!(
            output.contains("Operator"),
            "type must appear in list output"
        );
    }

    #[test]
    fn enroll_error_display_contains_context() {
        let err = EnrollError::AlreadyEnrolled {
            display_name: "Ada".to_owned(),
            identity_id: "some-uuid".to_owned(),
        };
        let rendered = err.to_string();
        assert!(rendered.contains("Ada"));
        assert!(rendered.contains("some-uuid"));
        assert!(rendered.contains("one operator per machine"));
    }
}
