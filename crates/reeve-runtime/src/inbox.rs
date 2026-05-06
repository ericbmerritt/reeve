//! Per-agent inbox directory layout per `specs/reeve-walking-skeleton.ladder.md`
//! phase 4 task 1 and `specs/reeve-transport-security.md` § Delivery Model.
//!
//! Provisions `agents/<identity_id>/inbox/{tmp,new,cur,quarantine,archive}` for
//! any registered identity. The directory name is `identity_id.to_string()` — a
//! hyphenated `UUIDv7` — which is filesystem-safe, globally unique, and directly
//! derivable from the registry without a separate name-to-id lookup. Human-
//! readable mapping (`identity_id` ↔ `display_name`) is already available through
//! the registry; adding it to the filesystem path would trade uniqueness for
//! readability at the cost of collisions and non-filesystem-safe characters.
//!
//! Filesystem safety follows `specs/reeve-transport-security.md` §
//! Filesystem Safety: no symlink following, non-regular-file rejection, mode-
//! bit checks, and no silent chmod on existing directories. An existing
//! directory with the wrong mode surfaces as [`InboxError::WrongDirectoryMode`]
//! rather than being silently fixed — operator misconfiguration is visible.
//!
//! The provisioned layout feeds two downstream consumers: the watcher (Task 13),
//! which inotify/kqueue-watches `new/` and moves messages into `cur/`, and the
//! verification pipeline (Task 12), which inspects signatures and routes
//! rejected messages into `quarantine/`. The `archive/` directory holds files
//! rotated out of `cur/` by [`crate::watcher::Watcher::rotate_cur`] after they
//! age past the configured retention threshold.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use reeve_types::IdentityId;

use crate::fs_util::{ensure_directory, FsCheckError};

/// Mode for all inbox directories on Unix. Restrictive: runtime-owned, not
/// world-readable. Per `specs/reeve-transport-security.md` § Filesystem Safety.
const INBOX_DIR_MODE: u32 = 0o700;

/// Canonical directory-name components. The spec defines these as stable names;
/// these constants make the coupling explicit and prevent drift from typos.
const AGENTS_DIR_NAME: &str = "agents";
const INBOX_DIR_NAME: &str = "inbox";
const TMP_SUBDIR: &str = "tmp";
const NEW_SUBDIR: &str = "new";
const CUR_SUBDIR: &str = "cur";
const QUARANTINE_SUBDIR: &str = "quarantine";
const ARCHIVE_SUBDIR: &str = "archive";

/// The shared `agents/` parent under a Reeve data directory. Owns the layout
/// root and provisions per-identity inbox trees beneath it.
///
/// `Clone` is intentionally not derived. Callers that need shared access
/// should wrap in `Arc<InboxLayout>` to make sharing explicit.
///
/// `data_dir` must not span filesystem boundaries; all paths produced by this
/// type are relative to the same mount point so that atomic rename within the
/// tree is safe.
#[derive(Debug)]
pub struct InboxLayout {
    /// The `<data_dir>/agents/` directory.
    agents_dir: PathBuf,
}

impl InboxLayout {
    /// Open (or create) the `agents/` layout root at `<data_dir>/agents/`.
    ///
    /// Creates the directory with mode `0o700` on Unix if it does not already
    /// exist. An existing directory is verified to carry `0o700`; mismatches
    /// surface as [`InboxError::WrongDirectoryMode`] rather than being silently
    /// fixed.
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self, InboxError> {
        let agents_dir = data_dir.into().join(AGENTS_DIR_NAME);
        ensure_directory(&agents_dir, INBOX_DIR_MODE).map_err(InboxError::from_fs)?;
        Ok(Self { agents_dir })
    }

