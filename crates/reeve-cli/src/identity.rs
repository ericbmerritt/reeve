//! Business logic for `reeve identity enroll`, `reeve identity list`, and
//! `reeve identity unenroll`.
//!
//! Stdin interaction lives in `main` only.

use std::io::Write;
use std::path::PathBuf;

use reeve_runtime::{
    AuditEvent, AuditLog, IdentityRegistry, KeychainError, OperatorKeyStore, StoredIdentity,
};
use reeve_types::{
    Identity, IdentityId, IdentityIdError, IdentityType, KeyIdError, KeyRecord, Keypair,
};

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
        identity_id: IdentityId,
        toml_path: PathBuf,
        source: KeychainError,
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
/// `EnrollError::Keychain`. The registry entry is left in place.
/// `IdentityRegistry::delete` now exists (landed in Task 19), but automatic
/// rollback of a half-enrolled state is deliberately deferred to a follow-on
/// task — `enroll()` does not call `delete()` to roll back. The operator is
/// expected to invoke `reeve identity unenroll --confirm` to clear the slot.
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

/// Error returned when `unenroll` cannot proceed.
#[derive(Debug)]
pub(crate) enum UnenrollError {
    /// `--confirm` flag was not supplied. The operator must re-run with the
    /// flag to acknowledge the destructive operation.
    ConfirmRequired,
    /// The registry contains no operator identity to remove.
    NoOperator,
    /// The registry contains more than one operator identity. This violates
    /// the one-operator-per-machine invariant and requires manual cleanup.
    MultipleOperators,
    /// The registry could not be listed or the entry could not be deleted.
    Registry(reeve_runtime::RegistryError),
    /// The keychain delete failed (not a `NotFound` — those are swallowed).
    Keychain(KeychainError),
    /// The audit log append failed. Surfaces as a warning; the unenrollment
    /// itself already succeeded.
    AuditFailed(reeve_runtime::AuditError),
}

/// Display message for `UnenrollError::ConfirmRequired`.
const MSG_CONFIRM_REQUIRED: &str =
    "Re-run with --confirm to remove the operator identity, its keychain entry, \
     and append an audit record.";
/// Display message for `UnenrollError::NoOperator`.
const MSG_NO_OPERATOR: &str = "No operator identity to unenroll.";
/// Display message for `UnenrollError::MultipleOperators`.
const MSG_MULTIPLE_OPERATORS: &str = "Multiple operator identities found; manual cleanup required.";

impl std::fmt::Display for UnenrollError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfirmRequired => f.write_str(MSG_CONFIRM_REQUIRED),
            Self::NoOperator => f.write_str(MSG_NO_OPERATOR),
            Self::MultipleOperators => f.write_str(MSG_MULTIPLE_OPERATORS),
            Self::Registry(err) => write!(f, "identity registry error: {err}"),
            Self::Keychain(err) => write!(f, "keychain error: {err}"),
            Self::AuditFailed(err) => {
                write!(f, "audit append failed (unenrollment succeeded): {err}")
            }
        }
    }
}

impl std::error::Error for UnenrollError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registry(err) => Some(err),
            Self::Keychain(err) => Some(err),
            Self::AuditFailed(err) => Some(err),
            Self::ConfirmRequired | Self::NoOperator | Self::MultipleOperators => None,
        }
    }
}

impl From<reeve_runtime::RegistryError> for UnenrollError {
    fn from(err: reeve_runtime::RegistryError) -> Self {
        Self::Registry(err)
    }
}

