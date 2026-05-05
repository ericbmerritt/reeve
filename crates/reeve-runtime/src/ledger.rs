//! Replay ledger and delivery ledger per `specs/reeve-walking-skeleton.md`
//! phase 4 and `specs/reeve-transport-security.md` § Replay Ledger and
//! Delivery Ledger.
//!
//! Two distinct durable JSONL files with distinct schemas. Conflating them
//! is a bug (spec § Replay Ledger and Delivery Ledger: "Conflating these two
//! leads to bugs in which a verified-but-not-delivered message is rejected
//! after a crash").
//!
//! **Replay ledger** — keyed on `(sender_id, message_id, nonce)`. Prevents
//! external replay within the retention window. Every message the runtime
//! accepts or rejects updates this ledger so that the same identifiers cannot
//! be retried. Written to `<data_dir>/replay-ledger.jsonl`.
//!
//! **Delivery ledger** — keyed on `(recipient_id, message_id)`. Ensures
//! idempotent delivery across crash recovery: a message that was verified and
//! durably inserted into agent context is recorded here so that a crash-and-
//! restart that re-picks the message from `new/` does not double-deliver it.
//! Written to `<data_dir>/delivery-ledger.jsonl`.
//!
//! Both ledgers share the same implementation strategy:
//!
//! - Append-only JSONL for the on-disk artifact (`O_APPEND`, `O_NOFOLLOW`,
//!   mode `0o600`), matching the posture from `audit.rs`.
//! - An in-memory index loaded at `open()` time for O(1) `contains` checks
//!   within the process lifetime.
//! - `prune_older_than` rewrites the file atomically via `tempfile` rename,
//!   dropping records whose `at` field precedes the cutoff. The in-memory
//!   index is rebuilt from the surviving set.
//! - Cross-process append atomicity: on regular files, `O_APPEND` writes are
//!   not atomicity-guaranteed by POSIX (only pipes carry the hard `PIPE_BUF`
//!   guarantee). In practice, Linux and macOS provide atomic `O_APPEND` for
//!   single-block writes on local filesystems. The 4 KiB bound keeps each
//!   record within one filesystem block as a pragmatic guard; tests assert
//!   this. Within-process concurrent access is serialized by `Mutex`.
//!
//! Schema distinctness: `ReplayRecord` requires `sender_id` and `nonce` while
//! `DeliveryRecord` requires `recipient_id` — neither schema is a subset of
//! the other. Both use `#[serde(deny_unknown_fields)]` so cross-type
//! deserialization fails fast rather than silently ignoring fields.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use reeve_types::{IdentityId, MessageId, Nonce};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use time::OffsetDateTime;

use crate::fs_util::{ensure_directory, open_jsonl_file, FsCheckError};

/// Mode for the ledger data directory on Unix. Matches the posture used by
/// the audit, inbox, and identity registry modules: runtime-owned, not
/// world-readable.
const LEDGER_DIR_MODE: u32 = 0o700;
const LEDGER_FILE_MODE: u32 = 0o600;
const REPLAY_LEDGER_NAME: &str = "replay-ledger.jsonl";
const DELIVERY_LEDGER_NAME: &str = "delivery-ledger.jsonl";

/// Errors surfaced by ledger operations.
///
/// Marked `#[non_exhaustive]` so that future phases can add variants without
/// breaking existing callers. Manual `Display` and `Error::source` are used
/// directly; no `thiserror` dependency.
#[non_exhaustive]
#[derive(Debug)]
pub enum LedgerError {
    /// Underlying filesystem error (open, write, mkdir, sync, rename).
    Io { path: PathBuf, source: io::Error },

    /// A record could not be serialized to JSON.
    Serialize(serde_json::Error),

    /// A record line could not be deserialized from JSON during `open` or
    /// `prune_older_than`. The line is included for diagnostic context.
    Deserialize {
        line: String,
        source: serde_json::Error,
    },
}

impl fmt::Display for LedgerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "ledger IO at {}: {source}", path.display())
            }
            Self::Serialize(source) => write!(f, "ledger serialize: {source}"),
            Self::Deserialize { line, source } => {
                write!(f, "ledger deserialize: {source}; line: {line}")
            }
        }
    }
}

impl std::error::Error for LedgerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Serialize(source) | Self::Deserialize { source, .. } => Some(source),
        }
    }
}

fn open_ledger_file(path: &Path) -> Result<File, LedgerError> {
    open_jsonl_file(path, LEDGER_FILE_MODE).map_err(|source| LedgerError::Io {
        path: path.to_path_buf(),
        source,
    })
}

impl LedgerError {
    fn from_fs(err: FsCheckError) -> Self {
        match err {
            FsCheckError::Io { path, source } => Self::Io { path, source },
            FsCheckError::Symlink { path } => Self::Io {
                path,
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ledger data directory is a symlink; runtime refuses to follow it",
                ),
            },
            FsCheckError::NotADirectory { path } => Self::Io {
                path,
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ledger data directory path is not a directory",
                ),
            },
            FsCheckError::WrongMode {
                path,
                actual,
                expected,
            } => Self::Io {
                path,
                source: io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("ledger data directory has mode 0o{actual:o}, expected 0o{expected:o}"),
                ),
            },
        }
    }
}

/// Composite key for the replay ledger. Identifies a unique `(sender, message,
/// nonce)` tuple as defined in the spec § Replay Ledger.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReplayKey {
    pub sender_id: IdentityId,
    pub message_id: MessageId,
    pub nonce: Nonce,
}

