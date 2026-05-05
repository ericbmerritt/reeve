//! On-disk identity registry per `specs/reeve-walking-skeleton.ladder.md`
//! phase 2 and `specs/reeve-domain-model.md` § State Ownership § Filesystem
//! (Durable).
//!
//! Persists registered identities and their key records as TOML files under
//! `<data_dir>/<identity_id>.toml`. Public keys live on disk; private key
//! material does not — domain-model invariant 5. The OS keychain integration
//! that holds private material is task 3 of this phase.
//!
//! The on-disk schema is `[identity]` plus a `[[keys]]` array. v1 always
//! writes exactly one key record per file; the array shape leaves room for
//! key rotation (active + deprecated entries) in later ladders without a
//! migration. Filenames are non-authoritative per
//! `specs/reeve-domain-model.md` invariant 12; `list` and `lookup` validate
//! that the contained `identity_id` matches the path stem.
//!
//! Filesystem safety follows `specs/reeve-transport-security.md` §
//! Filesystem Safety: defensive open (no symlink follow), bounded read size,
//! and atomic transitions via write-tmp-then-rename in the same directory.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use reeve_types::{Identity, IdentityId, IdentityIdError, KeyRecord};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::fs_util::{ensure_directory, set_nofollow, FsCheckError};

/// Maximum size in bytes of any single registry TOML file. Identities and
/// their key records serialize to well under a kilobyte; the cap guards
/// against torn writes, accidental large files, and decoder OOM. 64 KiB is
/// roomy enough for a degenerate (many-key) record while remaining trivially
/// bounded.
const MAX_REGISTRY_FILE_BYTES: u64 = 64 * 1024;

// Filesystem safety modes per `specs/reeve-transport-security.md` §
// Filesystem Safety: registry storage is runtime-owned and not expected to
// be modified by other local processes.
/// Mode for the registry data directory on Unix.
const REGISTRY_DIR_MODE: u32 = 0o700;
/// Mode for individual registry TOML files on Unix.
const REGISTRY_FILE_MODE: u32 = 0o600;

/// Filename suffix for stored identities.
const REGISTRY_FILE_EXTENSION: &str = "toml";

/// On-disk shape for a single identity registry file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RegistryFile {
    identity: Identity,
    #[serde(default)]
    keys: Vec<KeyRecord>,
}

/// A registered identity together with its active key record(s).
///
/// v1 stores exactly one key record per identity. The `key_records` `Vec`
/// shape mirrors the on-disk `[[keys]]` array so that future phases can add
/// deprecated entries without breaking the schema.
///
/// Fields are private: every `StoredIdentity` value is constructed through
/// `from_validated`, which is the single source of truth for the bond
/// between the identity and its key records. See [`StoredIdentity::new`]
/// (single-key construction) and the internal `from_validated` path used by
/// [`IdentityRegistry::list`] and [`IdentityRegistry::lookup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredIdentity {
    identity: Identity,
    key_records: Vec<KeyRecord>,
}

impl StoredIdentity {
    /// Bundle an identity with a single active key record.
    ///
    /// The key record's `identity_id` must match the identity's; mismatch
    /// surfaces as `KeyIdentityMismatch`. The validator (`from_validated`)
    /// is the single source of truth for the bond.
    pub fn new(identity: Identity, key_record: KeyRecord) -> Result<Self, RegistryError> {
        Self::from_validated(identity, vec![key_record])
    }

    /// Validate then construct. Single source of truth for the
    /// identity ↔ key-record bond and the v1 "exactly one key per file"
    /// invariant.
    fn from_validated(
        identity: Identity,
        key_records: Vec<KeyRecord>,
    ) -> Result<Self, RegistryError> {
        if key_records.is_empty() {
            return Err(RegistryError::NoKeys {
                identity_id: identity.identity_id,
            });
        }
        if key_records.len() > 1 {
            return Err(RegistryError::TooManyKeys {
                identity_id: identity.identity_id,
                count: key_records.len(),
            });
        }
        let key = &key_records[0];
        if key.identity_id != identity.identity_id {
            return Err(RegistryError::KeyIdentityMismatch {
                identity_id: identity.identity_id,
                key_identity_id: key.identity_id,
            });
        }
        Ok(Self {
            identity,
            key_records,
        })
    }

    /// Borrow the underlying identity.
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Borrow the key records associated with this identity. v1 always has
    /// exactly one entry; later phases may carry deprecated entries.
    pub fn key_records(&self) -> &[KeyRecord] {
        &self.key_records
    }
}

/// Operator-only on-disk store of registered identities and their public
/// key records. The runtime owns this directory; the TUI and other readers
/// load it through the runtime, never directly.
///
/// `Clone` is intentionally not derived: callers that need to share a
/// handle should wrap in `Arc<IdentityRegistry>`, making the share
/// explicit rather than duplicating the data-dir path silently.
#[derive(Debug)]
pub struct IdentityRegistry {
    data_dir: PathBuf,
}

impl IdentityRegistry {
    /// Open (or create) a registry rooted at `data_dir`. The directory is
    /// created with [`REGISTRY_DIR_MODE`] permissions on Unix if it does
    /// not already exist. An existing directory is verified to already
    /// carry [`REGISTRY_DIR_MODE`] on Unix; mismatches surface as
    /// [`RegistryError::WrongDirectoryMode`] rather than being silently
    /// chmodded, so an operator misconfiguration is visible.
    ///
    /// The mode posture is Unix-only: non-Unix platforms inherit the
    /// platform default and skip the mode check until the runtime grows
    /// a Windows ACL story.
    pub fn open(data_dir: PathBuf) -> Result<Self, RegistryError> {
        ensure_directory(&data_dir, REGISTRY_DIR_MODE).map_err(RegistryError::from_fs)?;
        Ok(Self { data_dir })
    }

