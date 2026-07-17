//! System-actor registry: `name → (identity, inbox)` for runtime-internal
//! actors that are not model-backed agents (today: the estate coordinator).
//!
//! `AgentRegistry` exists to persist agent-shaped state — persona, lifecycle
//! status, spawn time — none of which applies to a system actor. Giving the
//! estate coordinator a slot there anyway (to reuse the same name→identity
//! lookup every sender uses) made it invisible-by-omission wherever code
//! walks "every agent" — the panopticon, `attach`, resume-on-restart — none
//! of which know to filter it out. This registry is the structural fix:
//! system actors live somewhere agent-walking code never looks, so nothing
//! has to remember to exclude them.
//!
//! Same filesystem-safety posture as [`crate::agent_registry`]: mode
//! `0o700` directories, no symlink following, atomic tmp → fsync → rename
//! writes.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use reeve_types::IdentityId;
use serde::{Deserialize, Serialize};

use crate::agent_fs::validate_agent_name;
use crate::fs_util::{atomic_write_file, ensure_directory, read_nofollow_bounded, FsCheckError};

const REGISTRY_DIR_MODE: u32 = 0o700;
const REGISTRY_FILE_MODE: u32 = 0o600;
/// Generous for a handful of system actors; bounded to guard against
/// decoder OOM, same rationale as `AgentRegistry`'s cap.
const MAX_REGISTRY_FILE_BYTES: u64 = 64 * 1024;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors surfaced by system-actor registry operations.
#[derive(Debug)]
pub enum SystemRegistryError {
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
    /// `register` was called with a name that fails agent-name validation
    /// (system-actor names share the same filesystem-safety constraints).
    InvalidName { name: String },
    /// A lookup targeted a name not in the registry.
    NotFound { name: String },
}

impl SystemRegistryError {
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

impl fmt::Display for SystemRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "system registry IO at {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(
                    f,
                    "system registry parse error at {}: {source}",
                    path.display()
                )
            }
            Self::Serialize { path, source } => {
                write!(
                    f,
                    "system registry serialize error at {}: {source}",
                    path.display()
                )
            }
            Self::SymlinkedRegistryFile { path } => write!(
                f,
                "system registry file at {} is a symlink; refusing to follow it",
                path.display()
            ),
            Self::SymlinkedDataDir { path } => write!(
                f,
                "system registry parent directory at {} is a symlink; refusing to follow it",
                path.display()
            ),
            Self::NotADirectory { path } => {
                write!(
                    f,
                    "system registry parent at {} is not a directory",
                    path.display()
                )
            }
            Self::WrongDirectoryMode {
                path,
                actual,
                expected,
            } => write!(
                f,
                "system registry parent at {} has mode {actual:o}, expected {expected:o}",
                path.display()
            ),
            Self::InvalidName { name } => write!(f, "invalid system-actor name {name:?}"),
            Self::NotFound { name } => write!(f, "no system actor named {name:?}"),
        }
    }
}

impl std::error::Error for SystemRegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Serialize { source, .. } => Some(source),
            Self::SymlinkedRegistryFile { .. }
            | Self::SymlinkedDataDir { .. }
            | Self::NotADirectory { .. }
            | Self::WrongDirectoryMode { .. }
            | Self::InvalidName { .. }
            | Self::NotFound { .. } => None,
        }
    }
}

// ── SystemActorRecord ─────────────────────────────────────────────────────────

/// Persisted metadata for a registered system actor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemActorRecord {
    /// Reserved name (e.g. `"estate"`). Registry key.
    pub name: String,
    /// The actor's registered identity UUID.
    pub identity_id: IdentityId,
    /// Path to the actor's Maildir inbox root.
    pub inbox_dir: PathBuf,
}

// ── Private TOML shape ────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    records: Vec<SystemActorRecord>,
}

// ── SystemRegistry ────────────────────────────────────────────────────────────

/// Cumulative on-disk store of registered system actors, keyed by name.
///
/// `Clone` is intentionally not derived: callers that need to share a handle
/// should wrap in `Arc<SystemRegistry>`, making the share explicit.
#[derive(Debug)]
pub struct SystemRegistry {
    registry_path: PathBuf,
    records: HashMap<String, SystemActorRecord>,
}

