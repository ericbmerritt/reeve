//! Single-instance runtime lock for the Reeve daemon.
//!
//! Enforces the domain-model invariant that exactly one runtime process runs
//! per machine per operator at any time. Two primitives cooperate:
//!
//! - **Lockfile** at `<state_dir>/runtime.lock`: an `flock(LOCK_EX | LOCK_NB)`
//!   on this file's open `File` descriptor. The kernel releases the lock
//!   automatically when the last file descriptor referencing it is closed —
//!   i.e., when the runtime process exits, however it exits.
//!
//! - **PID file** at `<state_dir>/runtime.pid`: the ASCII decimal PID of the
//!   holding process, written atomically (tmp-write → fsync → rename → fsync
//!   parent). On `RuntimeLock` drop the PID file is removed.
//!
//! A second `reeve daemon start` attempt while the first is alive will:
//! 1. Try `flock(LOCK_EX | LOCK_NB)` → `EWOULDBLOCK`
//! 2. Read the PID from `runtime.pid` (if present)
//! 3. Return `RuntimeLockError::AlreadyRunning { pid: Some(N) }` or
//!    `AlreadyRunning { pid: None }` when the PID file is absent/unreadable.
//!
//! State directory: `$XDG_STATE_HOME/reeve`, falling back to
//! `$HOME/.local/state/reeve` when `XDG_STATE_HOME` is unset or empty.
//!
//! Filesystem safety follows `specs/reeve-transport-security.md` §
//! Filesystem Safety: no symlink follow, bounded reads, atomic writes.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use crate::fs_util::{
    apply_file_mode_options, ensure_directory, set_nofollow, sync_directory, FsCheckError,
};

/// Mode for the runtime state directory on Unix.
const STATE_DIR_MODE: u32 = 0o700;

/// Mode for the pid and lock files on Unix.
const STATE_FILE_MODE: u32 = 0o600;

/// Maximum bytes read from the PID file. PIDs are at most 22 decimal digits
/// (`u64::MAX`) plus a newline; 64 bytes is generous.
const MAX_PID_FILE_BYTES: u64 = 64;

/// Filename of the advisory lock inside the state directory.
const LOCK_FILENAME: &str = "runtime.lock";

/// Filename of the PID record inside the state directory.
const PID_FILENAME: &str = "runtime.pid";

// ── Error type ───────────────────────────────────────────────────────────────

/// Errors produced by [`RuntimeLock`] construction and the
/// [`default_state_dir`] factory.
///
/// No `anyhow` dependency: every variant carries its own diagnostic payload
/// so callers can produce actionable messages without string parsing.
#[derive(Debug)]
pub enum RuntimeLockError {
    /// A runtime process is already holding the lock. `pid` is read from the
    /// existing PID file; `None` means the file was absent or unreadable.
    ///
    /// **Contract:** When `pid` is `None`, the lock is held but the pid file
    /// was unreadable (missing, garbled, or written between flock and rename).
    /// This MUST NOT be interpreted as "lock is stale" — the kernel-level flock
    /// is the authoritative liveness signal. Any caller that force-acquires on
    /// `pid: None` violates the single-instance invariant.
    AlreadyRunning { pid: Option<u32> },

    /// Underlying filesystem error (open, read, write, rename, mkdir, flock).
    Io { path: PathBuf, source: io::Error },

    /// Neither `$XDG_STATE_HOME` nor `$HOME` is set in the environment.
    MissingHome,

    /// `$XDG_STATE_HOME` or `$HOME` is set to a relative path.
    ///
    /// Relative paths silently resolve against the process cwd at daemon-launch
    /// time, leading to state ending up in unexpected locations. Reject early.
    RelativeStateDir {
        var_name: &'static str,
        path: PathBuf,
    },
}

impl std::fmt::Display for RuntimeLockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning { pid: Some(pid) } => {
                write!(f, "runtime is already running, PID {pid}")
            }
            Self::AlreadyRunning { pid: None } => {
                f.write_str("runtime is already running (PID unknown — pid file absent)")
            }
            Self::Io { path, source } => {
                write!(f, "runtime lock IO at {}: {source}", path.display())
            }
            Self::MissingHome => f.write_str(
                "runtime lock default_state_dir requires HOME or XDG_STATE_HOME to be set",
            ),
            Self::RelativeStateDir { var_name, path } => {
                write!(
                    f,
                    "${var_name} is a relative path ({path}); the state directory must be absolute",
                    path = path.display()
                )
            }
        }
    }
}

impl std::error::Error for RuntimeLockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::AlreadyRunning { .. } | Self::MissingHome | Self::RelativeStateDir { .. } => None,
        }
    }
}