    /// Provision the inbox directory structure for an identity.
    ///
    /// Creates, in order:
    /// - `agents/<identity_id>/` — per-identity container
    /// - `agents/<identity_id>/inbox/` — maildir root
    /// - `agents/<identity_id>/inbox/tmp/` — sender staging area
    /// - `agents/<identity_id>/inbox/new/` — completed messages awaiting pickup
    /// - `agents/<identity_id>/inbox/cur/` — durably delivered messages
    /// - `agents/<identity_id>/inbox/quarantine/` — verification failures
    /// - `agents/<identity_id>/inbox/archive/` — post-retention cur/ housekeeping
    ///
    /// All directories are created with mode `0o700` on Unix. Idempotent:
    /// repeated calls for the same identity succeed when the directories already
    /// exist with the correct mode.
    ///
    /// Rejected when any path:
    /// - is a symbolic link ([`InboxError::SymlinkRejected`])
    /// - exists but is not a directory ([`InboxError::NotADirectory`])
    /// - is a directory with the wrong mode ([`InboxError::WrongDirectoryMode`])
    ///
    /// Callers that want to provision only `IdentityType::Agent` entries should
    /// filter before calling. This method provisions any identity — the spec's
    /// wording says "agents" but the watcher delivers to recipients of any type,
    /// and restricting here would prevent future operator or external inboxes
    /// without a new API. The typical caller iterates through Agent-typed
    /// entries from the registry.
    pub fn provision(&self, identity_id: IdentityId) -> Result<AgentInbox, InboxError> {
        let id_str = identity_id.to_string();
        let agent_dir = self.agents_dir.join(&id_str);
        let inbox_root = agent_dir.join(INBOX_DIR_NAME);
        for dir in dirs_to_provision(&agent_dir, &inbox_root) {
            ensure_directory(&dir, INBOX_DIR_MODE).map_err(InboxError::from_fs)?;
        }
        Ok(AgentInbox::from_root(inbox_root))
    }

    /// Returns a handle to an inbox that was previously provisioned.
    ///
    /// Verifies only the root (`agents/<id>/inbox/`); subdirectories are NOT
    /// checked at this call. Callers that need all four subdirs to exist with
    /// correct modes should use [`InboxLayout::provision`] instead. Any caller
    /// that opens files relative to the returned paths must use OS-level
    /// no-follow semantics (e.g., `O_NOFOLLOW`) to avoid TOCTOU symlink
    /// substitution.
    ///
    /// Returns `Err(InboxError::NotFound)` if the root does not exist.
    pub fn open_existing(&self, identity_id: IdentityId) -> Result<AgentInbox, InboxError> {
        let root = self.inbox_root(identity_id);
        match fs::symlink_metadata(&root) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(InboxError::SymlinkRejected { path: root });
                }
                if !metadata.is_dir() {
                    return Err(InboxError::NotADirectory { path: root });
                }
                check_inbox_directory_mode(&root, &metadata)?;
                Ok(AgentInbox::from_root(root))
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                Err(InboxError::NotFound { path: root })
            }
            Err(source) => Err(InboxError::Io { path: root, source }),
        }
    }

    fn inbox_root(&self, identity_id: IdentityId) -> PathBuf {
        self.agents_dir
            .join(identity_id.to_string())
            .join(INBOX_DIR_NAME)
    }
}

/// A handle to a single identity's provisioned inbox tree. Produced by
/// [`InboxLayout::provision`] or [`InboxLayout::open_existing`].
///
/// All path accessors return references into the handle's owned root path.
/// The directories are not re-checked on each access; callers should treat
/// them as stable after successful provisioning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInbox {
    root: PathBuf,
    tmp: PathBuf,
    new: PathBuf,
    cur: PathBuf,
    quarantine: PathBuf,
    archive: PathBuf,
}

impl AgentInbox {
    fn from_root(root: PathBuf) -> Self {
        let tmp = root.join(TMP_SUBDIR);
        let new = root.join(NEW_SUBDIR);
        let cur = root.join(CUR_SUBDIR);
        let quarantine = root.join(QUARANTINE_SUBDIR);
        let archive = root.join(ARCHIVE_SUBDIR);
        Self {
            root,
            tmp,
            new,
            cur,
            quarantine,
            archive,
        }
    }

    /// Construct an inbox handle from a plain path.
    ///
    /// The directories must already exist (e.g., created by
    /// [`crate::agent_fs::AgentDirs::provision`]). Use this for name-based
    /// agent inboxes (`agents/lead/inbox/`) rather than identity-ID-based
    /// ones (`agents/<uuid>/inbox/`).
    pub fn from_path(inbox_root: PathBuf) -> Self {
        Self::from_root(inbox_root)
    }

