//! Per-agent registry: name → record persistence and per-agent keypair helpers.
//!
//! The registry is a single TOML file at
//! `~/.local/share/reeve/agents/registry.toml` (XDG + HOME fallback). Records
//! accumulate — stopped agents are never deleted; `register` upserts.
//!
//! Keypairs are stored separately as raw 32-byte seed files alongside the
//! agent's directory tree; they are loaded on demand via
//! [`generate_or_load_keypair`] rather than kept in memory here.
//!
//! Filesystem safety follows `specs/reeve-transport-security.md` §
//! Filesystem Safety: no symlink following, mode `0o700` for the parent
//! directory, mode `0o600` for key files, atomic writes via
//! tmp → fsync → rename.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use reeve_types::{IdentityId, Keypair};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use zeroize::Zeroizing;

use crate::fs_util::{
    atomic_write_file, ensure_directory, read_nofollow_bounded, resolve_reeve_data_root,
    set_nofollow, FsCheckError, XdgBaseError,
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Maximum size in bytes for the registry TOML file. Generous for a large
/// fleet; bounded to guard against decoder OOM.
const MAX_REGISTRY_FILE_BYTES: u64 = 1024 * 1024;

const REGISTRY_DIR_MODE: u32 = 0o700;
const REGISTRY_FILE_MODE: u32 = 0o600;
const KEY_SEED_LEN: usize = 32;
const _: () = assert!(
    KEY_SEED_LEN == ed25519_dalek::SECRET_KEY_LENGTH,
    "KEY_SEED_LEN must match ed25519_dalek::SECRET_KEY_LENGTH",
);

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors surfaced by agent registry and key-file operations.
///
/// Not `Clone` or `PartialEq` because [`io::Error`] is neither.
#[derive(Debug)]
pub enum AgentRegistryError {
    /// Underlying filesystem error (open, read, write, rename, mkdir).
    Io { path: PathBuf, source: io::Error },
    /// Failed to deserialize the registry TOML file.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// Failed to serialize the registry to TOML.
    Serialize {
        path: PathBuf,
        source: toml::ser::Error,
    },
    /// `default_registry_path` could not resolve `$HOME` and
    /// `$XDG_DATA_HOME` was unset.
    MissingHome,
    /// `$XDG_DATA_HOME` or `$HOME` is set to a relative path.
    RelativeDataDir {
        var_name: &'static str,
        path: PathBuf,
    },
    /// `update_status` was called for an agent name not in the registry.
    NotFound { name: String },
    /// Key file exists but is not exactly 32 bytes.
    InvalidKeyFile { path: PathBuf, len: u64 },
    /// Key file path is a symlink; the runtime refuses to follow it.
    SymlinkedKeyFile { path: PathBuf },
    /// Registry file path is a symlink; the runtime refuses to follow it.
    SymlinkedRegistryFile { path: PathBuf },
    /// Registry parent directory is a symlink; the runtime refuses to follow it.
    SymlinkedDataDir { path: PathBuf },
    /// Registry parent directory exists but is not a directory.
    NotADirectory { path: PathBuf },
    /// Registry parent directory has unexpected permissions.
    WrongDirectoryMode {
        path: PathBuf,
        actual: u32,
        expected: u32,
    },
    /// `register` was called with a record whose name fails validation.
    InvalidAgentName { name: String },
}

impl fmt::Display for AgentRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "agent registry IO at {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(
                    f,
                    "agent registry parse error at {}: {source}",
                    path.display()
                )
            }
            Self::Serialize { path, source } => {
                write!(
                    f,
                    "agent registry serialize error at {}: {source}",
                    path.display()
                )
            }
            Self::MissingHome => f.write_str(
                "agent registry default_registry_path requires HOME or XDG_DATA_HOME to be set",
            ),
            Self::RelativeDataDir { var_name, path } => write!(
                f,
                "${var_name} is a relative path ({}); the data directory must be absolute",
                path.display(),
            ),
            Self::NotFound { name } => {
                write!(f, "agent registry has no record for agent {name:?}")
            }
            Self::InvalidKeyFile { path, len } => write!(
                f,
                "agent key file at {} is {len} bytes; expected exactly {KEY_SEED_LEN}",
                path.display(),
            ),
            Self::SymlinkedKeyFile { path } => write!(
                f,
                "agent registry refuses to follow symlinked key file at {}",
                path.display(),
            ),
            Self::SymlinkedRegistryFile { path } => write!(
                f,
                "agent registry refuses to follow symlinked registry file at {}",
                path.display(),
            ),
            Self::SymlinkedDataDir { path } => write!(
                f,
                "agent registry refuses to follow symlinked data directory at {}",
                path.display(),
            ),
            Self::NotADirectory { path } => write!(
                f,
                "agent registry path {} exists but is not a directory",
                path.display(),
            ),
            Self::WrongDirectoryMode {
                path,
                actual,
                expected,
            } => write!(
                f,
                "agent registry directory at {} has mode 0o{actual:o}, expected 0o{expected:o}",
                path.display(),
            ),
            Self::InvalidAgentName { name } => write!(
                f,
                "invalid agent name {name:?}: names must not be empty, contain '/', '\\0', or be '.' or '..'",
            ),
        }
    }
}