impl RuntimeLockError {
    fn from_fs(err: FsCheckError) -> Self {
        match err {
            FsCheckError::Io { path, source } => Self::Io { path, source },
            FsCheckError::Symlink { path } | FsCheckError::NotADirectory { path } => Self::Io {
                path,
                source: io::Error::other("runtime state directory is a symlink or non-directory"),
            },
            FsCheckError::WrongMode {
                path,
                actual,
                expected,
            } => Self::Io {
                path,
                source: io::Error::other(format!(
                    "state directory has mode 0o{actual:o}, expected 0o{expected:o}"
                )),
            },
        }
    }
}

// ── Public factory ────────────────────────────────────────────────────────────

/// Default runtime state directory: `$XDG_STATE_HOME/reeve`, falling back to
/// `$HOME/.local/state/reeve` when `XDG_STATE_HOME` is unset or empty.
///
/// Mirrors the pattern of `IdentityRegistry::default_data_dir()` but targets
/// the XDG *state* base (`~/.local/state`) rather than the data base
/// (`~/.local/share`).
pub fn default_state_dir() -> Result<PathBuf, RuntimeLockError> {
    resolve_default_state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn resolve_default_state_dir(
    xdg_state_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, RuntimeLockError> {
    let base = match xdg_state_home {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(RuntimeLockError::RelativeStateDir {
                    var_name: "XDG_STATE_HOME",
                    path,
                });
            }
            path
        }
        _ => {
            let home = home.ok_or(RuntimeLockError::MissingHome)?;
            let path = PathBuf::from(home);
            if !path.is_absolute() {
                return Err(RuntimeLockError::RelativeStateDir {
                    var_name: "HOME",
                    path,
                });
            }
            path.join(".local").join("state")
        }
    };
    Ok(base.join("reeve"))
}

// ── RuntimeLock ───────────────────────────────────────────────────────────────

/// Exclusive single-instance lock for the Reeve daemon.
///
/// Constructed via [`RuntimeLock::acquire`]. While alive, the process holds
/// an `flock(LOCK_EX | LOCK_NB)` on `<state_dir>/runtime.lock` and has
/// written its PID to `<state_dir>/runtime.pid`.
///
/// `Clone` is intentionally not derived: duplicating a lock handle would
/// produce two owners.
#[derive(Debug)]
pub struct RuntimeLock {
    state_dir: PathBuf,
    _lock_file: File,
}

impl RuntimeLock {
    /// Acquire the runtime lock in `state_dir`.
    ///
    /// - Creates `state_dir` with mode `0o700` if it does not yet exist.
    /// - Opens (or creates) `runtime.lock` and calls `flock(LOCK_EX | LOCK_NB)`.
    /// - On success: writes the current PID to `runtime.pid` atomically.
    /// - On `EWOULDBLOCK`: reads the existing `runtime.pid` and returns
    ///   [`RuntimeLockError::AlreadyRunning`].
    ///
    /// On any error after `flock` succeeds (e.g., `write_pid_file` fails), the
    /// flock is released via the local `lock_file` going out of scope during
    /// error propagation. The caller may retry from scratch; no lock is held
    /// after a returned `Err`.
    pub fn acquire(state_dir: PathBuf) -> Result<Self, RuntimeLockError> {
        ensure_directory(&state_dir, STATE_DIR_MODE).map_err(RuntimeLockError::from_fs)?;

        let lock_path = state_dir.join(LOCK_FILENAME);
        let lock_file = open_lock_file(&lock_path)?;

        match flock_exclusive_nb(&lock_file) {
            Ok(()) => {}
            Err(LockError::WouldBlock) => {
                let pid = read_pid_file(&state_dir.join(PID_FILENAME));
                return Err(RuntimeLockError::AlreadyRunning { pid });
            }
            Err(LockError::Io(source)) => {
                return Err(RuntimeLockError::Io {
                    path: lock_path.clone(),
                    source,
                });
            }
        }

        let pid_path = state_dir.join(PID_FILENAME);
        write_pid_file(&state_dir, &pid_path)?;

        Ok(Self {
            state_dir,
            _lock_file: lock_file,
        })
    }
}

