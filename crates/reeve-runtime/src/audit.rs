//! Append-only audit log per `specs/reeve-walking-skeleton.ladder.md` phase 4
//! task 2 and `specs/reeve-transport-security.md` § Audit Log.
//!
//! Records security-relevant transport and identity events as JSON Lines at
//! `<data_dir>/audit/log.jsonl`. Each call to [`AuditLog::append`] serializes
//! exactly one event as a self-contained JSON object terminated by `\n`, then
//! calls `sync_data()` before returning so the record is durable before the
//! caller proceeds. This matters for forensics: a record that is only in the
//! OS write buffer is not an audit record.
//!
//! Concurrent appends from multiple threads are serialized inside the process
//! via `Mutex<File>`. Cross-process appends rely on POSIX append-mode
//! atomicity: `O_APPEND` writes of up to `PIPE_BUF` bytes (≥ 4096 on all POSIX
//! platforms) are atomic at the kernel level. Every event must serialize to
//! under 4 KiB to preserve this guarantee; a test asserts this per variant.
//!
//! Filesystem safety follows `specs/reeve-transport-security.md` §
//! Filesystem Safety: the audit directory is created with mode `0o700` via
//! `fs_util::ensure_directory`, and an existing directory with the wrong mode
//! surfaces as [`AuditError::Io`] rather than being silently fixed. The log
//! file itself is opened with `O_NOFOLLOW` on Unix (mirroring `identity_registry
//! ::read_bounded`) so a symlink placed at `audit/log.jsonl` after directory
//! creation surfaces as [`AuditError::Io`] rather than being silently followed.
//!
//! Callers MUST pass UTC `OffsetDateTime` values for the `at` fields. All
//! production call sites use `OffsetDateTime::now_utc()`, which is UTC by
//! construction. The serde annotation `#[serde(with = "time::serde::rfc3339")]`
//! preserves whatever offset is given; it does not normalize. Passing a
//! non-UTC offset will produce a conforming RFC 3339 string with a non-zero
//! offset, which is legal but inconsistent with the rest of the log.
//!
//! This module is the primitive only. Wiring audit log emissions into identity
//! enrollment and transport pipeline operations is task 12+.

use std::fmt;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use reeve_types::{IdentityId, KeyId, MessageId};
use serde::Serialize;
use time::OffsetDateTime;

use crate::fs_util::{ensure_directory, open_jsonl_file, FsCheckError};
use crate::watcher::FilenameError;

/// Mode for the audit directory on Unix. Restrictive: runtime-owned, not
/// world-readable. Matches the posture applied to the identity registry and
/// inbox directories.
const AUDIT_DIR_MODE: u32 = 0o700;

/// Mode for the audit log file on Unix.
const AUDIT_FILE_MODE: u32 = 0o600;

const AUDIT_DIR_NAME: &str = "audit";
const AUDIT_LOG_NAME: &str = "log.jsonl";

/// A security-relevant event recorded to the audit log.
///
/// Each variant maps to a `kind` discriminator string in the JSONL output:
/// `identity.enrolled`, `transport.delivered`, `transport.quarantine`,
/// `transport.replay-rejected`, `transport.filename-rejected`. The dots in
/// the kind values require explicit `#[serde(rename = ...)]` because serde's
/// `rename_all` strategies cannot produce dots.
///
/// Marked `#[non_exhaustive]` because future phases will add variants for
/// authority decisions, cost-ceiling trips, tool invocations, and similar
/// events without breaking existing match arms in downstream callers.
#[non_exhaustive]
#[derive(Debug, Serialize)]
#[serde(tag = "kind")]
pub enum AuditEvent {
    /// An identity was enrolled and written to the registry.
    #[serde(rename = "identity.enrolled")]
    IdentityEnrolled {
        identity_id: IdentityId,
        display_name: String,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },

    /// A message was verified and durably delivered to an agent's inbox.
    #[serde(rename = "transport.delivered")]
    TransportDelivered {
        sender_id: IdentityId,
        sender_key_id: KeyId,
        recipient_id: IdentityId,
        message_id: MessageId,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },

    /// A message was rejected by the verification pipeline and moved to
    /// quarantine. The `reason` is a short machine-readable token (e.g.
    /// `"signature_invalid"`, `"recipient_mismatch"`, `"clock_skew"`).
    ///
    /// `sender_id` and `sender_key_id` are `Option` because the envelope may
    /// be malformed beyond the point where a sender identity can be extracted —
    /// spec § Audit Log: "sender identity where known".
    #[serde(rename = "transport.quarantine")]
    TransportQuarantine {
        sender_id: Option<IdentityId>,
        sender_key_id: Option<KeyId>,
        recipient_id: IdentityId,
        message_id: MessageId,
        reason: String,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },

