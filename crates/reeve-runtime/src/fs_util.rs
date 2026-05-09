//! Crate-private filesystem safety helpers shared by the inbox, audit, ledger,
//! and identity registry modules.
//!
//! Every public function here follows `specs/reeve-transport-security.md` §
//! Filesystem Safety: no symlink following, non-directory rejection, and
//! mode-bit checks without silent chmod. Callers map [`FsCheckError`] into
//! their own typed error via `From`.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

/// Mask for extracting permission bits (rwx for u/g/o plus sticky/setuid/setgid)
/// from a `mode_t`, discarding file-type bits.
pub(crate) const MODE_BITS_MASK: u32 = 0o7777;

/// Sub-error produced by the shared fs-safety helpers. Each consuming module
/// converts this into its own typed error via `From` or an explicit mapping.
#[derive(Debug)]
pub(crate) enum FsCheckError {
    /// Underlying filesystem error on `path`.
    Io { path: PathBuf, source: io::Error },
    /// `path` is a symbolic link; the runtime refuses to follow symlinks.
    Symlink { path: PathBuf },
    /// `path` exists but is not a directory.
    NotADirectory { path: PathBuf },
    /// `path` is a directory with the wrong mode bits.
    WrongMode {
        path: PathBuf,
        actual: u32,
        expected: u32,
    },
}

/// Ensure `path` is a directory with `expected_mode` on Unix.
///
/// - If `path` does not exist, it is created (with all missing parents) using
///   `expected_mode` on Unix. After creation the path is re-stat'd to catch
///   symlink-swap races: if the just-created path has become a symlink or a
///   non-directory, `FsCheckError::Symlink` / `FsCheckError::NotADirectory`
///   is returned. Future hardening would use `rustix`'s `openat2` with
///   `RESOLVE_NO_SYMLINKS` to guard intermediate components; the post-create
///   re-stat is the current defense.
/// - If `path` already exists, it is verified to be a plain directory with
///   exactly `expected_mode` (Unix) or accepted unconditionally (non-Unix).
///   Mode mismatches surface as [`FsCheckError::WrongMode`] rather than being
///   silently fixed — operator misconfiguration is visible.
/// - If `path` is a symlink at any point, [`FsCheckError::Symlink`] is
///   returned without following it.
pub(crate) fn ensure_directory(path: &Path, expected_mode: u32) -> Result<(), FsCheckError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(FsCheckError::Symlink {
                    path: path.to_path_buf(),
                });
            }
            if !metadata.is_dir() {
                return Err(FsCheckError::NotADirectory {
                    path: path.to_path_buf(),
                });
            }
            check_mode(path, &metadata, expected_mode)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            create_dir_all_secure(path, expected_mode).map_err(|source| FsCheckError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            // Defense-in-depth: re-stat after creation to catch symlink-swap
            // races between `create_dir_all_secure` returning and here. A full
            // TOCTOU-proof solution would use openat2(RESOLVE_NO_SYMLINKS) on
            // Linux; the re-stat catches the most common race window.
            let metadata = fs::symlink_metadata(path).map_err(|source| FsCheckError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            if metadata.file_type().is_symlink() {
                return Err(FsCheckError::Symlink {
                    path: path.to_path_buf(),
                });
            }
            if !metadata.is_dir() {
                return Err(FsCheckError::NotADirectory {
                    path: path.to_path_buf(),
                });
            }
            check_mode(path, &metadata, expected_mode)
        }
        Err(source) => Err(FsCheckError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(unix)]
fn check_mode(path: &Path, metadata: &fs::Metadata, expected: u32) -> Result<(), FsCheckError> {
    use std::os::unix::fs::PermissionsExt;
    let actual = metadata.permissions().mode() & MODE_BITS_MASK;
    if actual != expected {
        return Err(FsCheckError::WrongMode {
            path: path.to_path_buf(),
            actual,
            expected,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_mode(_path: &Path, _metadata: &fs::Metadata, _expected: u32) -> Result<(), FsCheckError> {
    Ok(())
}

#[cfg(unix)]
pub(crate) fn create_dir_all_secure(path: &Path, mode: u32) -> io::Result<()> {
    use std::fs::DirBuilder;
    use std::os::unix::fs::DirBuilderExt;

    DirBuilder::new().recursive(true).mode(mode).create(path)
}

#[cfg(not(unix))]
pub(crate) fn create_dir_all_secure(path: &Path, _mode: u32) -> io::Result<()> {
    fs::create_dir_all(path)
}

/// Set `O_NOFOLLOW` on `options` on Unix so a symlink placed at the target
/// path surfaces as an error rather than being silently followed. A no-op on
/// non-Unix platforms.
#[cfg(unix)]
pub(crate) fn set_nofollow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(not(unix))]
pub(crate) fn set_nofollow(_options: &mut OpenOptions) {}

/// Apply `mode` to `options` on Unix so a newly created file gets the
/// specified permission bits. A no-op on non-Unix platforms.
#[cfg(unix)]
pub(crate) fn apply_file_mode_options(options: &mut OpenOptions, mode: u32) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(mode);
}

#[cfg(not(unix))]
pub(crate) fn apply_file_mode_options(_options: &mut OpenOptions, _mode: u32) {}

/// Best-effort fsync of a directory after a rename for durability.
///
/// Failures are non-fatal: the file content is already durable through
/// `sync_all` on the tmp file, and the rename itself is the atomicity
/// primitive — directory metadata persistence is a power-loss durability
/// question, not a torn-state one.
///
/// Opens the directory with `O_NOFOLLOW` (and `O_DIRECTORY` on Unix) so a
/// symlink placed at `dir` between the rename and this call does not cause
/// fsync to hit an attacker-controlled target.
pub(crate) fn sync_directory(dir: &Path) {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY);
    }
    if let Ok(handle) = options.open(dir) {
        let _ = handle.sync_all();
    }
}

/// Open `path` for appending with `O_NOFOLLOW` and `mode` on Unix.
///
/// Creates the file if absent. Combines `apply_file_mode_options`,
/// `set_nofollow`, and the open call so audit and ledger modules share a
/// single JSONL-open path.
pub(crate) fn open_jsonl_file(path: &Path, mode: u32) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    apply_file_mode_options(&mut options, mode);
    set_nofollow(&mut options);
    options.open(path)
}

