//! Public envelope-deposit helper shared by [`crate::dispatcher`] and CLI
//! callers.
//!
//! The maildir-style inbox layout (`inbox/tmp/` for staging, `inbox/new/` for
//! pickup) is the same on both sides: a runtime actor signing on behalf of a
//! subagent and an operator-side CLI (`reeve send`) signing on behalf of the
//! human. Both paths require:
//!
//! - A symlink check on the inbox root and its `tmp/` and `new/` children so
//!   a malicious or misconfigured component cannot redirect message bytes
//!   outside the agent's data dir (`specs/reeve-transport-security.md` §
//!   Filesystem Safety).
//! - A write-tmp-then-rename so a partially-written envelope can never be
//!   picked up by the watcher. `rename(2)` within the same filesystem is
//!   atomic.
//! - Restrictive file mode (`0o600`) so only the runtime user can read the
//!   envelope between deposit and pickup.
//!
//! Keeping one implementation prevents the two call sites from drifting on
//! any of those invariants.

use std::path::{Path, PathBuf};

use reeve_types::MessageId;

use crate::fs_util::atomic_write_file;

/// Mode applied to envelope files deposited in `inbox/new/`. Matches the
/// constant the dispatcher used pre-extraction.
const ENVELOPE_FILE_MODE: u32 = 0o600;

/// Errors surfaced when depositing a signed envelope into an agent's inbox.
///
/// Marked `#[non_exhaustive]` so future filesystem-safety variants
/// (read-only mount, ENOSPC special-casing) can be added without a major
/// version bump.
#[non_exhaustive]
#[derive(Debug)]
pub enum DepositError {
    /// A path component on the inbox layout was a symbolic link; deposit
    /// refused. `path` is the symlinked component.
    SymlinkRejected { path: PathBuf },
    /// Underlying filesystem error (mkdir, open, write, fsync, rename).
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl std::fmt::Display for DepositError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SymlinkRejected { path } => write!(
                f,
                "inbox path is a symlink (deposit refused): {}",
                path.display()
            ),
            Self::Io { path, source } => {
                write!(f, "io error at {}: {}", path.display(), source)
            }
        }
    }
}

impl std::error::Error for DepositError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::SymlinkRejected { .. } => None,
        }
    }
}

/// Atomically deposit `bytes` as `<message_id>` into `<inbox_dir>/new/`.
///
/// `inbox_dir` is the agent's `inbox/` root (containing `tmp/` and `new/`
/// children). The function writes to a temp file in `inbox/tmp/` (named by
/// the underlying [`tempfile`] crate, *not* by the caller — the staging
/// filename is not the message id), fsyncs, and renames atomically to
/// `inbox/new/<message_id>`. The watcher picks up files only from `new/`,
/// so a partially-written envelope is never observed.
///
/// `message_id` is typed as a [`MessageId`] (`UUIDv7` newtype) rather than a
/// `&str` so a caller cannot smuggle path separators or `..` past the API
/// boundary — the rendered `Display` form is always `[0-9a-f-]+`, which
/// is unambiguously a single filename component and cannot traverse out
/// of `inbox/new/`.
///
/// Rejects symlinks on `inbox_dir`, `inbox/tmp/`, and `inbox/new/` rather
/// than following them — see module docs.
pub fn deposit_envelope(
    inbox_dir: &Path,
    message_id: MessageId,
    bytes: &[u8],
) -> Result<(), DepositError> {
    let root_meta = std::fs::symlink_metadata(inbox_dir).map_err(|source| DepositError::Io {
        path: inbox_dir.to_path_buf(),
        source,
    })?;
    if root_meta.file_type().is_symlink() {
        return Err(DepositError::SymlinkRejected {
            path: inbox_dir.to_path_buf(),
        });
    }

    let tmp_dir = inbox_dir.join("tmp");
    let new_dir = inbox_dir.join("new");
    let new_path = new_dir.join(message_id.to_string());

    for dir in [&tmp_dir, &new_dir] {
        let meta = std::fs::symlink_metadata(dir).map_err(|source| DepositError::Io {
            path: dir.clone(),
            source,
        })?;
        if meta.file_type().is_symlink() {
            return Err(DepositError::SymlinkRejected { path: dir.clone() });
        }
    }

    atomic_write_file(&new_path, &tmp_dir, bytes, ENVELOPE_FILE_MODE).map_err(|source| {
        DepositError::Io {
            path: new_path,
            source,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::{provision_inbox, secure_dir};
    use std::fs;

    // D1: happy path — bytes end up in inbox/new/<message_id> with correct
    // mode and the staging tmp file is gone.
    #[test]
    fn deposit_writes_to_new_dir() {
        let tmp = secure_dir();
        provision_inbox(tmp.path());
        let inbox = tmp.path().join("inbox");
        let id = MessageId::new().unwrap();

        deposit_envelope(&inbox, id, b"hello world").unwrap();

        let landed = inbox.join("new").join(id.to_string());
        let body = fs::read(&landed).unwrap();
        assert_eq!(body, b"hello world");
        assert!(
            fs::read_dir(inbox.join("tmp")).unwrap().next().is_none(),
            "tmp/ must be empty after rename"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&landed).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "envelope file must be 0o600");
        }
    }

    // D2: a symlinked inbox root is rejected with a typed error (not followed).
    #[test]
    #[cfg(unix)]
    fn deposit_rejects_symlinked_inbox_root() {
        let tmp = secure_dir();
        let real_root = tmp.path().join("real");
        fs::create_dir_all(&real_root).unwrap();
        provision_inbox(&real_root);
        let real_inbox = real_root.join("inbox");
        let link = tmp.path().join("inbox");
        std::os::unix::fs::symlink(&real_inbox, &link).unwrap();

        let err = deposit_envelope(&link, MessageId::new().unwrap(), b"body").unwrap_err();
        assert!(
            matches!(err, DepositError::SymlinkRejected { ref path } if path == &link),
            "expected SymlinkRejected for {link:?}, got {err}"
        );
    }

    // D3: a missing inbox root surfaces as Io (not a panic).
    #[test]
    fn deposit_returns_io_error_for_missing_inbox() {
        let tmp = secure_dir();
        let missing = tmp.path().join("no-such-inbox");
        let err = deposit_envelope(&missing, MessageId::new().unwrap(), b"body").unwrap_err();
        assert!(
            matches!(err, DepositError::Io { .. }),
            "expected Io error, got {err}"
        );
    }

    // D4: a MessageId's Display form is always `[0-9a-f-]+` — single
    // filename component, no path separators, no parent-dir indirection.
    // This is what makes the `MessageId` type the security boundary:
    // callers cannot construct a value that traverses out of `new/`.
    #[test]
    fn message_id_display_is_path_safe_filename() {
        for _ in 0..32 {
            let id = MessageId::new().unwrap();
            let s = id.to_string();
            assert!(
                s.chars().all(|c| c.is_ascii_hexdigit() || c == '-'),
                "MessageId display must be hex+dash only, got {s:?}"
            );
            assert!(
                !s.contains('/') && !s.contains('\\') && !s.contains(".."),
                "MessageId display must not contain path separators or ..; got {s:?}"
            );
        }
    }
}