    /// Default registry directory: `$XDG_DATA_HOME/reeve/identities`, falling
    /// back to `$HOME/.local/share/reeve/identities` when `XDG_DATA_HOME` is
    /// unset or empty per the XDG Base Directory Specification.
    ///
    /// Trusts the operator's process environment per the threat model in
    /// `specs/reeve-transport-security.md` § Trust Boundary; an
    /// env-controlled `XDG_DATA_HOME` or `HOME` is by design.
    pub fn default_data_dir() -> Result<PathBuf, RegistryError> {
        resolve_default_data_dir(
            std::env::var_os("XDG_DATA_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }

    /// Atomically write a stored identity to `<id>.toml`. Existing entries
    /// for the same `identity_id` are replaced; the rename is atomic on the
    /// same filesystem so concurrent readers never observe a torn file.
    ///
    /// Files are created with [`REGISTRY_FILE_MODE`] on Unix. Non-Unix
    /// platforms inherit the platform default until the runtime grows a
    /// Windows ACL story.
    pub fn write(&self, stored: &StoredIdentity) -> Result<(), RegistryError> {
        let path = self.path_for(stored.identity.identity_id);
        let file = RegistryFile {
            identity: stored.identity.clone(),
            keys: stored.key_records.clone(),
        };
        let body = toml::to_string(&file).map_err(|source| RegistryError::Serialize {
            path: path.clone(),
            source,
        })?;
        atomic_write(&self.data_dir, &path, body.as_bytes())
    }

    /// Read all `<uuid>.toml` files in the registry directory. `.toml`
    /// files that fail validation surface as typed errors; non-`.toml`
    /// files are skipped. Order is filesystem-dependent and not stable
    /// across calls.
    pub fn list(&self) -> Result<Vec<StoredIdentity>, RegistryError> {
        let entries = fs::read_dir(&self.data_dir).map_err(|source| RegistryError::Io {
            path: self.data_dir.clone(),
            source,
        })?;
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| RegistryError::Io {
                path: self.data_dir.clone(),
                source,
            })?;
            let path = entry.path();
            if !is_registry_file(&path) {
                continue;
            }
            out.push(read_validated(&path)?);
        }
        Ok(out)
    }

    /// Look up a single identity by id. Returns `Ok(None)` when the file
    /// does not exist; `Err(...)` for IO failures, parse failures, or
    /// filename / content mismatches.
    pub fn lookup(&self, id: IdentityId) -> Result<Option<StoredIdentity>, RegistryError> {
        let path = self.path_for(id);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(RegistryError::SymlinkedRegistryFile { path });
                }
                read_validated(&path).map(Some)
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(RegistryError::Io { path, source }),
        }
    }

    /// Returns the canonical TOML file path for `identity_id` under this
    /// registry. Useful for diagnostic messages that point operators at a
    /// specific file.
    pub fn toml_path(&self, identity_id: IdentityId) -> PathBuf {
        self.path_for(identity_id)
    }

    fn path_for(&self, id: IdentityId) -> PathBuf {
        self.data_dir
            .join(format!("{id}.{REGISTRY_FILE_EXTENSION}"))
    }
}

/// Errors surfaced by the on-disk identity registry. Every variant carries
/// the offending path (when applicable) so callers and audit-log consumers
/// can produce actionable diagnostics without re-deriving the path.
///
/// `RegistryError` is not `Clone` or `PartialEq`: [`io::Error`] is neither.
/// Audit-log fanout that needs cloneable errors must stringify at its own
/// boundary.
#[derive(Debug)]
pub enum RegistryError {
    /// Underlying filesystem error (open, read, write, rename, mkdir).
    Io { path: PathBuf, source: io::Error },

    /// Failed to deserialize a registry file from TOML.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },

    /// Failed to serialize a stored identity to TOML.
    Serialize {
        path: PathBuf,
        source: toml::ser::Error,
    },

    /// File `<expected>.toml` parsed cleanly but its `identity_id` does
    /// not match the path stem. Filenames are non-authoritative per
    /// `specs/reeve-domain-model.md` invariant 12; the file's content is
    /// authoritative, and `from_filename` is the derived path-stem id.
    FilenameMismatch {
        path: PathBuf,
        from_filename: IdentityId,
        from_content: IdentityId,
    },

    /// `<path>` reported a stat-time size larger than
    /// [`MAX_REGISTRY_FILE_BYTES`]. Likely a torn write, accidental write
    /// of unrelated content, or corruption.
    FileTooLargeStat { path: PathBuf, size: u64 },

    /// `<path>` was within the size cap at stat time but produced more
    /// than [`MAX_REGISTRY_FILE_BYTES`] bytes when read — i.e. the file
    /// grew between stat and open by at least one byte.
    FileTooLargeRead { path: PathBuf, size: u64 },

    /// `<path>` (the registry root) is a symbolic link. Operator setup
    /// mistake; the runtime owns its data directory and refuses to follow
    /// links there.
    SymlinkedDataDir { path: PathBuf },

    /// `<path>` (a registry entry file) is a symbolic link. Defense in
    /// depth: the runtime never follows symlinks for entry files per
    /// `specs/reeve-transport-security.md` § Filesystem Safety.
    SymlinkedRegistryFile { path: PathBuf },

    /// `<path>` exists but is not a regular file (FIFO, socket, device,
    /// etc.). Defense-in-depth against same-uid `DoS` at registry-read time.
    NotARegularFile { path: PathBuf },

    /// `<path>` exists but is not a directory; the registry root must be a
    /// real directory.
    NotADirectory { path: PathBuf },

    /// `default_data_dir` could not resolve `$HOME` and `$XDG_DATA_HOME` was
    /// unset.
    MissingHome,