    /// The inbox base: `agents/<id>/inbox/`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Sender staging area: `agents/<id>/inbox/tmp/`.
    pub fn tmp(&self) -> &Path {
        &self.tmp
    }

    /// Completed messages awaiting runtime pickup: `agents/<id>/inbox/new/`.
    /// Named `new_dir` because `new` is a Rust method name convention.
    pub fn new_dir(&self) -> &Path {
        &self.new
    }

    /// Verified messages durably delivered to agent context:
    /// `agents/<id>/inbox/cur/`.
    pub fn cur(&self) -> &Path {
        &self.cur
    }

    /// Messages that failed verification or trust-tier filtering:
    /// `agents/<id>/inbox/quarantine/`.
    pub fn quarantine(&self) -> &Path {
        &self.quarantine
    }

    /// Post-retention archive for `cur/` rotation housekeeping:
    /// `agents/<id>/inbox/archive/`. Files moved here by [`Watcher::rotate_cur`]
    /// have aged past the configured retention threshold and are no longer
    /// needed in the active `cur/` buffer.
    pub fn archive(&self) -> &Path {
        &self.archive
    }
}

/// Errors surfaced by inbox layout operations. Every variant carries the
/// offending path (when applicable) so callers and audit-log consumers can
/// produce actionable diagnostics.
///
/// `InboxError` is not `Clone` or `PartialEq`: [`io::Error`] is neither.
///
/// Marked `#[non_exhaustive]` because future maildir-pipeline tasks
/// (verification, ledgers, watcher) will add variants for new failure modes
/// (replay rejection, schema mismatch, etc.) without breaking downstream
/// callers.
#[non_exhaustive]
#[derive(Debug)]
pub enum InboxError {
    /// Underlying filesystem error (open, read, write, mkdir).
    Io { path: PathBuf, source: io::Error },

    /// A path that should be a plain directory is a symbolic link. The
    /// runtime refuses to follow symlinks per `specs/reeve-transport-security.md`
    /// § Filesystem Safety.
    SymlinkRejected { path: PathBuf },

    /// A path exists but is not a directory (regular file, socket, etc.).
    NotADirectory { path: PathBuf },

    /// A directory exists with permissions other than `0o700`. Surfaced on
    /// Unix only. Never silently fixed — operator misconfiguration is visible.
    WrongDirectoryMode {
        path: PathBuf,
        actual: u32,
        expected: u32,
    },

    /// The inbox root does not exist. Returned only by
    /// [`InboxLayout::open_existing`]; [`InboxLayout::provision`] creates
    /// the root rather than returning this error.
    NotFound { path: PathBuf },
}

impl InboxError {
    fn from_fs(err: FsCheckError) -> Self {
        match err {
            FsCheckError::Io { path, source } => Self::Io { path, source },
            FsCheckError::Symlink { path } => Self::SymlinkRejected { path },
            FsCheckError::NotADirectory { path } => Self::NotADirectory { path },
            FsCheckError::WrongMode {
                path,
                actual,
                expected,
            } => Self::WrongDirectoryMode {
                path,
                actual,
                expected,
            },
        }
    }
}

impl std::fmt::Display for InboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "inbox IO at {}: {source}", path.display())
            }
            Self::SymlinkRejected { path } => {
                write!(f, "inbox refuses to follow symlink at {}", path.display())
            }
            Self::NotADirectory { path } => write!(
                f,
                "inbox path at {} exists but is not a directory",
                path.display(),
            ),
            Self::WrongDirectoryMode {
                path,
                actual,
                expected,
            } => write!(
                f,
                "inbox directory at {} has mode 0o{actual:o}, expected 0o{expected:o}",
                path.display(),
            ),
            Self::NotFound { path } => write!(f, "inbox at {} does not exist", path.display()),
        }
    }
}

impl std::error::Error for InboxError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::SymlinkRejected { .. }
            | Self::NotADirectory { .. }
            | Self::WrongDirectoryMode { .. }
            | Self::NotFound { .. } => None,
        }
    }
}