impl SystemRegistry {
    /// Open (or create) the registry at `path`.
    ///
    /// If the file does not exist the registry starts empty. The parent
    /// directory is created with mode `0o700` if absent; an existing
    /// directory must already carry `0o700`.
    pub fn open(path: PathBuf) -> Result<Self, SystemRegistryError> {
        let parent = path.parent().ok_or_else(|| SystemRegistryError::Io {
            path: path.clone(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "registry path must have a parent directory",
            ),
        })?;
        ensure_directory(parent, REGISTRY_DIR_MODE).map_err(SystemRegistryError::from_fs)?;

        let records = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(SystemRegistryError::SymlinkedRegistryFile { path });
                }
                read_registry(&path)?
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(source) => return Err(SystemRegistryError::Io { path, source }),
        };

        Ok(Self {
            registry_path: path,
            records,
        })
    }

    /// Insert or replace the record for `record.name`, then flush to disk
    /// atomically. Refuses names that fail agent-name validation.
    pub fn register(&mut self, record: SystemActorRecord) -> Result<(), SystemRegistryError> {
        validate_agent_name(&record.name).map_err(|_| SystemRegistryError::InvalidName {
            name: record.name.clone(),
        })?;
        self.records.insert(record.name.clone(), record);
        self.flush()
    }

    /// Look up a record by name. In-memory only — no I/O.
    pub fn lookup(&self, name: &str) -> Option<&SystemActorRecord> {
        self.records.get(name)
    }

    /// Iterate all records in unspecified order.
    pub fn list(&self) -> impl Iterator<Item = &SystemActorRecord> {
        self.records.values()
    }

    fn flush(&self) -> Result<(), SystemRegistryError> {
        let file = RegistryFile {
            records: self.records.values().cloned().collect(),
        };
        let body = toml::to_string(&file).map_err(|source| SystemRegistryError::Serialize {
            path: self.registry_path.clone(),
            source,
        })?;
        let parent = self
            .registry_path
            .parent()
            .ok_or_else(|| SystemRegistryError::Io {
                path: self.registry_path.clone(),
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "registry path must have a parent directory",
                ),
            })?;
        atomic_write_file(
            &self.registry_path,
            parent,
            body.as_bytes(),
            REGISTRY_FILE_MODE,
        )
        .map_err(|source| SystemRegistryError::Io {
            path: self.registry_path.clone(),
            source,
        })
    }
}

fn read_registry(path: &Path) -> Result<HashMap<String, SystemActorRecord>, SystemRegistryError> {
    let body = read_nofollow_bounded(path, MAX_REGISTRY_FILE_BYTES).map_err(|source| {
        SystemRegistryError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let file: RegistryFile =
        toml::from_str(&body).map_err(|source| SystemRegistryError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

    let mut map = HashMap::with_capacity(file.records.len());
    for record in file.records {
        map.insert(record.name.clone(), record);
    }
    Ok(map)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record(name: &str) -> SystemActorRecord {
        SystemActorRecord {
            name: name.to_owned(),
            identity_id: IdentityId::new().unwrap(),
            inbox_dir: PathBuf::from(format!("/data/system/{name}/inbox")),
        }
    }

    fn registry_at() -> (SystemRegistry, tempfile::TempDir) {
        let dir = crate::test_support::secure_dir();
        let registry = SystemRegistry::open(dir.path().join("registry.toml")).unwrap();
        (registry, dir)
    }

    // T1: missing registry file → open succeeds with empty registry.
    #[test]
    fn open_missing_file_returns_empty_registry() {
        let (registry, _tmp) = registry_at();
        assert_eq!(registry.list().count(), 0);
    }

    // T2: register then lookup round-trips name, identity_id, and inbox_dir.
    #[test]
    fn register_then_lookup_round_trips() {
        let (mut registry, _tmp) = registry_at();
        let record = sample_record("estate");
        let id = record.identity_id;
        let inbox = record.inbox_dir.clone();
        registry.register(record).unwrap();
        let found = registry.lookup("estate").unwrap();
        assert_eq!(found.identity_id, id);
        assert_eq!(found.inbox_dir, inbox);
    }

    // T3: register upserts — second write to same name replaces.
    #[test]
    fn register_upserts_existing_record() {
        let (mut registry, _tmp) = registry_at();
        registry.register(sample_record("estate")).unwrap();
        let replacement = sample_record("estate");
        let new_id = replacement.identity_id;
        registry.register(replacement).unwrap();
        assert_eq!(registry.list().count(), 1);
        assert_eq!(registry.lookup("estate").unwrap().identity_id, new_id);
    }

    // T4: a record survives a close/reopen cycle via the on-disk file.
    #[test]
    fn record_survives_reopen() {
        let dir = crate::test_support::secure_dir();
        let path = dir.path().join("registry.toml");
        let id = {
            let mut registry = SystemRegistry::open(path.clone()).unwrap();
            let record = sample_record("estate");
            let id = record.identity_id;
            registry.register(record).unwrap();
            id
        };
        let reopened = SystemRegistry::open(path).unwrap();
        assert_eq!(reopened.lookup("estate").unwrap().identity_id, id);
    }

    // T5: an invalid name (path separator) is refused, not silently
    // written — the same filesystem-safety posture as AgentRegistry.
    #[test]
    fn register_refuses_invalid_name() {
        let (mut registry, _tmp) = registry_at();
        let mut record = sample_record("bad");
        record.name = "../escape".to_owned();
        let err = registry.register(record).unwrap_err();
        assert!(matches!(err, SystemRegistryError::InvalidName { .. }));
        assert_eq!(registry.list().count(), 0);
    }

    // T6: lookup of an unregistered name returns None, not an error — the
    // registry is in-memory-only for reads, matching AgentRegistry.
    #[test]
    fn lookup_unknown_name_returns_none() {
        let (registry, _tmp) = registry_at();
        assert!(registry.lookup("nobody").is_none());
    }
}