    /// A path stem could not be parsed as a `UUIDv7` identity id, but the
    /// file extension matched.
    InvalidFilename { path: PathBuf, source: uuid::Error },

    /// A path stem parsed as a UUID but failed `UUIDv7` validation.
    InvalidIdentityId {
        path: PathBuf,
        source: IdentityIdError,
    },

    /// A registry file's body is not valid UTF-8.
    NonUtf8Body {
        path: PathBuf,
        source: std::string::FromUtf8Error,
    },

    /// A registry file's path stem cannot be decoded as UTF-8.
    NonUtf8Filename { path: PathBuf },

    /// A bundle's key record's `identity_id` did not match the identity's
    /// `identity_id` — caught at construction or read time.
    KeyIdentityMismatch {
        identity_id: IdentityId,
        key_identity_id: IdentityId,
    },

    /// A registry file deserialized with no `[[keys]]` entries. v1
    /// requires exactly one key record per identity.
    NoKeys { identity_id: IdentityId },

    /// A registry file deserialized with more than one `[[keys]]` entry.
    /// v1 requires exactly one key record per identity; the array shape
    /// leaves room for rotation in later phases.
    TooManyKeys {
        identity_id: IdentityId,
        count: usize,
    },

    /// The registry data directory exists with permissions other than
    /// [`REGISTRY_DIR_MODE`]. Surfaced on Unix only.
    WrongDirectoryMode {
        path: PathBuf,
        actual: u32,
        expected: u32,
    },
}

impl RegistryError {
    fn from_fs(err: FsCheckError) -> Self {
        match err {
            FsCheckError::Io { path, source } => Self::Io { path, source },
            FsCheckError::Symlink { path } => Self::SymlinkedDataDir { path },
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

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Dispatch contract: every RegistryError variant is handled in exactly
        // one arm, here or in `fmt_short`. The OR-pattern + `unreachable!` in
        // each function makes drift compile-fail in either direction.
        match self {
            Self::Io { path, source } => write!(f, "registry IO at {}: {source}", path.display()),
            Self::Parse { path, source } => {
                write!(f, "registry parse at {}: {source}", path.display())
            }
            Self::Serialize { path, source } => {
                write!(f, "registry serialize at {}: {source}", path.display())
            }
            Self::FilenameMismatch {
                path,
                from_filename,
                from_content,
            } => write!(
                f,
                "registry filename mismatch at {}: file stem says {from_filename} but identity_id is {from_content}",
                path.display(),
            ),
            Self::FileTooLargeStat { path, size } => write!(
                f,
                "registry file at {} is {size} bytes, exceeds the 64 KiB cap",
                path.display(),
            ),
            Self::FileTooLargeRead { path, size } => write!(
                f,
                "registry file at {} grew past the 64 KiB cap during read (observed {size} bytes)",
                path.display(),
            ),
            Self::WrongDirectoryMode {
                path,
                actual,
                expected,
            } => write!(
                f,
                "registry directory at {} has mode 0o{actual:o}, expected 0o{expected:o}",
                path.display(),
            ),
            Self::KeyIdentityMismatch {
                identity_id,
                key_identity_id,
            } => write!(
                f,
                "key record identity_id {key_identity_id} does not match identity {identity_id}",
            ),
            Self::NoKeys { identity_id } => write!(
                f,
                "registry file for identity {identity_id} has no [[keys]] entries; v1 requires exactly one",
            ),
            Self::TooManyKeys { identity_id, count } => write!(
                f,
                "registry file for identity {identity_id} has {count} [[keys]] entries; v1 requires exactly one",
            ),
            Self::MissingHome
            | Self::SymlinkedDataDir { .. }
            | Self::SymlinkedRegistryFile { .. }
            | Self::NotARegularFile { .. }
            | Self::NotADirectory { .. }
            | Self::InvalidFilename { .. }
            | Self::InvalidIdentityId { .. }
            | Self::NonUtf8Body { .. }
            | Self::NonUtf8Filename { .. } => fmt_short(self, f),
        }
    }
}

/// Split from `Display::fmt` to satisfy the workspace `too_many_lines`
/// floor; the catch-all arm there delegates here.
fn fmt_short(err: &RegistryError, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match err {
        RegistryError::MissingHome => {
            f.write_str("registry default_data_dir requires HOME or XDG_DATA_HOME to be set")
        }
        RegistryError::SymlinkedDataDir { path } => write!(
            f,
            "registry refuses to follow symlinked data directory at {}",
            path.display(),
        ),
        RegistryError::SymlinkedRegistryFile { path } => write!(
            f,
            "registry refuses to follow symlink at {}",
            path.display(),
        ),
        RegistryError::NotARegularFile { path } => write!(
            f,
            "registry refuses to read non-regular file at {}",
            path.display(),
        ),
        RegistryError::NotADirectory { path } => write!(
            f,
            "registry path at {} exists but is not a directory",
            path.display(),
        ),
        RegistryError::InvalidFilename { path, source } => write!(
            f,
            "registry filename at {} is not a UUID: {source}",
            path.display(),
        ),
        RegistryError::InvalidIdentityId { path, source } => write!(
            f,
            "registry filename at {} is not a UUIDv7 identity id: {source}",
            path.display(),
        ),
        RegistryError::NonUtf8Body { path, source } => write!(
            f,
            "registry file at {} is not valid UTF-8: {source}",
            path.display(),
        ),
        RegistryError::NonUtf8Filename { path } => write!(
            f,
            "registry filename at {} is not valid UTF-8",
            path.display(),
        ),
        // `wildcard_enum_match_arm` lint: enumerate every other variant by name
        // so a new variant forces an explicit decision here, not a silent
        // fallthrough.
        RegistryError::Io { .. }
        | RegistryError::Parse { .. }
        | RegistryError::Serialize { .. }
        | RegistryError::FilenameMismatch { .. }
        | RegistryError::FileTooLargeStat { .. }
        | RegistryError::FileTooLargeRead { .. }
        | RegistryError::WrongDirectoryMode { .. }
        | RegistryError::KeyIdentityMismatch { .. }
        | RegistryError::NoKeys { .. }
        | RegistryError::TooManyKeys { .. } => {
            unreachable!("dispatched in <RegistryError as Display>::fmt")
        }
    }
}

impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Serialize { source, .. } => Some(source),
            Self::InvalidFilename { source, .. } => Some(source),
            Self::InvalidIdentityId { source, .. } => Some(source),
            Self::NonUtf8Body { source, .. } => Some(source),
            Self::FilenameMismatch { .. }
            | Self::FileTooLargeStat { .. }
            | Self::FileTooLargeRead { .. }
            | Self::SymlinkedDataDir { .. }
            | Self::SymlinkedRegistryFile { .. }
            | Self::NotARegularFile { .. }
            | Self::NotADirectory { .. }
            | Self::MissingHome
            | Self::NonUtf8Filename { .. }
            | Self::KeyIdentityMismatch { .. }
            | Self::NoKeys { .. }
            | Self::TooManyKeys { .. }
            | Self::WrongDirectoryMode { .. } => None,
        }
    }
}