/// Set permission bits on an already-open file on Unix.
///
/// Used for files created via `NamedTempFile` (which opens internally and
/// does not support `OpenOptions::mode` at create time). A no-op on
/// non-Unix platforms.
#[cfg(unix)]
pub(crate) fn apply_file_perms(file: &File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub(crate) fn apply_file_perms(_file: &File, _mode: u32) -> io::Result<()> {
    Ok(())
}

/// Write `content` atomically to `path` using a temp file in `dir`.
///
/// Pattern: `NamedTempFile::new_in(dir)` → set mode → write → fsync →
/// persist → sync dir. A crash at any point before persist leaves `path`
/// unchanged.
pub(crate) fn atomic_write_file(
    path: &Path,
    dir: &Path,
    content: &[u8],
    mode: u32,
) -> io::Result<()> {
    use std::io::Write as _;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    apply_file_perms(tmp.as_file(), mode)?;
    tmp.write_all(content)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    sync_directory(dir);
    Ok(())
}

/// Read a file with `O_NOFOLLOW`, up to `max_bytes`, as a UTF-8 string.
///
/// Returns `io::Error` on open failure (including symlinks → `ELOOP`/`ENOTDIR`),
/// read failure, or non-UTF-8 content. Callers map this to their own error types.
pub(crate) fn read_nofollow_bounded(path: &Path, max_bytes: u64) -> io::Result<String> {
    let mut options = OpenOptions::new();
    options.read(true);
    set_nofollow(&mut options);
    let file = options.open(path)?;

    let cap = max_bytes.saturating_add(1);
    let init_cap = usize::try_from(cap).unwrap_or(usize::MAX).min(8 * 1024);
    let mut buffer = Vec::with_capacity(init_cap);
    let cap_usize = usize::try_from(cap).unwrap_or(usize::MAX);
    file.take(u64::try_from(cap_usize).unwrap_or(u64::MAX))
        .read_to_end(&mut buffer)?;

    // Detect truncation: if buffer hit the cap, the file exceeds max_bytes.
    if buffer.len() >= cap_usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("file exceeds {max_bytes} byte limit"),
        ));
    }

    String::from_utf8(buffer)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file is not valid UTF-8"))
}

/// Error produced by [`resolve_xdg_base_dir`].
#[derive(Debug)]
pub(crate) enum XdgBaseError {
    /// Neither `$HOME` nor `$XDG_DATA_HOME` was set (or both were empty).
    MissingHome,
    /// The supplied environment variable contains a relative path, which
    /// would silently resolve against the process cwd at daemon-launch time.
    RelativeDir {
        var_name: &'static str,
        path: PathBuf,
    },
}

/// Resolve the XDG base data directory from the environment.
///
/// Returns the bare base `PathBuf` (no `reeve/...` suffix). Callers append
/// their own subdirectory suffix and map `XdgBaseError` into their own error
/// type.
///
/// Logic:
/// - If `xdg` is `Some` and non-empty, use it. Reject relative paths.
/// - Otherwise use `home`, appending `.local/share`. Reject relative paths.
/// - If `home` is also absent, return `XdgBaseError::MissingHome`.
pub(crate) fn resolve_xdg_base_dir(
    xdg: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, XdgBaseError> {
    match xdg {
        Some(value) if !value.is_empty() => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(XdgBaseError::RelativeDir {
                    var_name: "XDG_DATA_HOME",
                    path,
                });
            }
            Ok(path)
        }
        _ => {
            let home = home.ok_or(XdgBaseError::MissingHome)?;
            let path = PathBuf::from(home);
            if !path.is_absolute() {
                return Err(XdgBaseError::RelativeDir {
                    var_name: "HOME",
                    path,
                });
            }
            Ok(path.join(".local").join("share"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_nofollow_bounded_rejects_oversized_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.bin");
        let limit: u64 = 16;
        let size = usize::try_from(limit + 1).unwrap();
        fs::write(&path, vec![b'x'; size]).unwrap();
        let err = read_nofollow_bounded(&path, limit).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("byte limit"), "error: {err}");
    }

    #[test]
    fn read_nofollow_bounded_accepts_exactly_at_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("exact.txt");
        let limit: u64 = 16;
        let size = usize::try_from(limit).unwrap();
        fs::write(&path, vec![b'a'; size]).unwrap();
        let content = read_nofollow_bounded(&path, limit).unwrap();
        assert_eq!(content.len(), size);
    }
}