/// All directories that `provision` must create and mode-check, in order.
///
/// Includes `agent_dir` (`agents/<id>/`, the per-identity container) before
/// `inbox_root` (`agents/<id>/inbox/`) and the maildir subdirs
/// (`tmp/`, `new/`, `cur/`, `quarantine/`, `archive/`). Provisioning the
/// container explicitly ensures its mode is enforced even when it was
/// pre-created by an external tool with a permissive umask.
fn dirs_to_provision(agent_dir: &Path, inbox_root: &Path) -> [PathBuf; 7] {
    [
        agent_dir.to_path_buf(),
        inbox_root.to_path_buf(),
        inbox_root.join(TMP_SUBDIR),
        inbox_root.join(NEW_SUBDIR),
        inbox_root.join(CUR_SUBDIR),
        inbox_root.join(QUARANTINE_SUBDIR),
        inbox_root.join(ARCHIVE_SUBDIR),
    ]
}

#[cfg(unix)]
fn check_inbox_directory_mode(path: &Path, metadata: &fs::Metadata) -> Result<(), InboxError> {
    use crate::fs_util::MODE_BITS_MASK;
    use std::os::unix::fs::PermissionsExt;
    let actual = metadata.permissions().mode() & MODE_BITS_MASK;
    if actual != INBOX_DIR_MODE {
        return Err(InboxError::WrongDirectoryMode {
            path: path.to_path_buf(),
            actual,
            expected: INBOX_DIR_MODE,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_inbox_directory_mode(_path: &Path, _metadata: &fs::Metadata) -> Result<(), InboxError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::symlink;
    use std::sync::Arc;

    use reeve_types::IdentityId;
    use tempfile::tempdir;

    /// `tempfile::tempdir()` creates with the platform default (e.g. 0o755),
    /// which fails the 0o700 posture check on `open`. Tighten first.
    #[cfg(unix)]
    fn make_secure_tempdir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[cfg(not(unix))]
    fn make_secure_tempdir() -> tempfile::TempDir {
        tempdir().unwrap()
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use crate::fs_util::MODE_BITS_MASK;
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & MODE_BITS_MASK
    }

    fn open_layout(data_dir: &Path) -> InboxLayout {
        InboxLayout::open(data_dir.to_path_buf()).unwrap()
    }

    // I1: provision creates the expected layout with mode 0o700 on all dirs.
    #[cfg(unix)]
    #[test]
    fn provision_creates_expected_layout() {
        let data_dir = make_secure_tempdir();
        let layout = open_layout(data_dir.path());
        let id = IdentityId::new().unwrap();
        let inbox = layout.provision(id).unwrap();

        assert!(inbox.root().is_dir(), "inbox root missing");
        assert!(inbox.tmp().is_dir(), "tmp missing");
        assert!(inbox.new_dir().is_dir(), "new missing");
        assert!(inbox.cur().is_dir(), "cur missing");
        assert!(inbox.quarantine().is_dir(), "quarantine missing");
        assert!(inbox.archive().is_dir(), "archive missing");

        assert_eq!(mode_of(inbox.root()), 0o700, "root mode wrong");
        assert_eq!(mode_of(inbox.tmp()), 0o700, "tmp mode wrong");
        assert_eq!(mode_of(inbox.new_dir()), 0o700, "new mode wrong");
        assert_eq!(mode_of(inbox.cur()), 0o700, "cur mode wrong");
        assert_eq!(mode_of(inbox.quarantine()), 0o700, "quarantine mode wrong");
        assert_eq!(mode_of(inbox.archive()), 0o700, "archive mode wrong");
    }

    // I_archive_2: open_existing (via provision) rejects archive/ with mode 0o755
    // — WrongDirectoryMode is returned rather than silently accepting the bad mode.
    #[cfg(unix)]
    #[test]
    fn provision_rejects_archive_with_wrong_mode() {
        use std::os::unix::fs::PermissionsExt;

        let data_dir = make_secure_tempdir();
        let layout = open_layout(data_dir.path());
        let id = IdentityId::new().unwrap();

        // Provision the inbox fully, then chmod archive/ to 0o755.
        let inbox = layout.provision(id).unwrap();
        fs::set_permissions(inbox.archive(), fs::Permissions::from_mode(0o755)).unwrap();

        // A second provision call must detect the mode mismatch on archive/.
        let err = layout.provision(id).unwrap_err();
        let InboxError::WrongDirectoryMode {
            actual, expected, ..
        } = err
        else {
            panic!("expected WrongDirectoryMode, got {err:?}");
        };
        assert_eq!(actual, 0o755, "actual mode should be 0o755");
        assert_eq!(
            expected, INBOX_DIR_MODE,
            "expected mode should be INBOX_DIR_MODE"
        );
    }

    // I1-ext: agents/<id>/ container directory has mode 0o700.
    #[cfg(unix)]
    #[test]
    fn provision_sets_agent_container_dir_mode() {
        let data_dir = make_secure_tempdir();
        let layout = open_layout(data_dir.path());
        let id = IdentityId::new().unwrap();
        layout.provision(id).unwrap();

        let agent_dir = data_dir.path().join(AGENTS_DIR_NAME).join(id.to_string());
        assert!(agent_dir.is_dir(), "agent container dir missing");
        assert_eq!(mode_of(&agent_dir), 0o700, "agent container dir mode wrong");
    }

    // I2: provision is idempotent — calling it twice succeeds.
    #[test]
    fn provision_is_idempotent() {
        let data_dir = make_secure_tempdir();
        let layout = open_layout(data_dir.path());
        let id = IdentityId::new().unwrap();
        layout.provision(id).unwrap();
        layout.provision(id).unwrap();
    }

    // I3: symlink at a subdir path causes SymlinkRejected.
    #[cfg(unix)]
    #[test]
    fn provision_rejects_symlink_at_subdir() {
        let data_dir = make_secure_tempdir();
        let layout = open_layout(data_dir.path());
        let id = IdentityId::new().unwrap();

        // Pre-create the inbox root and tmp manually, then replace new/ with a symlink.
        let inbox_root = data_dir
            .path()
            .join(AGENTS_DIR_NAME)
            .join(id.to_string())
            .join(INBOX_DIR_NAME);
        let tmp = inbox_root.join(TMP_SUBDIR);
        let new_path = inbox_root.join(NEW_SUBDIR);

        fs::create_dir_all(&tmp).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            for dir in [&inbox_root, &tmp] {
                fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let agent_dir = inbox_root.parent().unwrap();
            fs::set_permissions(agent_dir, fs::Permissions::from_mode(0o700)).unwrap();
        }

        let outside = tempdir().unwrap();
        let target = outside.path().join("evil");
        fs::create_dir_all(&target).unwrap();
        symlink(&target, &new_path).unwrap();

        let err = layout.provision(id).unwrap_err();
        assert!(
            matches!(err, InboxError::SymlinkRejected { .. }),
            "expected SymlinkRejected, got {err:?}",
        );
    }

    // I3-ext: agents/<id>/ being a symlink causes SymlinkRejected.
    #[cfg(unix)]
    #[test]
    fn provision_rejects_symlink_at_agent_container() {
        let data_dir = make_secure_tempdir();
        let layout = open_layout(data_dir.path());
        let id = IdentityId::new().unwrap();

        let container = data_dir.path().join(AGENTS_DIR_NAME).join(id.to_string());

        let outside = tempdir().unwrap();
        let real_dir = outside.path().join("real");
        fs::create_dir_all(&real_dir).unwrap();
        symlink(&real_dir, &container).unwrap();

        let err = layout.provision(id).unwrap_err();
        assert!(
            matches!(err, InboxError::SymlinkRejected { .. }),
            "expected SymlinkRejected when agents/<id>/ is a symlink, got {err:?}",
        );
    }

    // I4: an existing subdir with mode 0o755 causes WrongDirectoryMode.
    #[cfg(unix)]
    #[test]
    fn provision_rejects_wrong_mode() {
        use std::os::unix::fs::PermissionsExt;

        let data_dir = make_secure_tempdir();
        let layout = open_layout(data_dir.path());
        let id = IdentityId::new().unwrap();

        let inbox_root = data_dir
            .path()
            .join(AGENTS_DIR_NAME)
            .join(id.to_string())
            .join(INBOX_DIR_NAME);
        fs::create_dir_all(&inbox_root).unwrap();
        fs::set_permissions(&inbox_root, fs::Permissions::from_mode(0o755)).unwrap();

        let err = layout.provision(id).unwrap_err();
        let InboxError::WrongDirectoryMode {
            actual, expected, ..
        } = err
        else {
            panic!("expected WrongDirectoryMode, got {err:?}");
        };
        assert_eq!(actual, 0o755);
        assert_eq!(expected, INBOX_DIR_MODE);
    }

    // I5: a regular file at a subdir path causes NotADirectory.
    #[test]
    fn provision_rejects_file_at_subdir() {
        let data_dir = make_secure_tempdir();
        let layout = open_layout(data_dir.path());
        let id = IdentityId::new().unwrap();

        let inbox_root = data_dir
            .path()
            .join(AGENTS_DIR_NAME)
            .join(id.to_string())
            .join(INBOX_DIR_NAME);
        fs::create_dir_all(inbox_root.join(TMP_SUBDIR)).unwrap();
        {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let agent_dir = inbox_root.parent().unwrap();
                for dir in [agent_dir, &inbox_root, &inbox_root.join(TMP_SUBDIR)] {
                    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
                }
            }
        }
        fs::write(inbox_root.join(NEW_SUBDIR), b"not a dir").unwrap();

        let err = layout.provision(id).unwrap_err();
        assert!(
            matches!(err, InboxError::NotADirectory { .. }),
            "expected NotADirectory, got {err:?}",
        );
    }

    // I6: provisioning two distinct identities creates independent layouts.
    #[test]
    fn provision_two_agents_are_independent() {
        let data_dir = make_secure_tempdir();
        let layout = open_layout(data_dir.path());
        let id_a = IdentityId::new().unwrap();
        let id_b = IdentityId::new().unwrap();

        let inbox_a = layout.provision(id_a).unwrap();
        let inbox_b = layout.provision(id_b).unwrap();

        assert_ne!(inbox_a.root(), inbox_b.root());
        assert!(inbox_a.root().is_dir());
        assert!(inbox_b.root().is_dir());
    }

    // I7: open_existing succeeds after provision; fails before.
    #[test]
    fn open_existing_succeeds_after_provision_fails_before() {
        let data_dir = make_secure_tempdir();
        let layout = open_layout(data_dir.path());
        let id = IdentityId::new().unwrap();

        let err = layout.open_existing(id).unwrap_err();
        assert!(
            matches!(err, InboxError::NotFound { .. }),
            "expected NotFound before provision, got {err:?}",
        );

        layout.provision(id).unwrap();
        layout.open_existing(id).unwrap();
    }

    // I8: distinct identity_ids produce distinct paths.
    #[test]
    fn distinct_identity_ids_produce_distinct_paths() {
        let data_dir = make_secure_tempdir();
        let layout = open_layout(data_dir.path());
        let id_a = IdentityId::new().unwrap();
        let id_b = IdentityId::new().unwrap();

        assert_ne!(id_a, id_b);
        let inbox_a = layout.provision(id_a).unwrap();
        let inbox_b = layout.provision(id_b).unwrap();
        assert_ne!(inbox_a.root(), inbox_b.root());
    }

    // I9: deeply nested data_dir is handled gracefully.
    #[test]
    fn deeply_nested_data_dir_works() {
        let base = tempdir().unwrap();
        let data_dir = base.path().join("a").join("b").join("c").join("reeve");
        let layout = InboxLayout::open(data_dir).unwrap();
        let id = IdentityId::new().unwrap();
        layout.provision(id).unwrap();
    }

    // I10: Display of all error variants is non-empty and contains path or
    // mode info.
    #[test]
    fn error_display_is_non_empty_and_informative() {
        let path = PathBuf::from("synthetic/test-path");

        let io_err = InboxError::Io {
            path: path.clone(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        let rendered = io_err.to_string();
        assert!(!rendered.is_empty());
        assert!(rendered.contains("synthetic/test-path"), "Io: {rendered}");

        let sym_err = InboxError::SymlinkRejected { path: path.clone() };
        let rendered = sym_err.to_string();
        assert!(!rendered.is_empty());
        assert!(
            rendered.contains("synthetic/test-path"),
            "SymlinkRejected: {rendered}"
        );

        let not_dir_err = InboxError::NotADirectory { path: path.clone() };
        let rendered = not_dir_err.to_string();
        assert!(!rendered.is_empty());
        assert!(
            rendered.contains("synthetic/test-path"),
            "NotADirectory: {rendered}"
        );

        let mode_err = InboxError::WrongDirectoryMode {
            path: path.clone(),
            actual: 0o755,
            expected: 0o700,
        };
        let rendered = mode_err.to_string();
        assert!(!rendered.is_empty());
        assert!(
            rendered.contains("synthetic/test-path"),
            "WrongDirectoryMode path: {rendered}"
        );
        assert!(
            rendered.contains("755"),
            "WrongDirectoryMode actual: {rendered}"
        );
        assert!(
            rendered.contains("700"),
            "WrongDirectoryMode expected: {rendered}"
        );

        let not_found_err = InboxError::NotFound { path: path.clone() };
        let rendered = not_found_err.to_string();
        assert!(!rendered.is_empty());
        assert!(
            rendered.contains("synthetic/test-path"),
            "NotFound: {rendered}"
        );
    }

    // open() rejects a symlinked agents/ dir.
    #[cfg(unix)]
    #[test]
    fn open_rejects_symlinked_agents_dir() {
        let outer = tempdir().unwrap();
        let real_dir = outer.path().join("real_agents");
        fs::create_dir_all(&real_dir).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&real_dir, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let data_dir = outer.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        symlink(&real_dir, data_dir.join(AGENTS_DIR_NAME)).unwrap();

        let err = InboxLayout::open(data_dir).unwrap_err();
        assert!(
            matches!(err, InboxError::SymlinkRejected { .. }),
            "expected SymlinkRejected, got {err:?}",
        );
    }

    // open() rejects agents/ path that is a file, not a directory.
    #[test]
    fn open_rejects_file_at_agents_dir() {
        let data_dir = make_secure_tempdir();
        fs::write(data_dir.path().join(AGENTS_DIR_NAME), b"not a dir").unwrap();

        let err = InboxLayout::open(data_dir.path().to_path_buf()).unwrap_err();
        assert!(
            matches!(err, InboxError::NotADirectory { .. }),
            "expected NotADirectory, got {err:?}",
        );
    }

    // Concurrent provision of the same identity by two threads must not
    // corrupt the filesystem. Both calls must succeed.
    #[test]
    fn concurrent_provision_same_identity_succeeds() {
        let data_dir = make_secure_tempdir();
        let layout = Arc::new(open_layout(data_dir.path()));
        let id = IdentityId::new().unwrap();

        std::thread::scope(|s| {
            let layout_a = Arc::clone(&layout);
            let layout_b = Arc::clone(&layout);
            let handle_a = s.spawn(move || layout_a.provision(id));
            let handle_b = s.spawn(move || layout_b.provision(id));
            handle_a.join().unwrap().unwrap();
            handle_b.join().unwrap().unwrap();
        });

        // Both succeeded; the tree must still be intact.
        let inbox = layout.open_existing(id).unwrap();
        assert!(inbox.tmp().is_dir());
        assert!(inbox.new_dir().is_dir());
        assert!(inbox.cur().is_dir());
        assert!(inbox.quarantine().is_dir());
        assert!(inbox.archive().is_dir());
    }

    // Provisioning two agents, deleting one's tree, leaves the other intact.
    #[test]
    fn delete_one_agent_inbox_leaves_other_intact() {
        let data_dir = make_secure_tempdir();
        let layout = open_layout(data_dir.path());
        let id_a = IdentityId::new().unwrap();
        let id_b = IdentityId::new().unwrap();

        layout.provision(id_a).unwrap();
        layout.provision(id_b).unwrap();

        // Remove id_a's entire per-identity tree.
        let agent_dir_a = data_dir.path().join(AGENTS_DIR_NAME).join(id_a.to_string());
        fs::remove_dir_all(&agent_dir_a).unwrap();

        // id_a's inbox is gone.
        let err = layout.open_existing(id_a).unwrap_err();
        assert!(
            matches!(err, InboxError::NotFound { .. }),
            "expected NotFound for deleted agent, got {err:?}",
        );

        // id_b's inbox is unaffected.
        let inbox_b = layout.open_existing(id_b).unwrap();
        assert!(inbox_b.root().is_dir(), "id_b root should still exist");
        assert!(inbox_b.tmp().is_dir());
        assert!(inbox_b.new_dir().is_dir());
        assert!(inbox_b.cur().is_dir());
        assert!(inbox_b.quarantine().is_dir());
        assert!(inbox_b.archive().is_dir());
    }
}