fn resolve_default_data_dir(
    xdg_data_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, RegistryError> {
    let base = match xdg_data_home {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => {
            let home = home.ok_or(RegistryError::MissingHome)?;
            PathBuf::from(home).join(".local").join("share")
        }
    };
    Ok(base.join("reeve").join("identities"))
}

/// Case-sensitive: registry files are lowercase `.toml` by convention;
/// `.TOML` is treated as foreign content.
fn is_registry_file(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str) == Some(REGISTRY_FILE_EXTENSION)
}

fn read_validated(path: &Path) -> Result<StoredIdentity, RegistryError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| RegistryError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(RegistryError::SymlinkedRegistryFile {
            path: path.to_path_buf(),
        });
    }
    if !metadata.file_type().is_file() {
        return Err(RegistryError::NotARegularFile {
            path: path.to_path_buf(),
        });
    }
    if metadata.len() > MAX_REGISTRY_FILE_BYTES {
        return Err(RegistryError::FileTooLargeStat {
            path: path.to_path_buf(),
            size: metadata.len(),
        });
    }
    let body = read_bounded(path)?;
    let file: RegistryFile = toml::from_str(&body).map_err(|source| RegistryError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    let from_filename = parse_identity_id_from_path(path)?;
    if from_filename != file.identity.identity_id {
        return Err(RegistryError::FilenameMismatch {
            path: path.to_path_buf(),
            from_filename,
            from_content: file.identity.identity_id,
        });
    }
    StoredIdentity::from_validated(file.identity, file.keys)
}