impl Drop for RuntimeLock {
    fn drop(&mut self) {
        let pid_path = self.state_dir.join(PID_FILENAME);
        // Best-effort removal: the kernel will release the flock when the fd
        // closes, so daemon exit safety does not depend on this succeeding.
        let _ = fs::remove_file(&pid_path);
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn open_lock_file(lock_path: &Path) -> Result<File, RuntimeLockError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    apply_file_mode_options(&mut options, STATE_FILE_MODE);
    set_nofollow(&mut options);
    options
        .open(lock_path)
        .map_err(|source| RuntimeLockError::Io {
            path: lock_path.to_path_buf(),
            source,
        })
}

/// Internal error returned by [`flock_exclusive_nb`].
enum LockError {
    /// The file is already locked by another process (EWOULDBLOCK / EAGAIN).
    WouldBlock,
    /// An unexpected OS error occurred.
    Io(io::Error),
}

/// Attempt to acquire an exclusive non-blocking advisory lock on `file`.
fn flock_exclusive_nb(file: &File) -> Result<(), LockError> {
    match file.try_lock() {
        Ok(()) => Ok(()),
        Err(fs::TryLockError::WouldBlock) => Err(LockError::WouldBlock),
        Err(fs::TryLockError::Error(e)) => Err(LockError::Io(e)),
    }
}

/// Write the current process's PID (decimal + newline) to `pid_path`.
///
/// Atomic: a crash mid-write leaves the previous pid file intact rather than
/// a partial write.
fn write_pid_file(state_dir: &Path, pid_path: &Path) -> Result<(), RuntimeLockError> {
    let pid = std::process::id();
    let content = format!("{pid}\n");

    let mut tmp = NamedTempFile::new_in(state_dir).map_err(|source| RuntimeLockError::Io {
        path: state_dir.to_path_buf(),
        source,
    })?;

    apply_pid_file_mode(tmp.as_file()).map_err(|source| RuntimeLockError::Io {
        path: tmp.path().to_path_buf(),
        source,
    })?;

    tmp.write_all(content.as_bytes())
        .map_err(|source| RuntimeLockError::Io {
            path: tmp.path().to_path_buf(),
            source,
        })?;

    tmp.as_file()
        .sync_all()
        .map_err(|source| RuntimeLockError::Io {
            path: tmp.path().to_path_buf(),
            source,
        })?;

    tmp.persist(pid_path).map_err(|err| RuntimeLockError::Io {
        path: pid_path.to_path_buf(),
        source: err.error,
    })?;

    sync_directory(state_dir);
    Ok(())
}

/// Read and parse the PID from a pid file, returning `None` on any failure.
pub(crate) fn read_pid_file(pid_path: &Path) -> Option<u32> {
    use std::io::Read;

    let mut options = OpenOptions::new();
    options.read(true);
    set_nofollow(&mut options);
    let mut file = options.open(pid_path).ok()?;

    let mut buf = Vec::with_capacity(32);
    (&mut file)
        .take(MAX_PID_FILE_BYTES)
        .read_to_end(&mut buf)
        .ok()?;

    let text = String::from_utf8(buf).ok()?;
    text.trim().parse::<u32>().ok()
}

// `apply_pid_file_mode` operates on an already-open `&File` via
// `set_permissions`, which is a different shape from `fs_util::apply_file_mode_options`
// (which takes `&mut OpenOptions`). The canonical shared version for
// already-open files is `fs_util::apply_file_perms`.
#[cfg(unix)]
fn apply_pid_file_mode(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(STATE_FILE_MODE))
}

