//! Crate-private filesystem safety helpers shared by the inbox and identity
//! registry modules.
//!
//! Every public function here follows `specs/reeve-transport-security.md` §
//! Filesystem Safety: no symlink following, non-directory rejection, and
//! mode-bit checks without silent chmod. Callers map [`FsCheckError`] into
//! their own typed error via `From`.

use std::fs;
use std::io;
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