/// On-disk record shape for the replay ledger.
///
/// The `nonce` is serialized as unpadded standard base64, matching the wire
/// encoding used throughout the codebase. `#[serde(deny_unknown_fields)]`
/// ensures cross-type deserialization fails fast — a `DeliveryRecord` line
/// cannot accidentally satisfy this deserializer because it carries
/// `recipient_id` (an unknown field here) and lacks `sender_id` and `nonce`
/// (required fields here).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayRecord {
    pub sender_id: IdentityId,
    pub message_id: MessageId,
    pub nonce: Nonce,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

impl ReplayRecord {
    fn key(&self) -> ReplayKey {
        ReplayKey {
            sender_id: self.sender_id,
            message_id: self.message_id,
            nonce: self.nonce,
        }
    }
}

/// Replay ledger backed by `<data_dir>/replay-ledger.jsonl`.
///
/// Prevents external replay by recording every `(sender_id, message_id,
/// nonce)` tuple the verification pipeline has seen — whether the message was
/// accepted or rejected. An in-memory `HashSet` built at `open()` time gives
/// O(1) `contains` checks within the process lifetime.
///
/// `Clone` is intentionally not derived. Callers that need shared access
/// should wrap in `Arc<ReplayLedger>`.
#[derive(Debug)]
pub struct ReplayLedger {
    path: PathBuf,
    file: Mutex<File>,
    /// In-memory index built from the on-disk records at `open()` time and
    /// kept current on every `observe` call. Also held under `Mutex` so that
    /// `observe` and `prune_older_than` are mutually exclusive.
    index: Mutex<HashSet<ReplayKey>>,
}

impl ReplayLedger {
    /// Open (or create) the replay ledger at `<data_dir>/replay-ledger.jsonl`.
    ///
    /// An existing file is read in full to populate the in-memory index; any
    /// malformed line surfaces as [`LedgerError::Deserialize`]. The file is
    /// then opened in append mode with `O_NOFOLLOW` so a symlink placed at the
    /// path after startup surfaces as [`LedgerError::Io`].
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self, LedgerError> {
        let data_dir = data_dir.into();
        ensure_directory(&data_dir, LEDGER_DIR_MODE).map_err(LedgerError::from_fs)?;
        let path = data_dir.join(REPLAY_LEDGER_NAME);
        let index = load_replay_index(&path)?;
        let file = open_ledger_file(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
            index: Mutex::new(index),
        })
    }

    /// Record a `(sender_id, message_id, nonce)` tuple as observed at `at`.
    ///
    /// Returns `Ok(true)` if the key was newly recorded. Returns `Ok(false)`
    /// if the key was already present in the index; in that case the file is
    /// not written again (the in-memory set is already the authoritative view
    /// within this process).
    pub fn observe(&self, key: ReplayKey, at: OffsetDateTime) -> Result<bool, LedgerError> {
        let mut index = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if index.contains(&key) {
            return Ok(false);
        }

        let record = ReplayRecord {
            sender_id: key.sender_id,
            message_id: key.message_id,
            nonce: key.nonce,
            at,
        };
        append_record(&self.path, &self.file, &record)?;
        index.insert(key);
        Ok(true)
    }

    /// Check whether a key is already in the ledger without recording it.
    ///
    /// The `Result` is currently infallible — the in-memory index lookup
    /// cannot fail. The `Result` return type is preserved for API symmetry
    /// with `observe` and to allow future phases to add fallible paths
    /// (e.g. a read-through from disk) without breaking callers.
    pub fn contains(&self, key: &ReplayKey) -> Result<bool, LedgerError> {
        let index = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(index.contains(key))
    }

    /// Remove records whose `at` timestamp is strictly before `cutoff`.
    ///
    /// Reads all records from disk, filters by cutoff, atomically replaces
    /// the file via tmp-then-rename in the same directory, and rebuilds the
    /// in-memory index from the surviving set. Returns the number of records
    /// pruned.
    ///
    /// Lock order: index first, then file — matching `observe`. Holding the
    /// index lock for the full duration prevents concurrent `observe` calls
    /// from interleaving between the disk rewrite and the index update.
    pub fn prune_older_than(&self, cutoff: OffsetDateTime) -> Result<usize, LedgerError> {
        let mut index_guard = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file_guard = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let records: Vec<ReplayRecord> = read_records(&self.path)?;
        let before = records.len();
        let surviving: Vec<ReplayRecord> = records.into_iter().filter(|r| r.at >= cutoff).collect();
        let pruned = before - surviving.len();

        rewrite_file(&self.path, &surviving, &mut file_guard)?;

        let new_index: HashSet<ReplayKey> = surviving.iter().map(ReplayRecord::key).collect();
        *index_guard = new_index;

        Ok(pruned)
    }
}

fn load_replay_index(path: &Path) -> Result<HashSet<ReplayKey>, LedgerError> {
    let records: Vec<ReplayRecord> = read_records(path)?;
    Ok(records.into_iter().map(|r| r.key()).collect())
}