impl std::error::Error for AgentRegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Serialize { source, .. } => Some(source),
            Self::MissingHome
            | Self::RelativeDataDir { .. }
            | Self::NotFound { .. }
            | Self::InvalidKeyFile { .. }
            | Self::SymlinkedKeyFile { .. }
            | Self::SymlinkedRegistryFile { .. }
            | Self::SymlinkedDataDir { .. }
            | Self::NotADirectory { .. }
            | Self::WrongDirectoryMode { .. }
            | Self::InvalidAgentName { .. } => None,
        }
    }
}

impl AgentRegistryError {
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

// ── AgentStatus ───────────────────────────────────────────────────────────────

/// Persisted lifecycle state of a registered agent.
///
/// Records are cumulative — a `Stopped` agent remains in the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Running,
    Stopped,
}

// ── ValidatedAgentName ────────────────────────────────────────────────────────

/// A validated agent name. Enforces that the name is non-empty, contains no
/// `/` or `\0`, and is neither `.` nor `..`.
#[derive(Debug, Clone)]
pub struct ValidatedAgentName(String);

impl ValidatedAgentName {
    /// Construct a `ValidatedAgentName`, returning
    /// [`AgentRegistryError::InvalidAgentName`] when the name fails validation.
    pub fn new(s: &str) -> Result<Self, AgentRegistryError> {
        crate::agent_fs::validate_agent_name(s)
            .map_err(|_| AgentRegistryError::InvalidAgentName { name: s.to_owned() })?;
        Ok(Self(s.to_owned()))
    }

    /// Borrow the inner string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for ValidatedAgentName {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for ValidatedAgentName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for ValidatedAgentName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for ValidatedAgentName {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ValidatedAgentName {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::new(&s).map_err(serde::de::Error::custom)
    }
}

// ── AgentRecord ───────────────────────────────────────────────────────────────

/// Persisted metadata for a registered agent.
///
/// Fully TOML-serializable. Keypair material is deliberately excluded —
/// `PrivateKey` does not implement `Serialize`. Load keypairs on demand via
/// [`generate_or_load_keypair`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    /// Human-readable role name (e.g., `"lead"`). Registry key.
    pub name: ValidatedAgentName,
    /// The agent's registered identity UUID.
    pub identity_id: IdentityId,
    /// Path to the agent's Maildir inbox root.
    pub inbox_dir: PathBuf,
    /// Optional persona label (e.g., `"maren"`).
    pub persona_name: Option<String>,
    /// Wall-clock time the agent was first spawned.
    #[serde(with = "time::serde::rfc3339")]
    pub spawned_at: OffsetDateTime,
    /// Last-written lifecycle state.
    pub status: AgentStatus,
    /// Machine-readable reason the agent is `Stopped`, when one applies (e.g.
    /// `"profile_missing"` after a rehydration that could not recover a
    /// capability profile). `None` for running agents and for stops with no
    /// recorded reason. Absent in records written before this field existed.
    #[serde(default)]
    pub stopped_reason: Option<String>,
}

// ── Private TOML shape ────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    records: Vec<AgentRecord>,
}

// ── AgentRegistry ─────────────────────────────────────────────────────────────

/// Cumulative on-disk store of registered agents, keyed by name.
///
/// `Clone` is intentionally not derived: callers that need to share a handle
/// should wrap in `Arc<AgentRegistry>`, making the share explicit.
#[derive(Debug)]
pub struct AgentRegistry {
    registry_path: PathBuf,
    records: HashMap<String, AgentRecord>,
}