    /// A message was rejected because its `message_id` had already been seen
    /// within the replay retention window for this sender.
    #[serde(rename = "transport.replay-rejected")]
    TransportReplayRejected {
        sender_id: IdentityId,
        sender_key_id: KeyId,
        message_id: MessageId,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },

    /// File in `inbox/new/` had a malformed filename and was left in place
    /// for operator inspection. Fires INSTEAD OF `transport.quarantine`
    /// (no rename, no quarantine/ entry). Operators should alert on
    /// accumulation: the runtime cannot self-clean these files.
    ///
    /// `reason` is one of: `"not_utf8"`, `"reserved"`, `"contains_null"`,
    /// or `"too_long(<N>)"` where `<N>` is the byte length. The token format
    /// is stable and machine-parseable.
    #[serde(rename = "transport.filename-rejected")]
    TransportFilenameRejected {
        agent_id: IdentityId,
        /// Typed filename error; serializes to a stable machine-readable token.
        reason: FilenameError,
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
}

/// Errors surfaced by audit log operations.
///
/// Marked `#[non_exhaustive]` so that future phases can add variants (e.g.
/// a dedicated `DirectorySymlinked` or `FileModeWrong` variant) without
/// breaking existing callers.
///
/// `AuditError` is not `Clone` or `PartialEq`: [`io::Error`] is neither.
/// Manual `Display` and `Error::source` are used directly; no `thiserror`
/// dependency is added.
#[non_exhaustive]
#[derive(Debug)]
pub enum AuditError {
    /// Underlying filesystem error (open, write, mkdir, sync).
    Io { path: PathBuf, source: io::Error },

    /// An event could not be serialized to JSON.
    Serialize(serde_json::Error),
}

impl fmt::Display for AuditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "audit log IO at {}: {source}", path.display())
            }
            Self::Serialize(source) => write!(f, "audit log serialize: {source}"),
        }
    }
}

impl std::error::Error for AuditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Serialize(source) => Some(source),
        }
    }
}

impl AuditError {
    fn from_fs(err: FsCheckError) -> Self {
        match err {
            FsCheckError::Io { path, source } => Self::Io { path, source },
            FsCheckError::Symlink { path } => Self::Io {
                path,
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "audit directory is a symlink; runtime refuses to follow it",
                ),
            },
            FsCheckError::NotADirectory { path } => Self::Io {
                path,
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "audit path exists but is not a directory",
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
                    format!("audit directory has mode 0o{actual:o}, expected 0o{expected:o}"),
                ),
            },
        }
    }
}

/// Append-only audit log backed by a single file handle opened in `O_APPEND`
/// mode. Concurrent callers within the same process are serialized by
/// `Mutex<File>`; concurrent callers from different processes rely on POSIX
/// append-mode atomicity for writes under `PIPE_BUF` bytes (≥ 4 KiB).
///
/// `Clone` is intentionally not derived. Callers that need shared access
/// should wrap in `Arc<AuditLog>` to make sharing explicit.
#[derive(Debug)]
pub struct AuditLog {
    /// The full path to the JSONL file, kept for diagnostic messages.
    path: PathBuf,
    /// Opened in append mode for the process lifetime; the mutex serializes
    /// within-process appends and prevents interleaved JSON records from
    /// concurrent threads.
    file: Mutex<File>,
}

impl AuditLog {
    /// Open (or create) the audit log at `<data_dir>/audit/log.jsonl`.
    ///
    /// The audit directory is created with mode `0o700` on Unix if it does
    /// not already exist. An existing directory is verified to carry `0o700`;
    /// mismatches surface as [`AuditError::Io`] rather than being silently
    /// fixed. The log file is created with mode `0o600` on Unix if it does
    /// not already exist, then opened in append mode.
    pub fn open(data_dir: impl Into<PathBuf>) -> Result<Self, AuditError> {
        let audit_dir = data_dir.into().join(AUDIT_DIR_NAME);
        ensure_directory(&audit_dir, AUDIT_DIR_MODE).map_err(AuditError::from_fs)?;
        let path = audit_dir.join(AUDIT_LOG_NAME);
        let file = open_log_file(&path)?;
        Ok(Self {
            path,
            file: Mutex::new(file),
        })
    }