/// Atomically replace the ledger file with `records` via tmp-then-rename.
///
/// Durability sequence: flush userspace buffer → `sync_data` makes the tmp
/// file's data durable → `persist` (`rename(2)`) atomically installs it →
/// parent-dir fsync makes the directory entry update durable. This matches
/// the per-record path in `append_record`, which also calls `sync_data` on
/// every write. We then re-open the replaced file because the old
/// mutex-held handle points at the unlinked tmp inode, not the new one.
fn rewrite_file<T: Serialize>(
    path: &Path,
    records: &[T],
    file_guard: &mut File,
) -> Result<(), LedgerError> {
    let parent = path.parent().ok_or_else(|| LedgerError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(io::ErrorKind::InvalidInput, "ledger path has no parent dir"),
    })?;
    let mut tmp = NamedTempFile::new_in(parent).map_err(|source| LedgerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    for record in records {
        let mut bytes = serde_json::to_vec(record).map_err(LedgerError::Serialize)?;
        bytes.push(b'\n');
        tmp.write_all(&bytes).map_err(|source| LedgerError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    }
    tmp.flush().map_err(|source| LedgerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    tmp.as_file()
        .sync_data()
        .map_err(|source| LedgerError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    tmp.persist(path).map_err(|e| LedgerError::Io {
        path: path.to_path_buf(),
        source: e.error,
    })?;

    // Defense-in-depth: fsync the parent directory so the rename(2) that
    // updated the directory entry is durable before we return Ok.
    let dir = File::open(parent).map_err(|source| LedgerError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    dir.sync_all().map_err(|source| LedgerError::Io {
        path: parent.to_path_buf(),
        source,
    })?;

    // Re-open the replaced file so the mutex-held handle stays valid.
    *file_guard = open_ledger_file(path)?;
    Ok(())
}

/// Composite key for the delivery ledger. Identifies a unique `(recipient,
/// message)` pair as defined in the spec § Delivery Ledger.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeliveryKey {
    pub recipient_id: IdentityId,
    pub message_id: MessageId,
}

/// On-disk record shape for the delivery ledger.
///
/// `#[serde(deny_unknown_fields)]` ensures cross-type deserialization fails
/// fast — a `ReplayRecord` line cannot satisfy this deserializer because it
/// carries `sender_id` and `nonce` (unknown fields here) and lacks
/// `recipient_id` (a required field here).
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryRecord {
    pub recipient_id: IdentityId,
    pub message_id: MessageId,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

impl DeliveryRecord {
    fn key(&self) -> DeliveryKey {
        DeliveryKey {
            recipient_id: self.recipient_id,
            message_id: self.message_id,
        }
    }
}

/// Delivery ledger backed by `<data_dir>/delivery-ledger.jsonl`.
///
/// Ensures idempotent delivery across crash recovery by recording every
/// `(recipient_id, message_id)` pair that has been durably inserted into
/// agent context. An in-memory `HashMap` built at `open()` time gives O(1)
/// `contains` checks within the process lifetime.
///
/// `Clone` is intentionally not derived. Callers that need shared access
/// should wrap in `Arc<DeliveryLedger>`.
#[derive(Debug)]
pub struct DeliveryLedger {
    path: PathBuf,
    file: Mutex<File>,
    /// In-memory index: maps each key to the `at` timestamp of first delivery.
    /// Held under `Mutex` so that `record` and `prune_older_than` are
    /// mutually exclusive.
    index: Mutex<HashMap<DeliveryKey, OffsetDateTime>>,
}

impl DeliveryLedger {
    /// Open (or create) the delivery ledger at
    /// `<data_dir>/delivery-ledger.jsonl`.
    ///
    /// An existing file is read in full to populate the in-memory index; any
    /// malformed line surfaces as [`LedgerError::Deserialize`]. The file is
    /// then opened in append mode with `O_NOFOLLOW`.
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self, LedgerError> {
        let data_dir = data_dir.into();
        ensure_directory(&data_dir, LEDGER_DIR_MODE).map_err(LedgerError::from_fs)?;
        let path = data_dir.join(DELIVERY_LEDGER_NAME);
        let index = load_delivery_index(&path)?;
        let file = open_ledger_file(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
            index: Mutex::new(index),
        })
    }

    /// Record a `(recipient_id, message_id)` pair as delivered at `at`.
    ///
    /// Returns `Ok(true)` if the key was newly recorded. Returns `Ok(false)`
    /// if the key was already present (idempotent: the in-memory index is
    /// authoritative within the process lifetime).
    pub fn record(&self, key: DeliveryKey, at: OffsetDateTime) -> Result<bool, LedgerError> {
        let mut index = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        if index.contains_key(&key) {
            return Ok(false);
        }

        let record = DeliveryRecord {
            recipient_id: key.recipient_id,
            message_id: key.message_id,
            at,
        };
        append_record(&self.path, &self.file, &record)?;
        index.insert(key, at);
        Ok(true)
    }

    /// Check whether a key is already in the ledger without recording it.
    ///
    /// The `Result` is currently infallible — the in-memory index lookup
    /// cannot fail. The `Result` return type is preserved for API symmetry
    /// with `record` and to allow future phases to add fallible paths without
    /// breaking callers.
    pub fn contains(&self, key: &DeliveryKey) -> Result<bool, LedgerError> {
        let index = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(index.contains_key(key))
    }

    /// Remove records whose `at` timestamp is strictly before `cutoff`.
    ///
    /// Reads all records from disk, filters by cutoff, atomically replaces
    /// the file, and rebuilds the in-memory index from the surviving set.
    /// Returns the number of records pruned.
    ///
    /// Lock order: index first, then file — matching `record`. Holding the
    /// index lock for the full duration prevents concurrent `record` calls
    /// from interleaving between the disk rewrite and the index update.
    pub fn prune_older_than(&self, cutoff: OffsetDateTime) -> Result<usize, LedgerError> {
        let mut index_guard = self
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut file_guard = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let records: Vec<DeliveryRecord> = read_records(&self.path)?;
        let before = records.len();
        let surviving: Vec<DeliveryRecord> =
            records.into_iter().filter(|r| r.at >= cutoff).collect();
        let pruned = before - surviving.len();

        rewrite_file(&self.path, &surviving, &mut file_guard)?;

        let new_index: HashMap<DeliveryKey, OffsetDateTime> =
            surviving.iter().map(|r| (r.key(), r.at)).collect();
        *index_guard = new_index;

        Ok(pruned)
    }
}

fn load_delivery_index(path: &Path) -> Result<HashMap<DeliveryKey, OffsetDateTime>, LedgerError> {
    let records: Vec<DeliveryRecord> = read_records(path)?;
    Ok(records.into_iter().map(|r| (r.key(), r.at)).collect())
}

fn read_records<T>(path: &Path) -> Result<Vec<T>, LedgerError>
where
    T: for<'de> Deserialize<'de>,
{
    let content = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(LedgerError::Io {
                path: path.to_path_buf(),
                source,
            })
        }
    };
    content
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|source| LedgerError::Deserialize {
                line: line.to_owned(),
                source,
            })
        })
        .collect()
}