impl AgentRegistry {
    /// Open (or create) the registry at `path`.
    ///
    /// If the file does not exist the registry starts empty. The parent
    /// directory is created with mode `0o700` if absent; an existing directory
    /// must already carry `0o700`.
    pub fn open(path: PathBuf) -> Result<Self, AgentRegistryError> {
        let parent = path.parent().ok_or_else(|| AgentRegistryError::Io {
            path: path.clone(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "registry path must have a parent directory",
            ),
        })?;
        ensure_directory(parent, REGISTRY_DIR_MODE).map_err(AgentRegistryError::from_fs)?;

        let records = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(AgentRegistryError::SymlinkedRegistryFile { path });
                }
                read_registry(&path)?
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(source) => return Err(AgentRegistryError::Io { path, source }),
        };

        Ok(Self {
            registry_path: path,
            records,
        })
    }

    /// Default registry path: `~/.local/share/reeve/agents/registry.toml`.
    ///
    /// Honors `$XDG_DATA_HOME` when set and non-empty; falls back to
    /// `$HOME/.local/share` otherwise. Relative values for either variable
    /// are rejected.
    pub fn default_registry_path() -> Result<PathBuf, AgentRegistryError> {
        resolve_default_registry_path(
            std::env::var_os("XDG_DATA_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        )
    }

    /// Insert or replace the record for `record.name`, then flush to disk
    /// atomically.
    pub fn register(&mut self, record: AgentRecord) -> Result<(), AgentRegistryError> {
        self.records.insert(record.name.0.clone(), record);
        self.flush()
    }

    /// Update the `status` field of the named agent and flush to disk,
    /// clearing any previously recorded `stopped_reason`.
    ///
    /// Returns [`AgentRegistryError::NotFound`] when `name` is not in the
    /// registry.
    pub fn update_status(
        &mut self,
        name: &str,
        status: AgentStatus,
    ) -> Result<(), AgentRegistryError> {
        self.set_status(name, status, None)
    }

    /// Mark the named agent `Stopped` with a machine-readable reason and flush
    /// to disk. Used by the rehydration path to record why an agent could not
    /// be re-launched (e.g. `"profile_missing"`).
    ///
    /// Returns [`AgentRegistryError::NotFound`] when `name` is not in the
    /// registry.
    pub fn update_stopped_with_reason(
        &mut self,
        name: &str,
        reason: impl Into<String>,
    ) -> Result<(), AgentRegistryError> {
        self.set_status(name, AgentStatus::Stopped, Some(reason.into()))
    }

    fn set_status(
        &mut self,
        name: &str,
        status: AgentStatus,
        stopped_reason: Option<String>,
    ) -> Result<(), AgentRegistryError> {
        let record = self
            .records
            .get_mut(name)
            .ok_or_else(|| AgentRegistryError::NotFound {
                name: name.to_owned(),
            })?;
        record.status = status;
        record.stopped_reason = stopped_reason;
        self.flush()
    }

    /// Remove the record for `name` and flush to disk.
    ///
    /// Returns [`AgentRegistryError::NotFound`] when `name` is not in the
    /// registry. Does not touch the agent's on-disk directory tree.
    pub fn remove(&mut self, name: &str) -> Result<(), AgentRegistryError> {
        if self.records.remove(name).is_none() {
            return Err(AgentRegistryError::NotFound {
                name: name.to_owned(),
            });
        }
        self.flush()
    }

    /// Look up a record by name. In-memory only — no I/O.
    pub fn lookup(&self, name: &str) -> Option<&AgentRecord> {
        self.records.get(name)
    }

    /// Iterate all records in unspecified order.
    pub fn list(&self) -> impl Iterator<Item = &AgentRecord> {
        self.records.values()
    }

    fn flush(&self) -> Result<(), AgentRegistryError> {
        let file = RegistryFile {
            records: self.records.values().cloned().collect(),
        };
        let body = toml::to_string(&file).map_err(|source| AgentRegistryError::Serialize {
            path: self.registry_path.clone(),
            source,
        })?;
        let parent = self
            .registry_path
            .parent()
            .ok_or_else(|| AgentRegistryError::Io {
                path: self.registry_path.clone(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "registry path must have a parent directory",
                ),
            })?;
        atomic_write(parent, &self.registry_path, body.as_bytes())
    }
}

// ── generate_or_load_keypair ──────────────────────────────────────────────────

/// Load an agent keypair from a 32-byte seed file, or generate and persist one.
///
/// If `path` does not exist: generates a fresh keypair, writes the 32-byte
/// seed atomically (tmp → fsync → rename, mode `0o600`), and returns it.
///
/// If `path` exists: opens it with `O_NOFOLLOW`, verifies it is not a symlink,
/// checks that it contains exactly 32 bytes, reads the seed, and reconstructs
/// the keypair. A file of any other length surfaces as
/// [`AgentRegistryError::InvalidKeyFile`].
pub fn generate_or_load_keypair(path: &Path) -> Result<Keypair, AgentRegistryError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(AgentRegistryError::SymlinkedKeyFile {
                    path: path.to_path_buf(),
                });
            }
            let len = metadata.len();
            if len != u64::try_from(KEY_SEED_LEN).unwrap_or(u64::MAX) {
                return Err(AgentRegistryError::InvalidKeyFile {
                    path: path.to_path_buf(),
                    len,
                });
            }
            load_keypair_from_file(path)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => generate_and_persist_keypair(path),
        Err(source) => Err(AgentRegistryError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

fn load_keypair_from_file(path: &Path) -> Result<Keypair, AgentRegistryError> {
    let mut options = OpenOptions::new();
    options.read(true);
    set_nofollow(&mut options);
    let mut file = options
        .open(path)
        .map_err(|source| AgentRegistryError::Io {
            path: path.to_path_buf(),
            source,
        })?;

    let mut seed = Zeroizing::new([0u8; KEY_SEED_LEN]);
    let n = file
        .read(seed.as_mut())
        .map_err(|source| AgentRegistryError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    // The stat-time check already verified len == KEY_SEED_LEN. A short read
    // here means a concurrent truncation or platform oddity — surface it.
    if n != KEY_SEED_LEN {
        return Err(AgentRegistryError::InvalidKeyFile {
            path: path.to_path_buf(),
            len: u64::try_from(n).unwrap_or(u64::MAX),
        });
    }

    Ok(Keypair::from_seed_bytes(&seed))
}

fn generate_and_persist_keypair(path: &Path) -> Result<Keypair, AgentRegistryError> {
    let keypair = Keypair::generate();
    let seed = keypair.private().to_seed_bytes();
    let parent = path.parent().ok_or_else(|| AgentRegistryError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "key file path must have a parent directory",
        ),
    })?;
    atomic_write(parent, path, seed.as_slice())?;
    Ok(keypair)
}