#[cfg(not(unix))]
fn apply_pid_file_mode(_file: &File) -> io::Result<()> {
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    // Serialize tests that mutate process-global environment variables so they
    // don't race with each other. The mutex is never poisoned in normal
    // operation; any test that panics while holding it will cause subsequent
    // env-var tests to fail loudly rather than silently observe stale state.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn state_dir() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("reeve");
        (tmp, dir)
    }

    // ── Core acquire / release ────────────────────────────────────────────────

    /// Acquiring the lock writes the current PID to the pid file; dropping the
    /// lock removes it.
    #[test]
    fn acquire_first_lock_writes_pid() {
        let (_tmp, dir) = state_dir();
        let lock = RuntimeLock::acquire(dir.clone()).unwrap();

        let pid_path = dir.join(PID_FILENAME);
        assert!(pid_path.exists(), "pid file must exist while lock is held");

        let contents = fs::read_to_string(&pid_path).unwrap();
        let recorded_pid: u32 = contents.trim().parse().unwrap();
        assert_eq!(recorded_pid, std::process::id());

        drop(lock);
        assert!(!pid_path.exists(), "pid file must be removed after drop");
    }

    /// A second acquire on the same state directory while the first lock is
    /// held returns `AlreadyRunning { pid: Some(N) }` where N is the first
    /// holder's PID.
    #[test]
    fn second_acquire_returns_already_running_with_pid() {
        let (_tmp, dir) = state_dir();
        // Keep the first lock alive for the entire test.
        let _first = RuntimeLock::acquire(dir.clone()).unwrap();

        let err = RuntimeLock::acquire(dir.clone()).unwrap_err();
        let debug = format!("{err:?}");
        match err {
            RuntimeLockError::AlreadyRunning { pid: Some(pid) } => {
                assert_eq!(pid, std::process::id());
            }
            RuntimeLockError::AlreadyRunning { pid: None }
            | RuntimeLockError::Io { .. }
            | RuntimeLockError::MissingHome
            | RuntimeLockError::RelativeStateDir { .. } => {
                panic!("expected AlreadyRunning {{ pid: Some(_) }}, got {debug}");
            }
        }
    }

    /// If the pid file is deleted while the lock is held (e.g., manual removal
    /// or crash-cleanup by an operator), a second acquire attempt still finds
    /// the flock already held and returns `AlreadyRunning { pid: None }`.
    #[test]
    fn second_acquire_with_missing_pid_file() {
        let (_tmp, dir) = state_dir();
        let _first = RuntimeLock::acquire(dir.clone()).unwrap();

        // Simulate the pid file being absent.
        let pid_path = dir.join(PID_FILENAME);
        fs::remove_file(&pid_path).unwrap();
        assert!(!pid_path.exists());

        let err = RuntimeLock::acquire(dir.clone()).unwrap_err();
        assert!(
            matches!(err, RuntimeLockError::AlreadyRunning { pid: None }),
            "expected AlreadyRunning {{ pid: None }}, got {err:?}",
        );
    }

    /// After the first `RuntimeLock` is dropped, a second acquire succeeds.
    #[test]
    fn release_via_drop_releases_lock() {
        let (_tmp, dir) = state_dir();
        let first = RuntimeLock::acquire(dir.clone()).unwrap();
        drop(first);
        let _second = RuntimeLock::acquire(dir.clone()).unwrap();
    }

    // ── Permissions check ─────────────────────────────────────────────────────

    /// `acquire` rejects a state directory with wrong permissions (0o755
    /// instead of the required 0o700).
    #[test]
    #[cfg(unix)]
    fn acquire_rejects_state_dir_with_wrong_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("reeve");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();

        let err = RuntimeLock::acquire(dir).unwrap_err();
        let debug = format!("{err:?}");
        match err {
            RuntimeLockError::Io { source, .. } => {
                let msg = source.to_string();
                assert!(
                    msg.contains("755") || msg.contains("700"),
                    "error message must reference mode bits: {msg}",
                );
            }
            RuntimeLockError::AlreadyRunning { .. }
            | RuntimeLockError::MissingHome
            | RuntimeLockError::RelativeStateDir { .. } => {
                panic!("expected Io error, got {debug}")
            }
        }
    }

    // ── default_state_dir ─────────────────────────────────────────────────────

    /// Restores an environment variable to its original value on drop, even if
    /// the test panics.
    struct EnvGuard {
        name: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(name: &'static str, value: &str) -> Self {
            let original = std::env::var_os(name);
            std::env::set_var(name, value);
            Self { name, original }
        }

        fn unset(name: &'static str) -> Self {
            let original = std::env::var_os(name);
            std::env::remove_var(name);
            Self { name, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.original.take() {
                Some(v) => std::env::set_var(self.name, v),
                None => std::env::remove_var(self.name),
            }
        }
    }

    /// `default_state_dir()` uses `$XDG_STATE_HOME/reeve` when the variable
    /// is set to a non-empty value.
    #[test]
    fn default_state_dir_respects_xdg() {
        let tmp = tempfile::tempdir().unwrap();
        let xdg = tmp.path().to_str().unwrap();
        let _guard = ENV_LOCK.lock().unwrap();
        let _xdg_guard = EnvGuard::set("XDG_STATE_HOME", xdg);
        let result = default_state_dir().unwrap();
        assert_eq!(result, tmp.path().join("reeve"));
    }

    /// When `$XDG_STATE_HOME` is unset, `default_state_dir()` falls back to
    /// `$HOME/.local/state/reeve`.
    #[test]
    fn default_state_dir_falls_back_to_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_str().unwrap();
        let _guard = ENV_LOCK.lock().unwrap();
        let _xdg_guard = EnvGuard::unset("XDG_STATE_HOME");
        let _home_guard = EnvGuard::set("HOME", home);
        let result = default_state_dir().unwrap();
        assert_eq!(
            result,
            tmp.path().join(".local").join("state").join("reeve")
        );
    }

    // ── Internal resolve_default_state_dir (pure, no env mutation) ────────────

    #[test]
    fn resolve_uses_xdg_state_home_when_set() {
        let result =
            resolve_default_state_dir(Some(OsStr::new("/srv/state")), Some(OsStr::new("/home/op")))
                .unwrap();
        assert_eq!(result, PathBuf::from("/srv/state/reeve"));
    }

    #[test]
    fn resolve_falls_back_to_home_when_xdg_unset() {
        let result = resolve_default_state_dir(None, Some(OsStr::new("/home/op"))).unwrap();
        assert_eq!(result, PathBuf::from("/home/op/.local/state/reeve"),);
    }

    #[test]
    fn resolve_falls_back_to_home_when_xdg_empty() {
        let result =
            resolve_default_state_dir(Some(OsStr::new("")), Some(OsStr::new("/home/op"))).unwrap();
        assert_eq!(result, PathBuf::from("/home/op/.local/state/reeve"),);
    }

    #[test]
    fn resolve_errors_when_both_unset() {
        let err = resolve_default_state_dir(None, None).unwrap_err();
        assert!(matches!(err, RuntimeLockError::MissingHome));
    }

    /// `resolve_default_state_dir` rejects relative `XDG_STATE_HOME` and
    /// relative `HOME` values.
    #[test]
    fn resolve_rejects_relative_xdg_state_home() {
        // Relative XDG_STATE_HOME
        let err =
            resolve_default_state_dir(Some(OsStr::new("state/home")), Some(OsStr::new("/home/op")))
                .unwrap_err();
        assert!(
            matches!(
                err,
                RuntimeLockError::RelativeStateDir {
                    var_name: "XDG_STATE_HOME",
                    ..
                }
            ),
            "expected RelativeStateDir for XDG_STATE_HOME, got {err:?}",
        );

        // Relative HOME (XDG unset so HOME fallback is used)
        let err = resolve_default_state_dir(None, Some(OsStr::new("home/op"))).unwrap_err();
        assert!(
            matches!(
                err,
                RuntimeLockError::RelativeStateDir {
                    var_name: "HOME",
                    ..
                }
            ),
            "expected RelativeStateDir for HOME, got {err:?}",
        );
    }

    /// Overwriting the pid file with garbage while the lock is held causes a
    /// second `acquire` to return `AlreadyRunning { pid: None }`.
    #[test]
    fn pid_file_with_garbage_content_yields_none() {
        fn check_garbage(garbage: &[u8]) {
            let (_tmp, dir) = state_dir();
            // Acquire and hold the first lock.
            let _first = RuntimeLock::acquire(dir.clone()).unwrap();
            let pid_path = dir.join(PID_FILENAME);
            // Overwrite the pid file with garbage while the lock is held.
            fs::write(&pid_path, garbage).unwrap();
            // Second acquire must find the flock held and report pid: None.
            let err = RuntimeLock::acquire(dir.clone()).unwrap_err();
            assert!(
                matches!(err, RuntimeLockError::AlreadyRunning { pid: None }),
                "expected AlreadyRunning {{ pid: None }} for garbage {garbage:?}, got {err:?}",
            );
        }

        // Non-UTF-8 bytes
        check_garbage(&[0xFF, 0xFF]);
        // Negative ASCII (not a valid u32)
        check_garbage(b"-1\n");
        // u32 overflow
        check_garbage(b"99999999999\n");
    }

    // ── Error Display / source ────────────────────────────────────────────────

    #[test]
    fn display_already_running_with_pid() {
        let err = RuntimeLockError::AlreadyRunning { pid: Some(12345) };
        let s = err.to_string();
        assert!(s.contains("12345"), "display must include pid: {s}");
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn display_already_running_without_pid() {
        let err = RuntimeLockError::AlreadyRunning { pid: None };
        let s = err.to_string();
        assert!(
            s.contains("unknown") || s.contains("absent"),
            "display must indicate pid is unknown: {s}",
        );
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn display_io_error_includes_path_and_source() {
        let io_err = io::Error::new(io::ErrorKind::PermissionDenied, "nope");
        let err = RuntimeLockError::Io {
            path: PathBuf::from("/some/path"),
            source: io_err,
        };
        let s = err.to_string();
        assert!(s.contains("/some/path"), "display must include path: {s}");
        assert!(
            s.contains("nope"),
            "display must include source message: {s}"
        );
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn display_missing_home() {
        let err = RuntimeLockError::MissingHome;
        let s = err.to_string();
        assert!(
            s.contains("HOME") || s.contains("XDG_STATE_HOME"),
            "display must reference env var names: {s}",
        );
        assert!(std::error::Error::source(&err).is_none());
    }
}