fn append_record<T: Serialize>(
    path: &Path,
    file_mutex: &Mutex<File>,
    record: &T,
) -> Result<(), LedgerError> {
    let mut bytes = serde_json::to_vec(record).map_err(LedgerError::Serialize)?;
    bytes.push(b'\n');

    let mut file = file_mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    file.write_all(&bytes).map_err(|source| LedgerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    file.sync_data().map_err(|source| LedgerError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::sync::Arc;

    use reeve_types::{IdentityId, MessageId, Nonce, NONCE_LEN};
    use tempfile::tempdir;
    use time::OffsetDateTime;

    fn at() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn sample_nonce() -> Nonce {
        Nonce::from_bytes([0xABu8; NONCE_LEN])
    }

    fn sample_replay_key() -> ReplayKey {
        ReplayKey {
            sender_id: IdentityId::new().unwrap(),
            message_id: MessageId::new().unwrap(),
            nonce: sample_nonce(),
        }
    }

    fn sample_delivery_key() -> DeliveryKey {
        DeliveryKey {
            recipient_id: IdentityId::new().unwrap(),
            message_id: MessageId::new().unwrap(),
        }
    }

    fn replay_path(data_dir: &Path) -> PathBuf {
        data_dir.join(REPLAY_LEDGER_NAME)
    }

    fn delivery_path(data_dir: &Path) -> PathBuf {
        data_dir.join(DELIVERY_LEDGER_NAME)
    }

    /// `tempfile::tempdir()` creates with a permissive mode (0o755 on macOS
    /// and Linux). `open` now enforces `LEDGER_DIR_MODE` (0o700); tighten
    /// first so tests don't trip the mode check.
    #[cfg(unix)]
    fn chmod_secure(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(LEDGER_DIR_MODE)).unwrap();
    }

    #[cfg(not(unix))]
    fn chmod_secure(_path: &Path) {}

    // RL1: open creates replay-ledger.jsonl with mode 0o600.
    #[cfg(unix)]
    #[test]
    fn replay_open_creates_file_with_correct_mode() {
        use crate::fs_util::MODE_BITS_MASK;
        use std::os::unix::fs::PermissionsExt;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let _ledger = ReplayLedger::open(data_dir.path().to_path_buf()).unwrap();

        let path = replay_path(data_dir.path());
        assert!(path.is_file(), "replay-ledger.jsonl missing");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & MODE_BITS_MASK;
        assert_eq!(mode, LEDGER_FILE_MODE, "replay-ledger.jsonl mode wrong");
    }

    // RL2: observe new key returns Ok(true); same key again returns Ok(false).
    #[test]
    fn replay_observe_new_key_returns_true_duplicate_returns_false() {
        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = ReplayLedger::open(data_dir.path().to_path_buf()).unwrap();
        let key = sample_replay_key();

        assert!(ledger.observe(key.clone(), at()).unwrap());
        assert!(!ledger.observe(key, at()).unwrap());
    }

    // RL3: contains returns true after observe.
    #[test]
    fn replay_contains_returns_true_after_observe() {
        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = ReplayLedger::open(data_dir.path().to_path_buf()).unwrap();
        let key = sample_replay_key();

        assert!(!ledger.contains(&key).unwrap());
        ledger.observe(key.clone(), at()).unwrap();
        assert!(ledger.contains(&key).unwrap());
    }

    // RL4: records persist across open() calls.
    #[test]
    fn replay_records_persist_across_open() {
        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let key = sample_replay_key();

        {
            let ledger = ReplayLedger::open(data_dir.path().to_path_buf()).unwrap();
            ledger.observe(key.clone(), at()).unwrap();
        }

        let ledger2 = ReplayLedger::open(data_dir.path().to_path_buf()).unwrap();
        assert!(
            ledger2.contains(&key).unwrap(),
            "key should be present after reopening"
        );
    }

    // RL5: prune removes only old records; newer records remain.
    #[test]
    fn replay_prune_removes_old_leaves_new() {
        use time::Duration;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = ReplayLedger::open(data_dir.path().to_path_buf()).unwrap();

        let old_key = sample_replay_key();
        let new_key = sample_replay_key();
        let cutoff = at();
        let old_at = cutoff - Duration::seconds(10);
        let new_at = cutoff + Duration::seconds(10);

        ledger.observe(old_key.clone(), old_at).unwrap();
        ledger.observe(new_key.clone(), new_at).unwrap();

        let pruned = ledger.prune_older_than(cutoff).unwrap();
        assert_eq!(pruned, 1, "expected 1 old record pruned");
        assert!(!ledger.contains(&old_key).unwrap());
        assert!(ledger.contains(&new_key).unwrap());

        let ledger2 = ReplayLedger::open(data_dir.path().to_path_buf()).unwrap();
        assert!(!ledger2.contains(&old_key).unwrap());
        assert!(ledger2.contains(&new_key).unwrap());
    }

    // RL6: concurrent observe of distinct keys all succeed.
    #[test]
    fn replay_concurrent_observe_distinct_keys() {
        const THREADS: usize = 4;
        const PER_THREAD: usize = 25;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = Arc::new(ReplayLedger::open(data_dir.path().to_path_buf()).unwrap());

        std::thread::scope(|s| {
            for _ in 0..THREADS {
                let ledger = Arc::clone(&ledger);
                s.spawn(move || {
                    for _ in 0..PER_THREAD {
                        let key = ReplayKey {
                            sender_id: IdentityId::new().unwrap(),
                            message_id: MessageId::new().unwrap(),
                            nonce: Nonce::from_bytes([0xBBu8; NONCE_LEN]),
                        };
                        assert!(ledger.observe(key, OffsetDateTime::now_utc()).unwrap());
                    }
                });
            }
        });

        let path = replay_path(data_dir.path());
        let lines: Vec<_> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        assert_eq!(lines.len(), THREADS * PER_THREAD);
        for (i, line) in lines.iter().enumerate() {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("line {i} is not valid JSON: {e}\n{line}"));
        }
    }

    // RL7: symlink at replay-ledger.jsonl causes open to fail.
    #[cfg(unix)]
    #[test]
    fn replay_open_rejects_symlinked_file() {
        use std::os::unix::fs::symlink;
        use std::os::unix::fs::PermissionsExt;

        let outer = tempdir().unwrap();
        let data_dir = outer.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(LEDGER_DIR_MODE)).unwrap();

        let real_target = outer.path().join("real.jsonl");
        fs::write(&real_target, b"").unwrap();
        symlink(&real_target, data_dir.join(REPLAY_LEDGER_NAME)).unwrap();

        let err = ReplayLedger::open(data_dir).unwrap_err();
        assert!(
            matches!(err, LedgerError::Io { .. }),
            "expected Io error for symlinked replay file, got {err:?}",
        );
    }

    // RL8: ReplayRecord serializes under 4096 bytes (O_APPEND write safety on
    // regular files; named after the POSIX minimum for pipes, which is the
    // canonical reference for this bound).
    #[test]
    fn replay_record_serializes_under_pipe_buf() {
        const PIPE_BUF: usize = 4096;
        let record = ReplayRecord {
            sender_id: IdentityId::new().unwrap(),
            message_id: MessageId::new().unwrap(),
            nonce: sample_nonce(),
            at: at(),
        };
        let bytes = serde_json::to_vec(&record).unwrap();
        assert!(
            bytes.len() < PIPE_BUF,
            "ReplayRecord serializes to {} bytes, exceeds PIPE_BUF {}",
            bytes.len(),
            PIPE_BUF,
        );
    }

    // RL9: open rejects a symlinked data_dir.
    #[cfg(unix)]
    #[test]
    fn replay_open_rejects_symlinked_data_dir() {
        use std::os::unix::fs::symlink;

        let outer = tempdir().unwrap();
        let real_dir = outer.path().join("real");
        fs::create_dir_all(&real_dir).unwrap();
        let link = outer.path().join("data");
        symlink(&real_dir, &link).unwrap();

        let err = ReplayLedger::open(link).unwrap_err();
        assert!(
            matches!(err, LedgerError::Io { .. }),
            "expected Io error for symlinked data_dir, got {err:?}",
        );
    }

    // RL10: open rejects a data_dir with wrong mode.
    #[cfg(unix)]
    #[test]
    fn replay_open_rejects_data_dir_with_wrong_mode() {
        use std::os::unix::fs::PermissionsExt;

        let data_dir = tempdir().unwrap();
        fs::set_permissions(data_dir.path(), fs::Permissions::from_mode(0o755)).unwrap();

        let err = ReplayLedger::open(data_dir.path().to_path_buf()).unwrap_err();
        assert!(
            matches!(err, LedgerError::Io { .. }),
            "expected Io error for wrong-mode data_dir, got {err:?}",
        );
    }

    // DL1: open creates delivery-ledger.jsonl with mode 0o600.
    #[cfg(unix)]
    #[test]
    fn delivery_open_creates_file_with_correct_mode() {
        use crate::fs_util::MODE_BITS_MASK;
        use std::os::unix::fs::PermissionsExt;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let _ledger = DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap();

        let path = delivery_path(data_dir.path());
        assert!(path.is_file(), "delivery-ledger.jsonl missing");
        let mode = fs::metadata(&path).unwrap().permissions().mode() & MODE_BITS_MASK;
        assert_eq!(mode, LEDGER_FILE_MODE, "delivery-ledger.jsonl mode wrong");
    }

    // DL2: record new key returns Ok(true); same key again returns Ok(false).
    #[test]
    fn delivery_record_new_key_returns_true_duplicate_returns_false() {
        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap();
        let key = sample_delivery_key();

        assert!(ledger.record(key.clone(), at()).unwrap());
        assert!(!ledger.record(key, at()).unwrap());
    }

    // DL3: contains returns true after record.
    #[test]
    fn delivery_contains_returns_true_after_record() {
        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap();
        let key = sample_delivery_key();

        assert!(!ledger.contains(&key).unwrap());
        ledger.record(key.clone(), at()).unwrap();
        assert!(ledger.contains(&key).unwrap());
    }

    // DL4: records persist across open() calls.
    #[test]
    fn delivery_records_persist_across_open() {
        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let key = sample_delivery_key();

        {
            let ledger = DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap();
            ledger.record(key.clone(), at()).unwrap();
        }

        let ledger2 = DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap();
        assert!(
            ledger2.contains(&key).unwrap(),
            "key should be present after reopening"
        );
    }

    // DL5: prune removes only old records; newer records remain.
    #[test]
    fn delivery_prune_removes_old_leaves_new() {
        use time::Duration;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap();

        let old_key = sample_delivery_key();
        let new_key = sample_delivery_key();
        let cutoff = at();
        let old_at = cutoff - Duration::seconds(10);
        let new_at = cutoff + Duration::seconds(10);

        ledger.record(old_key.clone(), old_at).unwrap();
        ledger.record(new_key.clone(), new_at).unwrap();

        let pruned = ledger.prune_older_than(cutoff).unwrap();
        assert_eq!(pruned, 1, "expected 1 old record pruned");
        assert!(!ledger.contains(&old_key).unwrap());
        assert!(ledger.contains(&new_key).unwrap());

        let ledger2 = DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap();
        assert!(!ledger2.contains(&old_key).unwrap());
        assert!(ledger2.contains(&new_key).unwrap());
    }

    // DL6: concurrent record of distinct keys all succeed.
    #[test]
    fn delivery_concurrent_record_distinct_keys() {
        const THREADS: usize = 4;
        const PER_THREAD: usize = 25;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = Arc::new(DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap());

        std::thread::scope(|s| {
            for _ in 0..THREADS {
                let ledger = Arc::clone(&ledger);
                s.spawn(move || {
                    for _ in 0..PER_THREAD {
                        let key = DeliveryKey {
                            recipient_id: IdentityId::new().unwrap(),
                            message_id: MessageId::new().unwrap(),
                        };
                        assert!(ledger.record(key, OffsetDateTime::now_utc()).unwrap());
                    }
                });
            }
        });

        let path = delivery_path(data_dir.path());
        let lines: Vec<_> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        assert_eq!(lines.len(), THREADS * PER_THREAD);
        for (i, line) in lines.iter().enumerate() {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("line {i} is not valid JSON: {e}\n{line}"));
        }
    }

    // DL7: symlink at delivery-ledger.jsonl causes open to fail.
    #[cfg(unix)]
    #[test]
    fn delivery_open_rejects_symlinked_file() {
        use std::os::unix::fs::symlink;
        use std::os::unix::fs::PermissionsExt;

        let outer = tempdir().unwrap();
        let data_dir = outer.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(LEDGER_DIR_MODE)).unwrap();

        let real_target = outer.path().join("real.jsonl");
        fs::write(&real_target, b"").unwrap();
        symlink(&real_target, data_dir.join(DELIVERY_LEDGER_NAME)).unwrap();

        let err = DeliveryLedger::open(data_dir).unwrap_err();
        assert!(
            matches!(err, LedgerError::Io { .. }),
            "expected Io error for symlinked delivery file, got {err:?}",
        );
    }

    // DL8: DeliveryRecord serializes under 4096 bytes (O_APPEND write safety
    // on regular files; named after the POSIX minimum for pipes).
    #[test]
    fn delivery_record_serializes_under_pipe_buf() {
        const PIPE_BUF: usize = 4096;
        let record = DeliveryRecord {
            recipient_id: IdentityId::new().unwrap(),
            message_id: MessageId::new().unwrap(),
            at: at(),
        };
        let bytes = serde_json::to_vec(&record).unwrap();
        assert!(
            bytes.len() < PIPE_BUF,
            "DeliveryRecord serializes to {} bytes, exceeds PIPE_BUF {}",
            bytes.len(),
            PIPE_BUF,
        );
    }

    // DL9: open rejects a symlinked data_dir.
    #[cfg(unix)]
    #[test]
    fn delivery_open_rejects_symlinked_data_dir() {
        use std::os::unix::fs::symlink;

        let outer = tempdir().unwrap();
        let real_dir = outer.path().join("real");
        fs::create_dir_all(&real_dir).unwrap();
        let link = outer.path().join("data");
        symlink(&real_dir, &link).unwrap();

        let err = DeliveryLedger::open(link).unwrap_err();
        assert!(
            matches!(err, LedgerError::Io { .. }),
            "expected Io error for symlinked data_dir, got {err:?}",
        );
    }

    // DL10: open rejects a data_dir with wrong mode.
    #[cfg(unix)]
    #[test]
    fn delivery_open_rejects_data_dir_with_wrong_mode() {
        use std::os::unix::fs::PermissionsExt;

        let data_dir = tempdir().unwrap();
        fs::set_permissions(data_dir.path(), fs::Permissions::from_mode(0o755)).unwrap();

        let err = DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap_err();
        assert!(
            matches!(err, LedgerError::Io { .. }),
            "expected Io error for wrong-mode data_dir, got {err:?}",
        );
    }

    // IT1: Schema distinctness integration test.
    //
    // A replay-ledger line must not be deserializable as a DeliveryRecord, and
    // a delivery-ledger line must not be deserializable as a ReplayRecord.
    // This is the Phase 4 done-when assertion: "distinct on-disk artifacts
    // with distinct schemas (asserted by integration test)".
    #[test]
    fn schemas_are_distinct_cross_deserialize_fails() {
        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());

        let replay_ledger = ReplayLedger::open(data_dir.path().to_path_buf()).unwrap();
        let delivery_ledger = DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap();

        let replay_key = sample_replay_key();
        let delivery_key = sample_delivery_key();
        replay_ledger.observe(replay_key, at()).unwrap();
        delivery_ledger.record(delivery_key, at()).unwrap();

        let replay_line = fs::read_to_string(replay_path(data_dir.path()))
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned();
        let delivery_line = fs::read_to_string(delivery_path(data_dir.path()))
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned();

        // A replay line must not parse as DeliveryRecord.
        let replay_as_delivery: Result<DeliveryRecord, _> = serde_json::from_str(&replay_line);
        assert!(
            replay_as_delivery.is_err(),
            "replay line must not deserialize as DeliveryRecord; line: {replay_line}"
        );

        // A delivery line must not parse as ReplayRecord.
        let delivery_as_replay: Result<ReplayRecord, _> = serde_json::from_str(&delivery_line);
        assert!(
            delivery_as_replay.is_err(),
            "delivery line must not deserialize as ReplayRecord; line: {delivery_line}"
        );
    }

    // IT2: LedgerError Display impls are non-empty and informative.
    #[test]
    fn ledger_error_display_impls() {
        let path = PathBuf::from("synthetic/test-path");

        let io_err = LedgerError::Io {
            path: path.clone(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        let rendered = io_err.to_string();
        assert!(!rendered.is_empty());
        assert!(rendered.contains("synthetic/test-path"), "Io: {rendered}");

        let serde_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let ser_err = LedgerError::Serialize(serde_err);
        let rendered = ser_err.to_string();
        assert!(!rendered.is_empty());
        assert!(rendered.contains("serialize"), "Serialize: {rendered}");

        let serde_err2 = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let de_err = LedgerError::Deserialize {
            line: "bad line".to_owned(),
            source: serde_err2,
        };
        let rendered = de_err.to_string();
        assert!(!rendered.is_empty());
        assert!(
            rendered.contains("bad line") || rendered.contains("deserialize"),
            "Deserialize: {rendered}"
        );
    }

    // C1: replay ledger — same key, 2 threads barrier-released simultaneously;
    // exactly one returns Ok(true).
    #[test]
    fn replay_same_key_concurrent_race_exactly_one_wins() {
        use std::sync::Barrier;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = Arc::new(ReplayLedger::open(data_dir.path().to_path_buf()).unwrap());
        let key = sample_replay_key();
        let barrier = Arc::new(Barrier::new(2));

        let results = std::thread::scope(|s| {
            let mut handles = Vec::new();
            for _ in 0..2 {
                let ledger = Arc::clone(&ledger);
                let key = key.clone();
                let barrier = Arc::clone(&barrier);
                handles.push(s.spawn(move || {
                    barrier.wait();
                    ledger.observe(key, OffsetDateTime::now_utc()).unwrap()
                }));
            }
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });

        let wins = results.iter().filter(|&&r| r).count();
        assert_eq!(
            wins, 1,
            "exactly one thread should win the race, got {wins}"
        );
    }

    // C2: delivery ledger — same key, 2 threads barrier-released simultaneously;
    // exactly one returns Ok(true).
    #[test]
    fn delivery_same_key_concurrent_race_exactly_one_wins() {
        use std::sync::Barrier;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = Arc::new(DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap());
        let key = sample_delivery_key();
        let barrier = Arc::new(Barrier::new(2));

        let results = std::thread::scope(|s| {
            let mut handles = Vec::new();
            for _ in 0..2 {
                let ledger = Arc::clone(&ledger);
                let key = key.clone();
                let barrier = Arc::clone(&barrier);
                handles.push(s.spawn(move || {
                    barrier.wait();
                    ledger.record(key, OffsetDateTime::now_utc()).unwrap()
                }));
            }
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });

        let wins = results.iter().filter(|&&r| r).count();
        assert_eq!(
            wins, 1,
            "exactly one thread should win the race, got {wins}"
        );
    }

    // P1: prune on empty replay ledger returns Ok(0).
    #[test]
    fn replay_prune_empty_returns_zero() {
        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = ReplayLedger::open(data_dir.path().to_path_buf()).unwrap();
        let pruned = ledger.prune_older_than(at()).unwrap();
        assert_eq!(pruned, 0);
    }

    // P2: prune on empty delivery ledger returns Ok(0).
    #[test]
    fn delivery_prune_empty_returns_zero() {
        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap();
        let pruned = ledger.prune_older_than(at()).unwrap();
        assert_eq!(pruned, 0);
    }

    // P3: prune cutoff after all records — replay ledger empties completely.
    #[test]
    fn replay_prune_after_all_records_empties_ledger() {
        use time::Duration;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = ReplayLedger::open(data_dir.path().to_path_buf()).unwrap();

        let now = at();
        let old_at = now - Duration::seconds(100);

        let k1 = sample_replay_key();
        let k2 = sample_replay_key();
        ledger.observe(k1.clone(), old_at).unwrap();
        ledger.observe(k2.clone(), old_at).unwrap();

        let pruned = ledger.prune_older_than(now).unwrap();
        assert_eq!(pruned, 2, "both records should be pruned");
        assert!(!ledger.contains(&k1).unwrap());
        assert!(!ledger.contains(&k2).unwrap());

        // File should contain no non-empty lines.
        let path = replay_path(data_dir.path());
        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.lines().filter(|l| !l.is_empty()).count() == 0,
            "file should be empty after pruning all records",
        );
    }

    // P4: prune cutoff after all records — delivery ledger empties completely.
    #[test]
    fn delivery_prune_after_all_records_empties_ledger() {
        use time::Duration;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap();

        let now = at();
        let old_at = now - Duration::seconds(100);

        let k1 = sample_delivery_key();
        let k2 = sample_delivery_key();
        ledger.record(k1.clone(), old_at).unwrap();
        ledger.record(k2.clone(), old_at).unwrap();

        let pruned = ledger.prune_older_than(now).unwrap();
        assert_eq!(pruned, 2, "both records should be pruned");
        assert!(!ledger.contains(&k1).unwrap());
        assert!(!ledger.contains(&k2).unwrap());

        let path = delivery_path(data_dir.path());
        let content = fs::read_to_string(&path).unwrap();
        assert!(
            content.lines().filter(|l| !l.is_empty()).count() == 0,
            "file should be empty after pruning all records",
        );
    }

    // P5: prune cutoff before all records — replay ledger preserves everything.
    #[test]
    fn replay_prune_before_all_records_preserves_all() {
        use time::Duration;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = ReplayLedger::open(data_dir.path().to_path_buf()).unwrap();

        let now = at();
        let future_at = now + Duration::seconds(100);

        let k1 = sample_replay_key();
        let k2 = sample_replay_key();
        ledger.observe(k1.clone(), future_at).unwrap();
        ledger.observe(k2.clone(), future_at).unwrap();

        let pruned = ledger.prune_older_than(now).unwrap();
        assert_eq!(pruned, 0, "no records should be pruned");
        assert!(ledger.contains(&k1).unwrap());
        assert!(ledger.contains(&k2).unwrap());
    }

    // P6: prune cutoff before all records — delivery ledger preserves everything.
    #[test]
    fn delivery_prune_before_all_records_preserves_all() {
        use time::Duration;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap();

        let now = at();
        let future_at = now + Duration::seconds(100);

        let k1 = sample_delivery_key();
        let k2 = sample_delivery_key();
        ledger.record(k1.clone(), future_at).unwrap();
        ledger.record(k2.clone(), future_at).unwrap();

        let pruned = ledger.prune_older_than(now).unwrap();
        assert_eq!(pruned, 0, "no records should be pruned");
        assert!(ledger.contains(&k1).unwrap());
        assert!(ledger.contains(&k2).unwrap());
    }

    // T1: far-past and far-future timestamps round-trip and prune correctly —
    // replay ledger.
    #[test]
    fn replay_far_past_and_far_future_timestamps() {
        use time::macros::datetime;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = ReplayLedger::open(data_dir.path().to_path_buf()).unwrap();

        let far_past = datetime!(0001-01-01 00:00:00 UTC);
        let far_future = datetime!(9999-12-31 23:59:59 UTC);
        let mid = datetime!(2026-01-01 00:00:00 UTC);

        let old_key = sample_replay_key();
        let new_key = sample_replay_key();
        ledger.observe(old_key.clone(), far_past).unwrap();
        ledger.observe(new_key.clone(), far_future).unwrap();

        let pruned = ledger.prune_older_than(mid).unwrap();
        assert_eq!(pruned, 1, "far-past record should be pruned");
        assert!(!ledger.contains(&old_key).unwrap());
        assert!(ledger.contains(&new_key).unwrap());
    }

    // T2: far-past and far-future timestamps round-trip and prune correctly —
    // delivery ledger.
    #[test]
    fn delivery_far_past_and_far_future_timestamps() {
        use time::macros::datetime;

        let data_dir = tempdir().unwrap();
        chmod_secure(data_dir.path());
        let ledger = DeliveryLedger::open(data_dir.path().to_path_buf()).unwrap();

        let far_past = datetime!(0001-01-01 00:00:00 UTC);
        let far_future = datetime!(9999-12-31 23:59:59 UTC);
        let mid = datetime!(2026-01-01 00:00:00 UTC);

        let old_key = sample_delivery_key();
        let new_key = sample_delivery_key();
        ledger.record(old_key.clone(), far_past).unwrap();
        ledger.record(new_key.clone(), far_future).unwrap();

        let pruned = ledger.prune_older_than(mid).unwrap();
        assert_eq!(pruned, 1, "far-past record should be pruned");
        assert!(!ledger.contains(&old_key).unwrap());
        assert!(ledger.contains(&new_key).unwrap());
    }
}