fn read_registry(path: &Path) -> Result<HashMap<String, AgentRecord>, AgentRegistryError> {
    let body = read_nofollow_bounded(path, MAX_REGISTRY_FILE_BYTES).map_err(|source| {
        AgentRegistryError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let file: RegistryFile = toml::from_str(&body).map_err(|source| AgentRegistryError::Parse {
        path: path.to_path_buf(),
        source,
    })?;

    let mut map = HashMap::with_capacity(file.records.len());
    for record in file.records {
        map.insert(record.name.0.clone(), record);
    }
    Ok(map)
}

fn atomic_write(dir: &Path, target: &Path, body: &[u8]) -> Result<(), AgentRegistryError> {
    atomic_write_file(target, dir, body, REGISTRY_FILE_MODE).map_err(|source| {
        AgentRegistryError::Io {
            path: target.to_path_buf(),
            source,
        }
    })
}

fn resolve_default_registry_path(
    xdg_data_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, AgentRegistryError> {
    let data_root = resolve_reeve_data_root(xdg_data_home, home).map_err(|e| match e {
        XdgBaseError::MissingHome => AgentRegistryError::MissingHome,
        XdgBaseError::RelativeDir { var_name, path } => {
            AgentRegistryError::RelativeDataDir { var_name, path }
        }
    })?;
    Ok(data_root.join("agents").join("registry.toml"))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::fs;
    use std::os::unix::fs::symlink;

    use time::OffsetDateTime;

    /// Returns the registry path for a given data directory, using the same
    /// layout as `AgentRegistry::default_registry_path()` with `XDG_DATA_HOME`
    /// set to `data_dir`. Use this in tests instead of hardcoding the path.
    pub(crate) fn registry_path_for_data_dir(data_dir: &Path) -> PathBuf {
        resolve_default_registry_path(Some(data_dir.as_os_str()), None).unwrap()
    }

    fn sample_record(name: &str) -> AgentRecord {
        AgentRecord {
            name: ValidatedAgentName::new(name).unwrap(),
            identity_id: IdentityId::new().unwrap(),
            inbox_dir: PathBuf::from(format!("/data/agents/{name}/inbox")),
            persona_name: None,
            spawned_at: OffsetDateTime::now_utc(),
            status: AgentStatus::Running,
            stopped_reason: None,
        }
    }

    fn registry_at() -> (AgentRegistry, tempfile::TempDir) {
        let dir = crate::test_support::secure_dir();
        let registry = AgentRegistry::open(dir.path().join("registry.toml")).unwrap();
        (registry, dir)
    }

    // T1: missing registry file → open succeeds with empty registry.
    #[test]
    fn open_missing_file_returns_empty_registry() {
        let (registry, _tmp) = registry_at();
        assert_eq!(registry.list().count(), 0);
    }

    // T2: register then lookup round-trips name and identity_id.
    #[test]
    fn register_then_lookup_round_trips() {
        let (mut registry, _tmp) = registry_at();
        let record = sample_record("lead");
        let id = record.identity_id;
        registry.register(record).unwrap();
        let found = registry.lookup("lead").unwrap();
        assert_eq!(found.identity_id, id);
    }

    // T3: register upserts — second write to same name replaces.
    #[test]
    fn register_upserts_existing_record() {
        let (mut registry, _tmp) = registry_at();
        registry.register(sample_record("lead")).unwrap();
        let mut updated = sample_record("lead");
        updated.status = AgentStatus::Stopped;
        registry.register(updated).unwrap();
        assert_eq!(registry.list().count(), 1);
        assert_eq!(
            registry.lookup("lead").unwrap().status,
            AgentStatus::Stopped,
        );
    }

    // T4: update_status on existing record changes status.
    #[test]
    fn update_status_changes_existing_record() {
        let (mut registry, _tmp) = registry_at();
        registry.register(sample_record("lead")).unwrap();
        registry
            .update_status("lead", AgentStatus::Stopped)
            .unwrap();
        assert_eq!(
            registry.lookup("lead").unwrap().status,
            AgentStatus::Stopped,
        );
    }

    // T4b: update_stopped_with_reason records the reason; a later plain
    // update_status clears it (a reasonless transition has no stale reason).
    #[test]
    fn update_stopped_with_reason_sets_and_clears() {
        let (mut registry, _tmp) = registry_at();
        registry.register(sample_record("lead")).unwrap();

        registry
            .update_stopped_with_reason("lead", "profile_missing")
            .unwrap();
        let rec = registry.lookup("lead").unwrap();
        assert_eq!(rec.status, AgentStatus::Stopped);
        assert_eq!(rec.stopped_reason.as_deref(), Some("profile_missing"));

        registry
            .update_status("lead", AgentStatus::Running)
            .unwrap();
        let rec = registry.lookup("lead").unwrap();
        assert_eq!(rec.status, AgentStatus::Running);
        assert_eq!(rec.stopped_reason, None, "plain update clears the reason");
    }

    // T5: update_status on missing name returns NotFound.
    #[test]
    fn update_status_on_missing_name_returns_not_found() {
        let (mut registry, _tmp) = registry_at();
        let err = registry
            .update_status("ghost", AgentStatus::Stopped)
            .unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::NotFound { .. }),
            "expected NotFound, got {err:?}",
        );
    }

    // T6: list returns all records.
    #[test]
    fn list_returns_all_records() {
        let (mut registry, _tmp) = registry_at();
        registry.register(sample_record("lead")).unwrap();
        registry.register(sample_record("worker")).unwrap();
        let mut names: Vec<_> = registry.list().map(|r| r.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["lead", "worker"]);
    }

    // T7: TOML round-trip — write → reopen → same records.
    #[test]
    fn toml_round_trip_survives_reopen() {
        let tmp = crate::test_support::secure_dir();
        let path = tmp.path().join("registry.toml");
        let record = sample_record("lead");
        let id = record.identity_id;

        {
            let mut registry = AgentRegistry::open(path.clone()).unwrap();
            registry.register(record).unwrap();
        }

        let registry = AgentRegistry::open(path).unwrap();
        let found = registry.lookup("lead").unwrap();
        assert_eq!(found.identity_id, id);
        assert_eq!(found.status, AgentStatus::Running);
    }

    // T8: generate_or_load_keypair — new file creates key; second call loads
    // same public key (seed is stable).
    #[test]
    fn generate_or_load_keypair_stable_across_calls() {
        let tmp = crate::test_support::secure_dir();
        let path = tmp.path().join("identity.key");
        let kp1 = generate_or_load_keypair(&path).unwrap();
        let kp2 = generate_or_load_keypair(&path).unwrap();
        assert_eq!(
            kp1.public().to_bytes(),
            kp2.public().to_bytes(),
            "public key must be identical across calls",
        );
    }

    // T9: generate_or_load_keypair — file of wrong length → InvalidKeyFile.
    #[test]
    fn generate_or_load_keypair_wrong_length_returns_invalid_key_file() {
        let tmp = crate::test_support::secure_dir();
        let path = tmp.path().join("identity.key");
        fs::write(&path, b"too short").unwrap();
        let err = generate_or_load_keypair(&path).unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::InvalidKeyFile { .. }),
            "expected InvalidKeyFile, got {err:?}",
        );
    }

    // T10: generate_or_load_keypair — symlinked file → SymlinkedKeyFile.
    #[test]
    fn generate_or_load_keypair_symlink_returns_symlinked_key_file() {
        let tmp = crate::test_support::secure_dir();
        let target = tmp.path().join("real.key");
        fs::write(&target, vec![0u8; KEY_SEED_LEN]).unwrap();
        let link = tmp.path().join("identity.key");
        symlink(&target, &link).unwrap();
        let err = generate_or_load_keypair(&link).unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::SymlinkedKeyFile { .. }),
            "expected SymlinkedKeyFile, got {err:?}",
        );
    }

    // T11: AgentDirs::identity_key_path returns expected path.
    #[test]
    fn agent_dirs_identity_key_path() {
        use crate::AgentDirs;

        let tmp = crate::test_support::secure_dir();
        let dirs = AgentDirs::provision(tmp.path(), "lead").unwrap();
        assert_eq!(dirs.identity_key_path(), dirs.root().join("identity.key"));
    }

    // T12: default_registry_path uses XDG_DATA_HOME when set; the registry
    // lives under the shared reeve data root (`identities/`) so it shares an
    // ancestor with the per-agent inboxes it references.
    #[test]
    fn resolve_default_registry_path_uses_xdg_when_set() {
        let path = resolve_default_registry_path(
            Some(OsStr::new("/srv/data")),
            Some(OsStr::new("/home/op")),
        )
        .unwrap();
        assert_eq!(
            path,
            PathBuf::from("/srv/data/reeve/identities/agents/registry.toml"),
        );
    }

    // T13: default_registry_path falls back to HOME when XDG unset.
    #[test]
    fn resolve_default_registry_path_falls_back_to_home() {
        let path = resolve_default_registry_path(None, Some(OsStr::new("/home/op"))).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/home/op/.local/share/reeve/identities/agents/registry.toml"),
        );
    }

    // T14: relative XDG_DATA_HOME is rejected.
    #[test]
    fn resolve_default_registry_path_rejects_relative_xdg() {
        let err = resolve_default_registry_path(
            Some(OsStr::new("data/home")),
            Some(OsStr::new("/home/op")),
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                AgentRegistryError::RelativeDataDir {
                    var_name: "XDG_DATA_HOME",
                    ..
                }
            ),
            "expected RelativeDataDir, got {err:?}",
        );
    }

    // T15: missing HOME and XDG_DATA_HOME → MissingHome.
    #[test]
    fn resolve_default_registry_path_missing_home_returns_error() {
        let err = resolve_default_registry_path(None, None).unwrap_err();
        assert!(matches!(err, AgentRegistryError::MissingHome));
    }

    // T16: Display impls are non-empty and include contextual information.
    #[test]
    fn error_display_impls_are_informative() {
        let path = PathBuf::from("test/path");

        let io_err = AgentRegistryError::Io {
            path: path.clone(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        assert!(io_err.to_string().contains("test/path"), "Io: {io_err}");

        let not_found = AgentRegistryError::NotFound {
            name: "lead".to_owned(),
        };
        assert!(
            not_found.to_string().contains("lead"),
            "NotFound: {not_found}",
        );

        let invalid = AgentRegistryError::InvalidKeyFile {
            path: path.clone(),
            len: 16,
        };
        let s = invalid.to_string();
        assert!(s.contains("16"), "InvalidKeyFile (len): {s}");
        assert!(s.contains("32"), "InvalidKeyFile (expected): {s}");

        let symlink_err = AgentRegistryError::SymlinkedKeyFile { path };
        assert!(
            symlink_err.to_string().contains("test/path"),
            "SymlinkedKeyFile: {symlink_err}",
        );
    }

    // T17: stopped agents stay in the registry after status update (cumulative).
    #[test]
    fn stopped_agents_remain_in_registry() {
        let tmp = crate::test_support::secure_dir();
        let path = tmp.path().join("registry.toml");

        {
            let mut registry = AgentRegistry::open(path.clone()).unwrap();
            registry.register(sample_record("lead")).unwrap();
            registry
                .update_status("lead", AgentStatus::Stopped)
                .unwrap();
        }

        let registry = AgentRegistry::open(path).unwrap();
        let found = registry.lookup("lead");
        assert!(found.is_some(), "stopped agent must survive reopen");
        assert_eq!(found.unwrap().status, AgentStatus::Stopped);
    }

    // T18: registry file larger than the cap at stat time is rejected.
    #[test]
    fn open_rejects_oversized_registry_file() {
        let tmp = crate::test_support::secure_dir();
        let path = tmp.path().join("registry.toml");
        let junk = vec![b'x'; usize::try_from(MAX_REGISTRY_FILE_BYTES + 1).unwrap()];
        fs::write(&path, junk).unwrap();
        let err = AgentRegistry::open(path).unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::Io { .. }),
            "expected Io for oversize file, got {err:?}",
        );
    }

    // T19: registry file with invalid TOML surfaces as Parse.
    #[test]
    fn open_invalid_toml_returns_parse_error() {
        let tmp = crate::test_support::secure_dir();
        let path = tmp.path().join("registry.toml");
        fs::write(&path, b"not valid toml :::").unwrap();
        let err = AgentRegistry::open(path).unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::Parse { .. }),
            "expected Parse, got {err:?}",
        );
    }

    // T20a: open rejects a symlinked registry file.
    #[test]
    fn open_rejects_symlinked_registry_file() {
        let tmp = crate::test_support::secure_dir();
        let real_file = tmp.path().join("real_registry.toml");
        fs::write(&real_file, b"").unwrap();
        let link = tmp.path().join("registry.toml");
        symlink(&real_file, &link).unwrap();
        let err = AgentRegistry::open(link).unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::SymlinkedRegistryFile { .. }),
            "expected SymlinkedRegistryFile, got {err:?}",
        );
    }

    // T20: lookup returns None for an unknown name (no I/O).
    #[test]
    fn lookup_unknown_name_returns_none() {
        let (registry, _tmp) = registry_at();
        assert!(registry.lookup("nobody").is_none());
    }

    // T21: AgentStatus serializes to lowercase strings via serde.
    #[test]
    fn agent_status_serializes_lowercase() {
        // toml::to_string cannot serialize a bare enum value; use serde_json
        // as a serde-compatibility check for the rename_all attribute.
        let running = serde_json::to_string(&AgentStatus::Running).unwrap();
        let stopped = serde_json::to_string(&AgentStatus::Stopped).unwrap();
        assert_eq!(running, "\"running\"", "running must serialize lowercase");
        assert_eq!(stopped, "\"stopped\"", "stopped must serialize lowercase");
    }

    // T22: persona_name round-trips through the registry.
    #[test]
    fn persona_name_survives_round_trip() {
        let tmp = crate::test_support::secure_dir();
        let path = tmp.path().join("registry.toml");
        let mut record = sample_record("lead");
        record.persona_name = Some("maren".to_owned());

        {
            let mut registry = AgentRegistry::open(path.clone()).unwrap();
            registry.register(record).unwrap();
        }

        let registry = AgentRegistry::open(path).unwrap();
        assert_eq!(
            registry.lookup("lead").unwrap().persona_name.as_deref(),
            Some("maren"),
        );
    }

    // T23: error Display for MissingHome and RelativeDataDir(HOME).
    #[test]
    fn error_display_missing_home_and_relative_dir() {
        let s = AgentRegistryError::MissingHome.to_string();
        assert!(!s.is_empty(), "MissingHome display must be non-empty");

        let rel = AgentRegistryError::RelativeDataDir {
            var_name: "HOME",
            path: PathBuf::from("relative/path"),
        };
        let s = rel.to_string();
        assert!(
            s.contains("HOME"),
            "RelativeDataDir display must name the var: {s}"
        );
        assert!(
            s.contains("relative/path"),
            "RelativeDataDir display must include path: {s}",
        );

        let parse_err = AgentRegistryError::Parse {
            path: PathBuf::from("p"),
            source: toml::from_str::<toml::Value>(":::").unwrap_err(),
        };
        let s = parse_err.to_string();
        assert!(!s.is_empty(), "Parse display must be non-empty");

        let ser_err = AgentRegistryError::Serialize {
            path: PathBuf::from("p"),
            source: toml::to_string(&f32::INFINITY).unwrap_err(),
        };
        let s = ser_err.to_string();
        assert!(!s.is_empty(), "Serialize display must be non-empty");
    }

    // T24: error::source() returns Some for Parse and Serialize variants.
    #[test]
    fn error_source_chain_for_parse_and_serialize() {
        use std::error::Error as _;

        let parse_err = AgentRegistryError::Parse {
            path: PathBuf::from("p"),
            source: toml::from_str::<toml::Value>(":::").unwrap_err(),
        };
        assert!(parse_err.source().is_some(), "Parse source must be Some");

        let io_err = AgentRegistryError::Io {
            path: PathBuf::from("p"),
            source: io::Error::from(io::ErrorKind::NotFound),
        };
        assert!(io_err.source().is_some(), "Io source must be Some");

        let not_found = AgentRegistryError::NotFound {
            name: "x".to_owned(),
        };
        assert!(not_found.source().is_none(), "NotFound source must be None");
    }

    // T25: generate_or_load_keypair creates key file with exactly 32 bytes.
    #[test]
    fn generate_or_load_keypair_writes_32_byte_file() {
        let tmp = crate::test_support::secure_dir();
        let path = tmp.path().join("identity.key");
        generate_or_load_keypair(&path).unwrap();
        let len = fs::metadata(&path).unwrap().len();
        assert_eq!(len, u64::try_from(KEY_SEED_LEN).unwrap());
    }

    // T26: from_fs mapping covers Symlink, NotADirectory, and WrongMode.
    #[test]
    fn from_fs_covers_all_variants() {
        let path = PathBuf::from("p");

        let sym = AgentRegistryError::from_fs(FsCheckError::Symlink { path: path.clone() });
        assert!(
            matches!(sym, AgentRegistryError::SymlinkedDataDir { .. }),
            "Symlink → SymlinkedDataDir: {sym:?}",
        );

        let not_dir =
            AgentRegistryError::from_fs(FsCheckError::NotADirectory { path: path.clone() });
        assert!(
            matches!(not_dir, AgentRegistryError::NotADirectory { .. }),
            "NotADirectory → NotADirectory: {not_dir:?}",
        );

        let wrong_mode = AgentRegistryError::from_fs(FsCheckError::WrongMode {
            path: path.clone(),
            actual: 0o755,
            expected: 0o700,
        });
        assert!(
            matches!(wrong_mode, AgentRegistryError::WrongDirectoryMode { .. }),
            "WrongMode → WrongDirectoryMode: {wrong_mode:?}",
        );

        let io = AgentRegistryError::from_fs(FsCheckError::Io {
            path,
            source: io::Error::from(io::ErrorKind::Other),
        });
        assert!(
            matches!(io, AgentRegistryError::Io { .. }),
            "Io → Io: {io:?}",
        );
    }

    // T27: relative HOME is rejected.
    #[test]
    fn resolve_default_registry_path_rejects_relative_home() {
        let err =
            resolve_default_registry_path(None, Some(OsStr::new("home/operator"))).unwrap_err();
        assert!(
            matches!(
                err,
                AgentRegistryError::RelativeDataDir {
                    var_name: "HOME",
                    ..
                }
            ),
            "expected RelativeDataDir(HOME), got {err:?}",
        );
    }

    // T28 (m1): ValidatedAgentName rejects an empty name with InvalidAgentName.
    #[test]
    fn validated_agent_name_rejects_empty_name() {
        let err = ValidatedAgentName::new("").unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::InvalidAgentName { .. }),
            "expected InvalidAgentName for empty name, got {err:?}",
        );
    }

    // T29 (m1): ValidatedAgentName rejects a name containing a slash with InvalidAgentName.
    #[test]
    fn validated_agent_name_rejects_name_with_slash() {
        let err = ValidatedAgentName::new("foo/bar").unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::InvalidAgentName { .. }),
            "expected InvalidAgentName for slash in name, got {err:?}",
        );
    }

    // T30 (m3): open rejects a symlinked parent directory with SymlinkedDataDir.
    #[test]
    #[cfg(unix)]
    fn open_rejects_symlinked_parent_dir() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = crate::test_support::secure_dir();
        // Create a real directory to serve as the symlink target.
        let real_dir = tmp.path().join("real_agents");
        fs::create_dir_all(&real_dir).unwrap();
        fs::set_permissions(&real_dir, fs::Permissions::from_mode(REGISTRY_DIR_MODE)).unwrap();
        // Symlink the directory that would be the registry parent.
        let link_dir = tmp.path().join("linked_agents");
        symlink(&real_dir, &link_dir).unwrap();
        let registry_path = link_dir.join("registry.toml");
        let err = AgentRegistry::open(registry_path).unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::SymlinkedDataDir { .. }),
            "expected SymlinkedDataDir for symlinked parent, got {err:?}",
        );
    }

    // T31: ValidatedAgentName rejects "." name.
    #[test]
    fn validated_agent_name_rejects_dot_name() {
        let err = ValidatedAgentName::new(".").unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::InvalidAgentName { .. }),
            "expected InvalidAgentName for '.' name, got {err:?}",
        );
    }

    // T32: ValidatedAgentName rejects ".." name.
    #[test]
    fn validated_agent_name_rejects_dotdot_name() {
        let err = ValidatedAgentName::new("..").unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::InvalidAgentName { .. }),
            "expected InvalidAgentName for '..' name, got {err:?}",
        );
    }

    // T33: ValidatedAgentName rejects name with NUL byte.
    #[test]
    fn validated_agent_name_rejects_null_byte_name() {
        let err = ValidatedAgentName::new("foo\0bar").unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::InvalidAgentName { .. }),
            "expected InvalidAgentName for name with NUL byte, got {err:?}",
        );
    }

    // T33b: ValidatedAgentName rejects names containing control characters.
    #[test]
    fn validated_agent_name_rejects_newline() {
        assert!(ValidatedAgentName::new("name\nwith\nnewline").is_err());
    }

    #[test]
    fn validated_agent_name_rejects_carriage_return() {
        assert!(ValidatedAgentName::new("name\rwith\rcr").is_err());
    }

    #[test]
    fn validated_agent_name_rejects_escape() {
        assert!(ValidatedAgentName::new("name\x1bwith\x1bescape").is_err());
    }

    #[test]
    fn validated_agent_name_rejects_tab() {
        assert!(ValidatedAgentName::new("name\twith\ttab").is_err());
    }

    // T34: TOML with a semantically invalid name surfaces as Parse.
    //
    // Writes the TOML manually so the invalid name reaches the deserializer
    // path: read_registry → toml::from_str → ValidatedAgentName::Deserialize
    // → Self::new() → Err → serde::de::Error::custom → AgentRegistryError::Parse.
    #[test]
    fn open_invalid_agent_name_in_toml_returns_parse_error() {
        let tmp = crate::test_support::secure_dir();
        let path = tmp.path().join("registry.toml");
        // UUIDv7: version nibble = 7, variant bits = 10xx.
        let toml = concat!(
            "[[records]]\n",
            "name = \"foo/bar\"\n",
            "identity_id = \"01960000-0000-7000-8000-000000000000\"\n",
            "inbox_dir = \"/data/agents/foo/inbox\"\n",
            "spawned_at = \"2026-01-01T00:00:00Z\"\n",
            "status = \"stopped\"\n",
        );
        fs::write(&path, toml).unwrap();
        let err = AgentRegistry::open(path).unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::Parse { .. }),
            "expected Parse for invalid agent name in TOML, got {err:?}",
        );
    }

    // T35: registry parent path is a regular file → NotADirectory.
    #[test]
    #[cfg(unix)]
    fn open_rejects_file_at_parent_dir_path() {
        let tmp = crate::test_support::secure_dir();
        // Create a regular file where the parent directory should be.
        let parent = tmp.path().join("agents");
        fs::write(&parent, b"not a directory").unwrap();
        let registry_path = parent.join("registry.toml");
        let err = AgentRegistry::open(registry_path).unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::NotADirectory { .. }),
            "expected NotADirectory when parent is a file, got {err:?}",
        );
    }

    // T36: registry parent directory has mode 0o755 → WrongDirectoryMode.
    #[test]
    #[cfg(unix)]
    fn open_rejects_parent_dir_with_wrong_mode() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = crate::test_support::secure_dir();
        let parent = tmp.path().join("agents");
        fs::create_dir_all(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
        let registry_path = parent.join("registry.toml");
        let err = AgentRegistry::open(registry_path).unwrap_err();
        assert!(
            matches!(err, AgentRegistryError::WrongDirectoryMode { .. }),
            "expected WrongDirectoryMode for 0o755 parent, got {err:?}",
        );
    }
}