/// Open with no-follow semantics on Unix and read up to
/// [`MAX_REGISTRY_FILE_BYTES`]+1 to detect oversized files even when the
/// stat-reported length lied (e.g., file grew between metadata and open).
fn read_bounded(path: &Path) -> Result<String, RegistryError> {
    let mut options = OpenOptions::new();
    options.read(true);
    set_nofollow(&mut options);
    let mut file = options.open(path).map_err(|source| RegistryError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let cap = MAX_REGISTRY_FILE_BYTES.saturating_add(1);
    let cap_usize = usize::try_from(cap).unwrap_or(usize::MAX);
    let mut buffer = Vec::with_capacity(cap_usize.min(8 * 1024));
    (&mut file)
        .take(cap)
        .read_to_end(&mut buffer)
        .map_err(|source| RegistryError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if u64::try_from(buffer.len()).unwrap_or(u64::MAX) > MAX_REGISTRY_FILE_BYTES {
        return Err(RegistryError::FileTooLargeRead {
            path: path.to_path_buf(),
            size: u64::try_from(buffer.len()).unwrap_or(u64::MAX),
        });
    }
    String::from_utf8(buffer).map_err(|source| RegistryError::NonUtf8Body {
        path: path.to_path_buf(),
        source,
    })
}

fn parse_identity_id_from_path(path: &Path) -> Result<IdentityId, RegistryError> {
    let stem =
        path.file_stem()
            .and_then(OsStr::to_str)
            .ok_or_else(|| RegistryError::NonUtf8Filename {
                path: path.to_path_buf(),
            })?;
    let uuid = stem
        .parse::<uuid::Uuid>()
        .map_err(|source| RegistryError::InvalidFilename {
            path: path.to_path_buf(),
            source,
        })?;
    IdentityId::try_from(uuid).map_err(|source| RegistryError::InvalidIdentityId {
        path: path.to_path_buf(),
        source,
    })
}

fn atomic_write(dir: &Path, target: &Path, body: &[u8]) -> Result<(), RegistryError> {
    let mut tmp = NamedTempFile::new_in(dir).map_err(|source| RegistryError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    apply_file_mode(tmp.as_file()).map_err(|source| RegistryError::Io {
        path: tmp.path().to_path_buf(),
        source,
    })?;
    tmp.write_all(body).map_err(|source| RegistryError::Io {
        path: tmp.path().to_path_buf(),
        source,
    })?;
    tmp.as_file()
        .sync_all()
        .map_err(|source| RegistryError::Io {
            path: tmp.path().to_path_buf(),
            source,
        })?;
    tmp.persist(target).map_err(|err| RegistryError::Io {
        path: target.to_path_buf(),
        source: err.error,
    })?;
    sync_directory(dir);
    Ok(())
}

#[cfg(unix)]
fn apply_file_mode(file: &File) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(REGISTRY_FILE_MODE))
}

#[cfg(not(unix))]
fn apply_file_mode(_file: &File) -> io::Result<()> {
    Ok(())
}

/// Best-effort fsync of the directory after a rename so the entry is durable
/// before we return. Failures here are non-fatal: the file content is
/// already durable through `sync_all` on the tmp file, and the rename itself
/// is the atomicity primitive — directory metadata persistence is a power-
/// loss durability question, not a torn-state one.
fn sync_directory(dir: &Path) {
    if let Ok(handle) = File::open(dir) {
        let _ = handle.sync_all();
    }
}

#[cfg(test)]
mod tests {
    // what-if-defer: this layer's contract is single-process,
    // well-formed-FS. Out of scope:
    //   * P4 — simulated mid-read IO failure (needs a fault-injecting FS shim).
    //   * P7 — concurrent last-writer-wins (needs the supervisor).

    use super::*;

    use std::os::unix::fs::symlink;

    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;
    use reeve_types::{Identity, IdentityId, KeyId, KeyRecord, KeyState, PublicKey};
    use tempfile::tempdir;
    use time::OffsetDateTime;

    fn fresh_public_key() -> PublicKey {
        let signing_key = SigningKey::generate(&mut OsRng);
        PublicKey::from_verifying_key(signing_key.verifying_key())
    }

    /// `tempfile::tempdir()` creates with the platform default (e.g. 0o755
    /// on macOS), which fails the 0o700 posture check on `open`. Tighten
    /// first.
    #[cfg(unix)]
    fn chmod_secure(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(REGISTRY_DIR_MODE)).unwrap();
    }

    #[cfg(not(unix))]
    fn chmod_secure(_path: &Path) {}

    fn fresh_stored_operator(name: &str) -> StoredIdentity {
        let identity = Identity::new_operator(name.to_owned()).unwrap();
        let key = KeyRecord::new(identity.identity_id, fresh_public_key()).unwrap();
        StoredIdentity::new(identity, key).unwrap()
    }

    #[test]
    fn open_creates_directory_when_missing() {
        let dir = tempdir().unwrap();
        let registry_path = dir.path().join("identities");
        assert!(!registry_path.exists());
        let _registry = IdentityRegistry::open(registry_path.clone()).unwrap();
        assert!(registry_path.is_dir());
    }

    #[test]
    fn open_accepts_existing_directory() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let _first = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let _second = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
    }

    #[test]
    fn write_then_lookup_round_trips() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let stored = fresh_stored_operator("Ada");
        registry.write(&stored).unwrap();
        let loaded = registry
            .lookup(stored.identity.identity_id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded, stored);
    }

    #[test]
    fn write_then_list_returns_all_identities() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let a = fresh_stored_operator("Ada");
        let b = fresh_stored_operator("Babbage");
        let c = fresh_stored_operator("Curie");
        registry.write(&a).unwrap();
        registry.write(&b).unwrap();
        registry.write(&c).unwrap();

        let mut listed = registry.list().unwrap();
        listed.sort_by_key(|entry| entry.identity.identity_id.to_string());
        let mut expected = [a, b, c];
        expected.sort_by_key(|entry| entry.identity.identity_id.to_string());
        assert_eq!(listed, expected);
    }

    #[test]
    fn list_on_empty_directory_returns_empty() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        assert!(registry.list().unwrap().is_empty());
    }

    #[test]
    fn lookup_for_missing_id_returns_none() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let id = IdentityId::new().unwrap();
        assert!(registry.lookup(id).unwrap().is_none());
    }

    #[test]
    fn write_overwrites_existing_entry() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let mut stored = fresh_stored_operator("Ada");
        registry.write(&stored).unwrap();
        stored.identity.display_name = "Ada Lovelace".to_owned();
        registry.write(&stored).unwrap();
        let loaded = registry
            .lookup(stored.identity.identity_id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.identity.display_name, "Ada Lovelace");

        // overwrite must not leave a sibling .toml behind.
        let toml_count = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.path().extension().and_then(OsStr::to_str) == Some(REGISTRY_FILE_EXTENSION)
            })
            .count();
        assert_eq!(toml_count, 1);
    }

    #[test]
    fn orphan_tmp_file_does_not_break_list() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let stored = fresh_stored_operator("Ada");
        registry.write(&stored).unwrap();

        let orphan = dir.path().join("interrupted-write.tmp");
        fs::write(&orphan, b"<half written>").unwrap();

        let listed = registry.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].identity.identity_id, stored.identity.identity_id);
    }

    #[test]
    fn list_rejects_filename_mismatch() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let stored = fresh_stored_operator("Ada");
        let body = toml::to_string(&RegistryFile {
            identity: stored.identity.clone(),
            keys: stored.key_records.clone(),
        })
        .unwrap();
        let wrong_id = IdentityId::new().unwrap();
        let wrong_path = dir
            .path()
            .join(format!("{wrong_id}.{REGISTRY_FILE_EXTENSION}"));
        fs::write(&wrong_path, body).unwrap();

        let err = registry.list().unwrap_err();
        let RegistryError::FilenameMismatch {
            from_filename,
            from_content,
            ..
        } = err
        else {
            panic!("expected FilenameMismatch, got {err:?}");
        };
        assert_eq!(from_filename, wrong_id);
        assert_eq!(from_content, stored.identity.identity_id);
    }

    #[test]
    fn lookup_rejects_filename_mismatch() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let stored = fresh_stored_operator("Ada");
        let body = toml::to_string(&RegistryFile {
            identity: stored.identity.clone(),
            keys: stored.key_records.clone(),
        })
        .unwrap();
        let wrong_id = IdentityId::new().unwrap();
        let wrong_path = dir
            .path()
            .join(format!("{wrong_id}.{REGISTRY_FILE_EXTENSION}"));
        fs::write(&wrong_path, body).unwrap();

        let err = registry.lookup(wrong_id).unwrap_err();
        assert!(matches!(err, RegistryError::FilenameMismatch { .. }));
    }

    #[test]
    fn list_rejects_oversize_file() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let id = IdentityId::new().unwrap();
        let path = dir.path().join(format!("{id}.{REGISTRY_FILE_EXTENSION}"));
        let junk = vec![b'x'; usize::try_from(MAX_REGISTRY_FILE_BYTES + 1024).unwrap()];
        fs::write(&path, junk).unwrap();

        let err = registry.list().unwrap_err();
        let RegistryError::FileTooLargeStat { size, .. } = err else {
            panic!("expected FileTooLargeStat, got {err:?}");
        };
        assert!(size > MAX_REGISTRY_FILE_BYTES);
    }

    #[test]
    fn list_rejects_symlinked_entries() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let stored = fresh_stored_operator("Ada");
        registry.write(&stored).unwrap();

        let outside = tempdir().unwrap();
        let target = outside.path().join("evil.toml");
        fs::write(&target, b"identity = {}\n").unwrap();
        let link = dir.path().join(format!(
            "{}.{REGISTRY_FILE_EXTENSION}",
            IdentityId::new().unwrap()
        ));
        symlink(&target, &link).unwrap();

        let err = registry.list().unwrap_err();
        assert!(
            matches!(err, RegistryError::SymlinkedRegistryFile { .. }),
            "expected SymlinkedRegistryFile, got {err:?}",
        );
    }

    #[test]
    fn lookup_rejects_symlink_for_target_id() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let outside = tempdir().unwrap();
        let target = outside.path().join("evil.toml");
        fs::write(&target, b"identity = {}\n").unwrap();
        let id = IdentityId::new().unwrap();
        let link = dir.path().join(format!("{id}.{REGISTRY_FILE_EXTENSION}"));
        symlink(&target, &link).unwrap();

        let err = registry.lookup(id).unwrap_err();
        assert!(matches!(err, RegistryError::SymlinkedRegistryFile { .. }));
    }

    #[test]
    fn list_surfaces_lifecycle_violation_as_parse_error() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let id = IdentityId::new().unwrap();
        let key_id = KeyId::new().unwrap();
        let public_key = fresh_public_key();
        let body = format!(
            r#"
[identity]
identity_id = "{id}"
identity_type = "operator"
display_name = "Ada"
created_at = "2026-06-01T00:00:00Z"
expires_at = "2026-06-01T00:00:00Z"

[[keys]]
key_id = "{key_id}"
identity_id = "{id}"
public_key = "{}"
status = "active"
valid_from = "2026-06-01T00:00:00Z"
"#,
            public_key.to_base64(),
        );
        let path = dir.path().join(format!("{id}.{REGISTRY_FILE_EXTENSION}"));
        fs::write(&path, body).unwrap();

        let err = registry.list().unwrap_err();
        assert!(
            matches!(err, RegistryError::Parse { .. }),
            "expected Parse, got {err:?}",
        );
    }

    #[test]
    fn list_rejects_key_record_with_wrong_identity() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let stored = fresh_stored_operator("Ada");
        let other_identity = IdentityId::new().unwrap();
        let mismatched_key = KeyRecord {
            key_id: KeyId::new().unwrap(),
            identity_id: other_identity,
            public_key: fresh_public_key(),
            valid_from: OffsetDateTime::now_utc(),
            state: KeyState::Active,
        };
        let body = toml::to_string(&RegistryFile {
            identity: stored.identity.clone(),
            keys: vec![mismatched_key],
        })
        .unwrap();
        let path = dir.path().join(format!(
            "{}.{REGISTRY_FILE_EXTENSION}",
            stored.identity.identity_id,
        ));
        fs::write(&path, body).unwrap();

        let err = registry.list().unwrap_err();
        assert!(
            matches!(err, RegistryError::KeyIdentityMismatch { .. }),
            "expected KeyIdentityMismatch, got {err:?}",
        );
    }

    #[test]
    fn new_rejects_mismatched_key() {
        let identity = Identity::new_operator("Ada".to_owned()).unwrap();
        let other = IdentityId::new().unwrap();
        let key = KeyRecord {
            key_id: KeyId::new().unwrap(),
            identity_id: other,
            public_key: fresh_public_key(),
            valid_from: OffsetDateTime::now_utc(),
            state: KeyState::Active,
        };
        let err = StoredIdentity::new(identity, key).unwrap_err();
        assert!(matches!(err, RegistryError::KeyIdentityMismatch { .. }));
    }

    #[test]
    fn list_rejects_file_with_no_keys() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let identity = Identity::new_operator("Ada".to_owned()).unwrap();
        let body = toml::to_string(&RegistryFile {
            identity: identity.clone(),
            keys: Vec::new(),
        })
        .unwrap();
        let path = dir.path().join(format!(
            "{}.{REGISTRY_FILE_EXTENSION}",
            identity.identity_id,
        ));
        fs::write(&path, body).unwrap();

        let err = registry.list().unwrap_err();
        assert!(
            matches!(err, RegistryError::NoKeys { .. }),
            "expected NoKeys, got {err:?}",
        );
    }

    #[test]
    fn resolve_default_data_dir_uses_xdg_data_home_when_set() {
        let resolved = resolve_default_data_dir(
            Some(OsStr::new("/srv/reeve-xdg")),
            Some(OsStr::new("/home/operator")),
        )
        .unwrap();
        assert_eq!(resolved, PathBuf::from("/srv/reeve-xdg/reeve/identities"));
    }

    #[test]
    fn resolve_default_data_dir_falls_back_to_home_when_xdg_unset() {
        let resolved = resolve_default_data_dir(None, Some(OsStr::new("/home/operator"))).unwrap();
        assert_eq!(
            resolved,
            PathBuf::from("/home/operator/.local/share/reeve/identities"),
        );
    }

    #[test]
    fn resolve_default_data_dir_falls_back_to_home_when_xdg_empty() {
        let resolved =
            resolve_default_data_dir(Some(OsStr::new("")), Some(OsStr::new("/home/operator")))
                .unwrap();
        assert_eq!(
            resolved,
            PathBuf::from("/home/operator/.local/share/reeve/identities"),
        );
    }

    #[test]
    fn resolve_default_data_dir_errors_when_xdg_and_home_both_unset() {
        let err = resolve_default_data_dir(None, None).unwrap_err();
        assert!(matches!(err, RegistryError::MissingHome));
    }

    #[test]
    fn write_is_atomic_via_rename() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let stored = fresh_stored_operator("Ada");
        registry.write(&stored).unwrap();
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        let toml_files: Vec<_> = entries
            .iter()
            .filter(|name| {
                Path::new(name).extension().and_then(OsStr::to_str) == Some(REGISTRY_FILE_EXTENSION)
            })
            .collect();
        assert_eq!(toml_files.len(), 1);
        assert!(toml_files[0].contains(&stored.identity.identity_id.to_string()));
    }

    #[test]
    fn list_skips_non_toml_files() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        fs::write(dir.path().join("README"), b"not an identity").unwrap();
        fs::write(dir.path().join("notes.txt"), b"scratch").unwrap();
        assert!(registry.list().unwrap().is_empty());
    }

    // P1: 64 KiB boundary. A file at exactly MAX_REGISTRY_FILE_BYTES with
    // valid, parseable TOML content must clear the size gate and load
    // cleanly. The contract is "size cap rejects only strictly above MAX".
    #[test]
    fn list_accepts_file_at_exactly_cap() {
        // Pad inside an unknown TOML key (`padding = "xxx..."\n`) so the
        // document is guaranteed parseable. `RegistryFile` does not opt
        // into `deny_unknown_fields`, so serde silently ignores the extra
        // top-level key.
        const PADDING_KEY: &str = "padding = \"";
        const PADDING_TAIL: &str = "\"\n";

        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let stored = fresh_stored_operator("Ada");
        let base = toml::to_string(&RegistryFile {
            identity: stored.identity.clone(),
            keys: stored.key_records.clone(),
        })
        .unwrap();
        let cap = usize::try_from(MAX_REGISTRY_FILE_BYTES).unwrap();
        let overhead = PADDING_KEY.len() + PADDING_TAIL.len();
        assert!(
            base.len() + overhead <= cap,
            "base + padding overhead must fit in cap",
        );
        let inner_len = cap - base.len() - overhead;
        let mut body = String::with_capacity(cap);
        body.push_str(PADDING_KEY);
        for _ in 0..inner_len {
            body.push('x');
        }
        body.push_str(PADDING_TAIL);
        body.push_str(&base);
        assert_eq!(body.len(), cap);

        let path = dir.path().join(format!(
            "{}.{REGISTRY_FILE_EXTENSION}",
            stored.identity.identity_id,
        ));
        fs::write(&path, &body).unwrap();
        let entries = registry.list().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].identity.identity_id, stored.identity.identity_id);
    }

    #[test]
    fn list_rejects_file_one_byte_past_cap() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let id = IdentityId::new().unwrap();
        let path = dir.path().join(format!("{id}.{REGISTRY_FILE_EXTENSION}"));
        let size = usize::try_from(MAX_REGISTRY_FILE_BYTES + 1).unwrap();
        let body = vec![b'x'; size];
        fs::write(&path, body).unwrap();

        let err = registry.list().unwrap_err();
        let RegistryError::FileTooLargeStat { size: reported, .. } = err else {
            panic!("expected FileTooLargeStat, got {err:?}");
        };
        assert_eq!(reported, MAX_REGISTRY_FILE_BYTES + 1);
    }

    // P2: filename that does not parse as any UUID must surface
    // InvalidFilename via list (lookup builds the filename from a typed id
    // and cannot reach this branch).
    #[test]
    fn list_rejects_non_uuid_toml_filename() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let stored = fresh_stored_operator("Ada");
        let body = toml::to_string(&RegistryFile {
            identity: stored.identity.clone(),
            keys: stored.key_records.clone(),
        })
        .unwrap();
        let bad_path = dir.path().join("not-a-uuid.toml");
        fs::write(&bad_path, body).unwrap();

        let err = registry.list().unwrap_err();
        assert!(
            matches!(err, RegistryError::InvalidFilename { .. }),
            "expected InvalidFilename, got {err:?}",
        );
    }

    // P3: filename whose stem is a syntactically valid but non-v7 UUID
    // (here v4) must surface InvalidIdentityId via list.
    #[test]
    fn list_rejects_non_v7_uuid_filename() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let stored = fresh_stored_operator("Ada");
        let body = toml::to_string(&RegistryFile {
            identity: stored.identity.clone(),
            keys: stored.key_records.clone(),
        })
        .unwrap();
        // A v4 UUID literal: the stem parses as a Uuid but fails the v7
        // newtype check.
        let bad_path = dir.path().join(format!(
            "00000000-0000-4000-8000-000000000000.{REGISTRY_FILE_EXTENSION}"
        ));
        fs::write(&bad_path, body).unwrap();

        let err = registry.list().unwrap_err();
        assert!(
            matches!(err, RegistryError::InvalidIdentityId { .. }),
            "expected InvalidIdentityId, got {err:?}",
        );
    }

    // P5: open() rejects a path that exists and is a regular file with
    // NotADirectory.
    #[test]
    fn open_rejects_non_directory_path() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("not-a-dir");
        fs::write(&file_path, b"hello").unwrap();
        let err = IdentityRegistry::open(file_path).unwrap_err();
        assert!(
            matches!(err, RegistryError::NotADirectory { .. }),
            "expected NotADirectory, got {err:?}",
        );
    }

    // P6: open() rejects a symlinked data_dir even when the target is a
    // valid directory.
    #[test]
    fn open_rejects_symlinked_data_dir() {
        let outer = tempdir().unwrap();
        let real_dir = outer.path().join("target");
        fs::create_dir_all(&real_dir).unwrap();
        let link = outer.path().join("data_dir");
        symlink(&real_dir, &link).unwrap();
        let err = IdentityRegistry::open(link).unwrap_err();
        assert!(
            matches!(err, RegistryError::SymlinkedDataDir { .. }),
            "expected SymlinkedDataDir, got {err:?}",
        );
    }

    // Y1: a non-regular file (here a Unix domain socket) at a registry
    // path must be rejected at metadata time, before any read can block on
    // the FIFO/socket.
    #[cfg(unix)]
    #[test]
    fn list_rejects_non_regular_file() {
        use std::os::unix::net::UnixListener;

        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let id = IdentityId::new().unwrap();
        let socket_path = dir.path().join(format!("{id}.{REGISTRY_FILE_EXTENSION}"));
        let _listener = UnixListener::bind(&socket_path).unwrap();

        let err = registry.list().unwrap_err();
        assert!(
            matches!(err, RegistryError::NotARegularFile { .. }),
            "expected NotARegularFile, got {err:?}",
        );
    }

    // T2: directory at an entry path must hit the is_file() gate.
    #[test]
    fn list_rejects_directory_at_entry_path() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let id = IdentityId::new().unwrap();
        let entry = dir.path().join(format!("{id}.{REGISTRY_FILE_EXTENSION}"));
        fs::create_dir_all(&entry).unwrap();
        let err = registry.list().unwrap_err();
        assert!(
            matches!(err, RegistryError::NotARegularFile { .. }),
            "expected NotARegularFile, got {err:?}",
        );
    }

    // T1: a registry file whose body is not valid UTF-8 must surface as a
    // typed NonUtf8Body error after the metadata + size gates pass.
    #[test]
    fn list_rejects_non_utf8_body_as_typed_error() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let id = IdentityId::new().unwrap();
        let path = dir.path().join(format!("{id}.{REGISTRY_FILE_EXTENSION}"));
        fs::write(&path, [0xFFu8]).unwrap();

        let err = registry.list().unwrap_err();
        assert!(
            matches!(err, RegistryError::NonUtf8Body { .. }),
            "expected NonUtf8Body, got {err:?}",
        );
    }

    #[test]
    fn toml_path_joins_data_dir_and_filename() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let id = IdentityId::new().unwrap();
        let path = registry.toml_path(id);
        assert_eq!(path, dir.path().join(format!("{id}.toml")));
    }

    // T1: a registry file whose path stem is not valid UTF-8 must surface
    // as NonUtf8Filename. The extension is ASCII "toml", so
    // `is_registry_file` lets it through; the stem is what fails to decode.
    //
    // The defense applies on every Unix; the test exercises it wherever
    // the kernel will accept the file.
    #[cfg(target_os = "linux")]
    #[test]
    fn list_rejects_non_utf8_filename() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let mut bytes = vec![0xFFu8];
        bytes.extend_from_slice(b".toml");
        let name = OsString::from_vec(bytes);
        let path = dir.path().join(name);
        fs::write(&path, b"identity = {}\n").unwrap();

        let err = registry.list().unwrap_err();
        assert!(
            matches!(err, RegistryError::NonUtf8Filename { .. }),
            "expected NonUtf8Filename, got {err:?}",
        );
    }

    // C1: an existing data_dir with permissions other than
    // REGISTRY_DIR_MODE must be rejected (not silently chmodded). Operator
    // misconfiguration should be visible.
    #[cfg(unix)]
    #[test]
    fn open_rejects_directory_with_wrong_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let target = dir.path().join("identities");
        fs::create_dir_all(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

        let err = IdentityRegistry::open(target).unwrap_err();
        let RegistryError::WrongDirectoryMode {
            actual, expected, ..
        } = err
        else {
            panic!("expected WrongDirectoryMode, got {err:?}");
        };
        assert_eq!(actual, 0o755);
        assert_eq!(expected, REGISTRY_DIR_MODE);
    }

    // D3 (paired with `list_rejects_file_with_no_keys`): v1 requires
    // exactly one key per file. More than one is a TooManyKeys, not a
    // silent acceptance.
    #[test]
    fn list_rejects_file_with_multiple_keys() {
        let dir = tempdir().unwrap();
        chmod_secure(dir.path());
        let registry = IdentityRegistry::open(dir.path().to_path_buf()).unwrap();
        let identity = Identity::new_operator("Ada".to_owned()).unwrap();
        let key_one = KeyRecord::new(identity.identity_id, fresh_public_key()).unwrap();
        let key_two = KeyRecord::new(identity.identity_id, fresh_public_key()).unwrap();
        let body = toml::to_string(&RegistryFile {
            identity: identity.clone(),
            keys: vec![key_one, key_two],
        })
        .unwrap();
        let path = dir.path().join(format!(
            "{}.{REGISTRY_FILE_EXTENSION}",
            identity.identity_id,
        ));
        fs::write(&path, body).unwrap();

        let err = registry.list().unwrap_err();
        let RegistryError::TooManyKeys { count, .. } = err else {
            panic!("expected TooManyKeys, got {err:?}");
        };
        assert_eq!(count, 2);
    }
}