/// Remove the operator identity from the registry and keychain, then append
/// an audit record.
///
/// Requires `confirm == true`; returns `Err(UnenrollError::ConfirmRequired)`
/// otherwise so callers (including tests) get a typed signal rather than a
/// process exit. `--confirm` is a UX guardrail against accidents, NOT a
/// security boundary. Same-UID actors can run this command. A
/// workstation-wide lock will land in a future phase.
///
/// ## Deletion order: registry → keychain → audit
///
/// 1. `registry.delete` — if this fails, no destructive change has occurred;
///    the operator is unaffected and can retry.
/// 2. `keychain.delete` — swallows `KeychainError::NotFound` (idempotent:
///    a missing entry is fine if the operator deleted it manually). Any other
///    keychain error is returned as `UnenrollError::Keychain`. At this point
///    the registry is already empty, so the operator can re-enroll cleanly;
///    an orphan keychain entry remains until cleared.
/// 3. `audit.append` — records the event for forensic purposes. If this
///    fails after both deletions succeeded, returns
///    `Err(UnenrollError::AuditFailed)`. The CLI treats `AuditFailed` as a
///    warning and exits 0 — the destructive operation completed; only the
///    forensic record is missing. This is a deliberate policy.
///
/// ## Error summary
///
/// - `UnenrollError::Registry` — registry list or delete failed; no
///   destructive change occurred.
/// - `UnenrollError::Keychain` — registry deletion committed but keychain
///   delete returned a non-`NotFound` error. The operator can re-enroll.
/// - `UnenrollError::AuditFailed` — both deletions committed; only the
///   audit record is missing. Callers may treat this as a soft failure.
pub(crate) fn unenroll(
    registry: &IdentityRegistry,
    keychain: &dyn OperatorKeyStore,
    audit: &AuditLog,
    confirm: bool,
) -> Result<IdentityId, UnenrollError> {
    if !confirm {
        return Err(UnenrollError::ConfirmRequired);
    }

    let stored = registry.list()?;
    let operators: Vec<_> = stored
        .iter()
        .filter(|s| s.identity().identity_type == IdentityType::Operator)
        .collect();

    let operator_id = match operators.len() {
        0 => return Err(UnenrollError::NoOperator),
        1 => operators[0].identity().identity_id,
        _ => return Err(UnenrollError::MultipleOperators),
    };

    // Step 1: delete from registry first. If this fails, no destructive
    // change has occurred and the operator is unaffected.
    registry.delete(operator_id)?;

    // Step 2: delete from keychain. Swallow NotFound — idempotent: a missing
    // keychain entry is fine. Any other error is returned as Keychain(_);
    // the registry is already empty at this point, so the operator can
    // re-enroll cleanly. An orphan keychain entry remains until cleared.
    match keychain.delete(operator_id) {
        Ok(()) | Err(KeychainError::NotFound { .. }) => {}
        Err(source) => return Err(UnenrollError::Keychain(source)),
    }

    // Step 3: append audit record. Both deletions have committed. If this
    // fails, the operator state is structurally gone but unrecorded; the
    // caller should surface this as a warning and exit 0.
    let at = time::OffsetDateTime::now_utc();
    if let Err(source) = audit.append(&AuditEvent::IdentityUnenrolled {
        identity_id: operator_id,
        at,
    }) {
        return Err(UnenrollError::AuditFailed(source));
    }

    Ok(operator_id)
}