    /// Append one event as a JSON line to the log file.
    ///
    /// Each call:
    /// 1. Serializes `event` to a JSON byte vector (no intermediate `String`).
    /// 2. Acquires the file mutex.
    /// 3. Writes the JSON bytes then a `\n` byte.
    /// 4. Calls `sync_data()` to flush to storage before returning `Ok(())`.
    ///
    /// `sync_data()` rather than `sync_all()` is used because we only need
    /// the data to be durable, not necessarily the file metadata (atime,
    /// mtime). This is cheaper on most kernels while still making the record
    /// durable for forensic purposes.
    ///
    /// # Errors
    ///
    /// Returns [`AuditError::Serialize`] if the event cannot be serialized.
    /// Returns [`AuditError::Io`] for any write or sync failure.
    pub fn append(&self, event: &AuditEvent) -> Result<(), AuditError> {
        let mut json_bytes = serde_json::to_vec(event).map_err(AuditError::Serialize)?;
        json_bytes.push(b'\n');

        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        file.write_all(&json_bytes)
            .map_err(|source| AuditError::Io {
                path: self.path.clone(),
                source,
            })?;
        file.sync_data().map_err(|source| AuditError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

fn open_log_file(path: &Path) -> Result<File, AuditError> {
    // O_NOFOLLOW: a symlink placed at audit/log.jsonl after directory creation
    // surfaces as ELOOP rather than writing audit records to an attacker-chosen
    // target.
    open_jsonl_file(path, AUDIT_FILE_MODE).map_err(|source| AuditError::Io {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    use reeve_types::{IdentityId, KeyId, MessageId};
    use tempfile::tempdir;
    use time::OffsetDateTime;

    /// `tempfile::tempdir()` creates with a permissive mode (0o755 on macOS
    /// and Linux). The `ensure_directory` call inside `AuditLog::open`
    /// creates `audit/` with `0o700`, so the `data_dir` itself can be 0o755 —
    /// only the audit subdirectory is checked.
    fn sample_data_dir() -> tempfile::TempDir {
        tempdir().unwrap()
    }

    fn at() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn open(data_dir: &Path) -> AuditLog {
        AuditLog::open(data_dir.to_path_buf()).unwrap()
    }

    fn read_lines(data_dir: &Path) -> Vec<String> {
        let path = data_dir.join(AUDIT_DIR_NAME).join(AUDIT_LOG_NAME);
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    // A1: open creates audit/ with mode 0o700 and log.jsonl with mode 0o600.
    #[cfg(unix)]
    #[test]
    fn open_creates_audit_dir_and_file_with_correct_modes() {
        use crate::fs_util::MODE_BITS_MASK;
        use std::os::unix::fs::PermissionsExt;

        let data_dir = sample_data_dir();
        open(data_dir.path());

        let audit_dir = data_dir.path().join(AUDIT_DIR_NAME);
        let log_file = audit_dir.join(AUDIT_LOG_NAME);

        assert!(audit_dir.is_dir(), "audit/ directory missing");
        assert!(log_file.is_file(), "log.jsonl missing");

        let dir_mode = fs::metadata(&audit_dir).unwrap().permissions().mode() & MODE_BITS_MASK;
        let file_mode = fs::metadata(&log_file).unwrap().permissions().mode() & MODE_BITS_MASK;

        assert_eq!(dir_mode, AUDIT_DIR_MODE, "audit/ mode wrong");
        assert_eq!(file_mode, AUDIT_FILE_MODE, "log.jsonl mode wrong");
    }

    // A2: append writes exactly one JSONL line; parsed as JSON with correct kind.
    #[test]
    fn append_writes_one_jsonl_line() {
        let data_dir = sample_data_dir();
        let log = open(data_dir.path());

        log.append(&AuditEvent::IdentityEnrolled {
            identity_id: IdentityId::new().unwrap(),
            display_name: "Ada".to_owned(),
            at: at(),
        })
        .unwrap();

        let lines = read_lines(data_dir.path());
        assert_eq!(lines.len(), 1, "expected exactly one line");
        let value: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(value["kind"], "identity.enrolled");
        assert_eq!(value["display_name"], "Ada");
    }

    // A3: multiple appends produce multiple lines in order.
    #[test]
    fn multiple_appends_produce_multiple_lines_in_order() {
        let data_dir = sample_data_dir();
        let log = open(data_dir.path());

        let id_a = IdentityId::new().unwrap();
        let sender_id = IdentityId::new().unwrap();
        let id_b = IdentityId::new().unwrap();
        let msg = MessageId::new().unwrap();

        log.append(&AuditEvent::IdentityEnrolled {
            identity_id: id_a,
            display_name: "Ada".to_owned(),
            at: at(),
        })
        .unwrap();
        log.append(&AuditEvent::TransportDelivered {
            sender_id,
            sender_key_id: KeyId::new().unwrap(),
            recipient_id: id_b,
            message_id: msg,
            at: at(),
        })
        .unwrap();

        let lines = read_lines(data_dir.path());
        assert_eq!(lines.len(), 2, "expected two lines");

        let first: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(first["kind"], "identity.enrolled");
        assert_eq!(second["kind"], "transport.delivered");
    }

    // A4: round-trip for each variant — append then read, verify all fields.
    #[test]
    fn round_trip_all_event_variants() {
        let data_dir = sample_data_dir();
        let log = open(data_dir.path());

        let identity_id = IdentityId::new().unwrap();
        let delivered_sender_id = IdentityId::new().unwrap();
        let delivered_sender_key_id = KeyId::new().unwrap();
        let recipient_id = IdentityId::new().unwrap();
        let quarantine_sender_id = IdentityId::new().unwrap();
        let quarantine_sender_key_id = KeyId::new().unwrap();
        let replay_sender_id = IdentityId::new().unwrap();
        let replay_sender_key_id = KeyId::new().unwrap();
        let msg_a = MessageId::new().unwrap();
        let msg_b = MessageId::new().unwrap();
        let msg_c = MessageId::new().unwrap();

        log.append(&AuditEvent::IdentityEnrolled {
            identity_id,
            display_name: "Babbage".to_owned(),
            at: at(),
        })
        .unwrap();
        log.append(&AuditEvent::TransportDelivered {
            sender_id: delivered_sender_id,
            sender_key_id: delivered_sender_key_id,
            recipient_id,
            message_id: msg_a,
            at: at(),
        })
        .unwrap();
        log.append(&AuditEvent::TransportQuarantine {
            sender_id: Some(quarantine_sender_id),
            sender_key_id: Some(quarantine_sender_key_id),
            recipient_id,
            message_id: msg_b,
            reason: "signature_invalid".to_owned(),
            at: at(),
        })
        .unwrap();
        log.append(&AuditEvent::TransportReplayRejected {
            sender_id: replay_sender_id,
            sender_key_id: replay_sender_key_id,
            message_id: msg_c,
            at: at(),
        })
        .unwrap();

        let lines = read_lines(data_dir.path());
        assert_eq!(lines.len(), 4);

        let v0: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v0["kind"], "identity.enrolled");
        assert_eq!(v0["identity_id"], identity_id.to_string());
        assert_eq!(v0["display_name"], "Babbage");

        let v1: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(v1["kind"], "transport.delivered");
        assert_eq!(v1["sender_id"], delivered_sender_id.to_string());
        assert_eq!(v1["sender_key_id"], delivered_sender_key_id.to_string());
        assert_eq!(v1["recipient_id"], recipient_id.to_string());
        assert_eq!(v1["message_id"], msg_a.to_string());

        let v2: serde_json::Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(v2["kind"], "transport.quarantine");
        assert_eq!(v2["sender_id"], quarantine_sender_id.to_string());
        assert_eq!(v2["sender_key_id"], quarantine_sender_key_id.to_string());
        assert_eq!(v2["recipient_id"], recipient_id.to_string());
        assert_eq!(v2["reason"], "signature_invalid");

        let v3: serde_json::Value = serde_json::from_str(&lines[3]).unwrap();
        assert_eq!(v3["kind"], "transport.replay-rejected");
        assert_eq!(v3["sender_id"], replay_sender_id.to_string());
        assert_eq!(v3["sender_key_id"], replay_sender_key_id.to_string());
        assert_eq!(v3["message_id"], msg_c.to_string());
    }

    // A5: opening AuditLog when audit dir + file already exist succeeds.
    #[test]
    fn idempotent_open_succeeds() {
        let data_dir = sample_data_dir();

        let log = open(data_dir.path());
        log.append(&AuditEvent::IdentityEnrolled {
            identity_id: IdentityId::new().unwrap(),
            display_name: "Curie".to_owned(),
            at: at(),
        })
        .unwrap();
        drop(log);

        let log2 = open(data_dir.path());
        log2.append(&AuditEvent::IdentityEnrolled {
            identity_id: IdentityId::new().unwrap(),
            display_name: "Dirac".to_owned(),
            at: at(),
        })
        .unwrap();

        let lines = read_lines(data_dir.path());
        assert_eq!(
            lines.len(),
            2,
            "both opens should produce persistent records"
        );
    }

    // A6: concurrent appends from multiple threads produce all expected lines.
    #[test]
    fn concurrent_appends_from_multiple_threads() {
        const THREADS: usize = 4;
        const PER_THREAD: usize = 25;

        let data_dir = sample_data_dir();
        let log = Arc::new(open(data_dir.path()));

        std::thread::scope(|s| {
            for _ in 0..THREADS {
                let log = Arc::clone(&log);
                s.spawn(move || {
                    for _ in 0..PER_THREAD {
                        log.append(&AuditEvent::TransportDelivered {
                            sender_id: IdentityId::new().unwrap(),
                            sender_key_id: KeyId::new().unwrap(),
                            recipient_id: IdentityId::new().unwrap(),
                            message_id: MessageId::new().unwrap(),
                            at: OffsetDateTime::now_utc(),
                        })
                        .unwrap();
                    }
                });
            }
        });

        let lines = read_lines(data_dir.path());
        assert_eq!(
            lines.len(),
            THREADS * PER_THREAD,
            "expected {} lines total",
            THREADS * PER_THREAD,
        );
        for (i, line) in lines.iter().enumerate() {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|e| panic!("line {i} is not valid JSON: {e}\n{line}"));
        }
    }

    // A7: symlink at <data_dir>/audit/ causes an error on open.
    #[cfg(unix)]
    #[test]
    fn open_rejects_symlinked_audit_dir() {
        use std::os::unix::fs::symlink;

        let outer = tempdir().unwrap();
        let real_dir = outer.path().join("real_audit");
        fs::create_dir_all(&real_dir).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&real_dir, fs::Permissions::from_mode(0o700)).unwrap();
        }

        let data_dir = outer.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        symlink(&real_dir, data_dir.join(AUDIT_DIR_NAME)).unwrap();

        let err = AuditLog::open(data_dir).unwrap_err();
        assert!(
            matches!(err, AuditError::Io { .. }),
            "expected Io error for symlinked audit dir, got {err:?}",
        );
    }

    // A8: existing audit dir with mode 0o755 surfaces as an error.
    #[cfg(unix)]
    #[test]
    fn open_rejects_audit_dir_with_wrong_mode() {
        use std::os::unix::fs::PermissionsExt;

        let data_dir = sample_data_dir();
        let audit_dir = data_dir.path().join(AUDIT_DIR_NAME);
        fs::create_dir_all(&audit_dir).unwrap();
        fs::set_permissions(&audit_dir, fs::Permissions::from_mode(0o755)).unwrap();

        let err = AuditLog::open(data_dir.path().to_path_buf()).unwrap_err();
        assert!(
            matches!(err, AuditError::Io { .. }),
            "expected Io error for wrong-mode audit dir, got {err:?}",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("755") || msg.contains("mode"),
            "error message should mention mode or 755: {msg}",
        );
    }

    // A9: Display impls for AuditError variants are non-empty and informative.
    #[test]
    fn audit_error_display_impls() {
        let path = PathBuf::from("synthetic/test-path");

        let io_err = AuditError::Io {
            path: path.clone(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        let rendered = io_err.to_string();
        assert!(!rendered.is_empty());
        assert!(rendered.contains("synthetic/test-path"), "Io: {rendered}");

        let serde_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let ser_err = AuditError::Serialize(serde_err);
        let rendered = ser_err.to_string();
        assert!(!rendered.is_empty());
        assert!(rendered.contains("serialize"), "Serialize: {rendered}");
    }

    // A10: each event variant serializes to under 4096 bytes (PIPE_BUF safety).
    #[test]
    fn each_event_serializes_under_pipe_buf() {
        const PIPE_BUF: usize = 4096;
        let identity_id = IdentityId::new().unwrap();
        let sender_id = IdentityId::new().unwrap();
        let sender_key_id = KeyId::new().unwrap();
        let recipient_id = IdentityId::new().unwrap();
        let message_id = MessageId::new().unwrap();

        let events: &[AuditEvent] = &[
            AuditEvent::IdentityEnrolled {
                identity_id,
                display_name: "A".repeat(255),
                at: at(),
            },
            AuditEvent::TransportDelivered {
                sender_id,
                sender_key_id,
                recipient_id,
                message_id,
                at: at(),
            },
            AuditEvent::TransportQuarantine {
                sender_id: Some(sender_id),
                sender_key_id: Some(sender_key_id),
                recipient_id,
                message_id,
                reason: "signature_invalid".to_owned(),
                at: at(),
            },
            AuditEvent::TransportReplayRejected {
                sender_id,
                sender_key_id,
                message_id,
                at: at(),
            },
            AuditEvent::TransportFilenameRejected {
                agent_id: recipient_id,
                reason: FilenameError::NotUtf8,
                at: at(),
            },
        ];

        for event in events {
            let json = serde_json::to_vec(event).unwrap();
            assert!(
                json.len() < PIPE_BUF,
                "event {:?} serializes to {} bytes, exceeds PIPE_BUF {}",
                std::mem::discriminant(event),
                json.len(),
                PIPE_BUF,
            );
        }
    }

    // A11: TransportQuarantine with empty reason round-trips cleanly.
    #[test]
    fn append_handles_empty_quarantine_reason() {
        let data_dir = sample_data_dir();
        let log = open(data_dir.path());

        log.append(&AuditEvent::TransportQuarantine {
            sender_id: None,
            sender_key_id: None,
            recipient_id: IdentityId::new().unwrap(),
            message_id: MessageId::new().unwrap(),
            reason: String::new(),
            at: at(),
        })
        .unwrap();

        let lines = read_lines(data_dir.path());
        assert_eq!(lines.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["kind"], "transport.quarantine");
        assert_eq!(v["reason"], "");
    }

    // A12: IdentityEnrolled with a multi-script Unicode display_name
    // round-trips without loss or mangling.
    #[test]
    fn append_handles_unicode_display_name() {
        let data_dir = sample_data_dir();
        let log = open(data_dir.path());

        let name = "日本語 🔑 مرحبا".to_owned();
        log.append(&AuditEvent::IdentityEnrolled {
            identity_id: IdentityId::new().unwrap(),
            display_name: name.clone(),
            at: at(),
        })
        .unwrap();

        let lines = read_lines(data_dir.path());
        assert_eq!(lines.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["display_name"].as_str().unwrap(), name);
    }

    // A13: far-past and far-future timestamps round-trip through RFC 3339.
    #[test]
    fn append_handles_far_past_and_far_future_timestamps() {
        use time::format_description::well_known::Rfc3339;
        use time::macros::datetime;

        let data_dir = sample_data_dir();
        let log = open(data_dir.path());

        let far_past = datetime!(0001-01-01 00:00:00 UTC);
        let far_future = datetime!(9999-12-31 23:59:59 UTC);

        log.append(&AuditEvent::IdentityEnrolled {
            identity_id: IdentityId::new().unwrap(),
            display_name: "past".to_owned(),
            at: far_past,
        })
        .unwrap();
        log.append(&AuditEvent::IdentityEnrolled {
            identity_id: IdentityId::new().unwrap(),
            display_name: "future".to_owned(),
            at: far_future,
        })
        .unwrap();

        let lines = read_lines(data_dir.path());
        assert_eq!(lines.len(), 2);

        let v0: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        let ts0 = v0["at"].as_str().unwrap();
        let decoded0 = OffsetDateTime::parse(ts0, &Rfc3339).unwrap();
        assert_eq!(decoded0, far_past);

        let v1: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        let ts1 = v1["at"].as_str().unwrap();
        let decoded1 = OffsetDateTime::parse(ts1, &Rfc3339).unwrap();
        assert_eq!(decoded1, far_future);
    }

    // A14: a symlink at audit/log.jsonl causes AuditLog::open to fail with Io.
    #[cfg(unix)]
    #[test]
    fn open_rejects_symlinked_log_file() {
        use std::os::unix::fs::symlink;

        let outer = tempdir().unwrap();
        let data_dir = outer.path().join("data");
        let audit_dir = data_dir.join(AUDIT_DIR_NAME);
        fs::create_dir_all(&audit_dir).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&audit_dir, fs::Permissions::from_mode(0o700)).unwrap();
        }

        let real_target = outer.path().join("real_target.jsonl");
        fs::write(&real_target, b"").unwrap();
        symlink(&real_target, audit_dir.join(AUDIT_LOG_NAME)).unwrap();

        let err = AuditLog::open(data_dir).unwrap_err();
        assert!(
            matches!(err, AuditError::Io { .. }),
            "expected Io error for symlinked log file, got {err:?}",
        );
    }
}