/// Return `true` when at least one [`IdentityType::Operator`] entry exists in
/// `registry`.
pub(crate) fn has_operator(registry: &IdentityRegistry) -> Result<bool, reeve_runtime::RegistryError> {
    let stored = registry.list()?;
    Ok(find_existing_operator(&stored).is_some())
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
    use tempfile::TempDir;
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

    /// A keychain whose `store()` always rejects with a backend error.
    ///
    /// Used to test enroll failure paths: a `NotFound` from `store()` would
    /// be semantically incoherent (you cannot "not find" something you are
    /// storing), so this returns the platform-independent `InvalidSeedLength`
    /// as a proxy for "backend rejected the operation."
    struct RejectingKeyStore;

    impl OperatorKeyStore for RejectingKeyStore {
        fn store(
            &self,
            identity_id: IdentityId,
            _seed: &Zeroizing<[u8; SEED_LEN]>,
        ) -> Result<(), KeychainError> {
            Err(KeychainError::InvalidSeedLength {
                identity_id,
                len: 0,
            })
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

    /// A keychain where every method returns a hard, non-`NotFound` error.
    ///
    /// Used to test the unenroll keychain-failure path: when registry deletion
    /// has already succeeded but keychain deletion fails, the caller should see
    /// `UnenrollError::Keychain(_)` and the registry should be empty.
    ///
    /// Every method returns `InvalidSeedLength { len: 0 }` — never `NotFound`
    /// — so the type lives up to its name: hard failure, not absence.
    struct HardFailKeyStore;

    impl OperatorKeyStore for HardFailKeyStore {
        fn store(
            &self,
            identity_id: IdentityId,
            _seed: &Zeroizing<[u8; SEED_LEN]>,
        ) -> Result<(), KeychainError> {
            Err(KeychainError::InvalidSeedLength {
                identity_id,
                len: 0,
            })
        }

        fn retrieve(
            &self,
            identity_id: IdentityId,
        ) -> Result<Zeroizing<[u8; SEED_LEN]>, KeychainError> {
            Err(KeychainError::InvalidSeedLength {
                identity_id,
                len: 0,
            })
        }

        fn delete(&self, identity_id: IdentityId) -> Result<(), KeychainError> {
            Err(KeychainError::InvalidSeedLength {
                identity_id,
                len: 0,
            })
        }
    }

    fn open_audit(dir: &std::path::Path) -> AuditLog {
        AuditLog::open(dir.to_path_buf()).unwrap()
    }

    fn read_audit_lines(dir: &std::path::Path) -> Vec<serde_json::Value> {
        let path = dir.join("audit").join("log.jsonl");
        let text = std::fs::read_to_string(path).unwrap_or_default();
        text.lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Shared fixture for unenroll tests: a tempdir with an open registry,
    /// a fresh in-memory keychain, and an open audit log all rooted in the
    /// same directory.
    fn unenroll_fixture() -> (TempDir, IdentityRegistry, MemoryKeyStore, AuditLog) {
        let dir = tempdir().unwrap();
        let registry = open_registry(dir.path());
        let keychain = MemoryKeyStore::new();
        let audit = open_audit(dir.path());
        (dir, registry, keychain, audit)
    }

    /// Build a fresh operator `StoredIdentity` with the given display name,
    /// bypassing the single-operator guard in `enroll()`. Used in tests that
    /// need to inject operator entries directly into a registry.
    fn make_stored_operator(name: &str) -> StoredIdentity {
        let identity = Identity::new_operator(name.to_owned()).unwrap();
        let (_, public) = Keypair::generate().into_parts();
        let key = KeyRecord::new(identity.identity_id, public).unwrap();
        StoredIdentity::new(identity, key).unwrap()
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
        let keychain = RejectingKeyStore;

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
        let keychain = RejectingKeyStore;

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

    // CT_unenroll_round_trip: enroll an operator, then unenroll; registry is
    // empty, keychain has no entry, audit log has one event.
    #[test]
    fn unenroll_round_trip() {
        let (dir, registry, keychain, audit) = unenroll_fixture();

        let stored = enroll(&registry, &keychain, "Ada").unwrap();
        let operator_id = stored.identity().identity_id;

        let removed_id = unenroll(&registry, &keychain, &audit, true).unwrap();
        assert_eq!(removed_id, operator_id);

        // Registry must be empty.
        let listed = registry.list().unwrap();
        assert!(listed.is_empty(), "registry must be empty after unenroll");

        // Keychain must not have an entry.
        let kc_err = keychain.retrieve(operator_id).unwrap_err();
        assert!(
            matches!(kc_err, KeychainError::NotFound { .. }),
            "keychain must not have entry after unenroll",
        );

        // Audit log: only unenroll writes to audit in this test; enroll()
        // is the library function which does not emit an audit event.
        let lines = read_audit_lines(dir.path());
        assert_eq!(lines.len(), 1, "expected exactly one audit line");
        assert_eq!(lines[0]["kind"], "identity.unenrolled");
        assert_eq!(lines[0]["identity_id"], operator_id.to_string());
    }

    // CT_unenroll_requires_confirm: confirm=false → ConfirmRequired error.
    #[test]
    fn unenroll_requires_confirm() {
        let (_dir, registry, keychain, audit) = unenroll_fixture();

        enroll(&registry, &keychain, "Ada").unwrap();

        let err = unenroll(&registry, &keychain, &audit, false).unwrap_err();
        assert!(
            matches!(err, UnenrollError::ConfirmRequired),
            "expected ConfirmRequired, got {err}",
        );
        // Variant discrimination via pattern; string pinned in unenroll_error_display.
        assert!(
            err.to_string().contains("--confirm"),
            "ConfirmRequired message must mention --confirm: {err}",
        );
    }

    // CT_unenroll_no_operator: empty registry → NoOperator error.
    #[test]
    fn unenroll_no_operator() {
        let (_dir, registry, keychain, audit) = unenroll_fixture();

        let err = unenroll(&registry, &keychain, &audit, true).unwrap_err();
        assert!(
            matches!(err, UnenrollError::NoOperator),
            "expected NoOperator, got {err}",
        );
    }

    // CT_unenroll_idempotent_keychain: registry has the operator but the
    // keychain entry is already gone — unenroll must succeed anyway.
    #[test]
    fn unenroll_idempotent_keychain() {
        let (_dir, registry, keychain, audit) = unenroll_fixture();

        let stored = enroll(&registry, &keychain, "Ada").unwrap();
        let operator_id = stored.identity().identity_id;

        // Manually delete from keychain before calling unenroll.
        keychain.delete(operator_id).unwrap();

        // Unenroll must succeed despite the missing keychain entry.
        let removed_id = unenroll(&registry, &keychain, &audit, true).unwrap();
        assert_eq!(removed_id, operator_id);

        // Registry must be empty.
        assert!(registry.list().unwrap().is_empty());
    }

    // CT_unenroll_multiple_operators: registry has two Operator entries →
    // MultipleOperators error; no deletion occurs.
    #[test]
    fn unenroll_multiple_operators() {
        let (dir, registry, keychain, audit) = unenroll_fixture();

        // Write two operator-type StoredIdentity records directly into the
        // registry, bypassing the single-operator guard in enroll().
        let stored_a = make_stored_operator("Ada");
        let stored_b = make_stored_operator("Babbage");
        registry.write(&stored_a).unwrap();
        registry.write(&stored_b).unwrap();

        let err = unenroll(&registry, &keychain, &audit, true).unwrap_err();
        assert!(
            matches!(err, UnenrollError::MultipleOperators),
            "expected MultipleOperators, got {err}",
        );

        // Neither registry entry was deleted.
        assert_eq!(
            registry.list().unwrap().len(),
            2,
            "both registry entries must remain after MultipleOperators guard fires",
        );

        // No audit record was written.
        let lines = read_audit_lines(dir.path());
        assert!(
            lines.is_empty(),
            "no audit record must be written when MultipleOperators guard fires",
        );
    }

    // CT_unenroll_audit_failed_after_deletions: verify the AuditFailed variant
    // and its Display properties. Full end-to-end (registry + keychain deleted,
    // then audit append fails) is not implementable without either:
    //   (a) unsafe fd manipulation (prohibited by workspace `unsafe_code = deny`), or
    //   (b) AuditLog trait abstraction (architectural change deferred to a future phase).
    //
    // This test therefore validates:
    //   1. AuditFailed Display contains "unenrollment succeeded".
    //   2. AuditFailed::source() is Some (the underlying AuditError).
    //
    // The structural guarantee — that registry and keychain deletions committed
    // before the audit append — is verified by code-reading: `unenroll()` calls
    // registry.delete(), keychain.delete(), then audit.append(); if append fails,
    // the earlier deletions have already returned Ok(()). The ordering test in
    // unenroll_keychain_hard_failure demonstrates that registry delete commits
    // before subsequent failures.
    #[test]
    fn unenroll_audit_failed_display() {
        use std::error::Error;

        let dir = tempdir().unwrap();
        let audit_err = reeve_runtime::AuditError::Io {
            path: dir.path().join("audit").join("log.jsonl"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        let err = UnenrollError::AuditFailed(audit_err);

        let msg = err.to_string();
        assert!(
            msg.contains("unenrollment succeeded"),
            "AuditFailed Display must note that unenrollment succeeded: {msg}",
        );

        // Pin both levels of the source() chain:
        //   UnenrollError::AuditFailed → AuditError → io::Error
        let source = err.source().expect("AuditFailed must have a source");
        assert!(
            source.is::<reeve_runtime::AuditError>(),
            "source must be AuditError, got: {source:?}",
        );
        let inner = source
            .source()
            .expect("AuditError must have an inner source");
        assert!(
            inner.is::<std::io::Error>(),
            "inner source must be io::Error, got: {inner:?}",
        );
    }

    // CT_unenroll_keychain_hard_failure: registry deletion commits, then
    // keychain deletion returns a hard (non-NotFound) error → Keychain(_).
    // The registry is already empty at the point of keychain failure.
    //
    // This verifies the deliberate trade-off of registry-first ordering:
    // registry deletion commits even if keychain fails. The operator can
    // re-enroll cleanly; the orphan keychain entry remains until cleared.
    #[test]
    fn unenroll_keychain_hard_failure() {
        let (dir, registry, _mem_keychain, audit) = unenroll_fixture();

        // Write an operator directly so we can use HardFailKeyStore.
        let stored = make_stored_operator("Ada");
        registry.write(&stored).unwrap();

        let hard_keychain = HardFailKeyStore;
        let err = unenroll(&registry, &hard_keychain, &audit, true).unwrap_err();
        assert!(
            matches!(err, UnenrollError::Keychain(_)),
            "expected Keychain error, got {err}",
        );

        // Registry deletion committed before keychain failure.
        assert!(
            registry.list().unwrap().is_empty(),
            "registry must be empty: deletion committed before keychain failure",
        );

        // No audit record was written (audit append is past the failure point).
        let lines = read_audit_lines(dir.path());
        assert!(
            lines.is_empty(),
            "no audit record must be written when keychain delete fails",
        );
    }

    // CT_unenroll_error_display: UnenrollError variants produce messages
    // pinned to the constants declared at module level.
    #[test]
    fn unenroll_error_display() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().to_path_buf();

        // ConfirmRequired — pinned to MSG_CONFIRM_REQUIRED constant.
        assert_eq!(
            UnenrollError::ConfirmRequired.to_string(),
            MSG_CONFIRM_REQUIRED,
            "ConfirmRequired Display must match MSG_CONFIRM_REQUIRED",
        );

        // NoOperator — pinned to MSG_NO_OPERATOR constant.
        assert_eq!(
            UnenrollError::NoOperator.to_string(),
            MSG_NO_OPERATOR,
            "NoOperator Display must match MSG_NO_OPERATOR",
        );

        // MultipleOperators — pinned to MSG_MULTIPLE_OPERATORS constant.
        assert_eq!(
            UnenrollError::MultipleOperators.to_string(),
            MSG_MULTIPLE_OPERATORS,
            "MultipleOperators Display must match MSG_MULTIPLE_OPERATORS",
        );

        // Registry
        let reg_err = reeve_runtime::RegistryError::MissingHome;
        let msg = UnenrollError::Registry(reg_err).to_string();
        assert!(!msg.is_empty(), "Registry: {msg}");

        // Keychain
        let id = IdentityId::new().unwrap();
        let kc_err = KeychainError::NotFound { identity_id: id };
        let msg = UnenrollError::Keychain(kc_err).to_string();
        assert!(!msg.is_empty(), "Keychain: {msg}");
        assert!(
            msg.contains("keychain error"),
            "Keychain message must mention 'keychain error': {msg}",
        );

        // AuditFailed
        let audit_err = reeve_runtime::AuditError::Io {
            path: data_dir,
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        let msg = UnenrollError::AuditFailed(audit_err).to_string();
        assert!(!msg.is_empty(), "AuditFailed: {msg}");
        assert!(
            msg.contains("unenrollment succeeded"),
            "AuditFailed message must clarify unenrollment succeeded: {msg}",
        );
    }
}
