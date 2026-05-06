//! Envelope signing and atomic submission for the Reeve TUI.
//!
//! [`submit_message`] writes a payload to the lead agent's maildir inbox via a
//! tmp → rename atomic sequence, following the Maildir convention:
//!
//! 1. Write to `inbox/tmp/<uuid>.json`
//! 2. `rename` to `inbox/new/<uuid>.json`
//!
//! This guarantees the inbox watcher never observes a partial file.
//!
//! # Walking-skeleton simplification
//!
//! The full spec calls for a signed [`reeve_types::Envelope`] addressed from the
//! operator identity to the lead agent identity. Signing requires `sha2` (not yet
//! a dependency of `reeve-tui`) and a non-trivial lead-agent identity look-up
//! path that does not yet exist. For the Phase 8 walking skeleton, we write a
//! plain JSON object `{"type":"inbound","payload":"...","timestamp_utc":"..."}` —
//! enough for the TUI exercise to land a file in `inbox/new/` and for the
//! filesystem watcher to fire. The runtime's verification pipeline will reject it,
//! which is expected at this stage. A follow-on ladder step will replace this
//! with a proper signed envelope once the lead-agent identity is wired up.

use std::fmt;
use std::fs;
use std::io::Write as _;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use tempfile::NamedTempFile;
use time::OffsetDateTime;

use reeve_runtime::AgentDirs;
use reeve_types::MessageId;

// ── Error type ─────────────────────────────────────────────────────────────────

/// Errors produced by [`submit_message`].
#[derive(Debug)]
pub enum SubmitError {
    /// A filesystem operation (open, write, rename) failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// A fresh `UUIDv7` message id could not be minted (clock skew or
    /// monotonicity violation).
    MintMessageId(reeve_types::MessageIdError),
    /// JSON serialisation of the envelope payload failed.
    Serialize(serde_json::Error),
    /// Timestamp formatting failed (should be unreachable for UTC timestamps).
    FormatTimestamp(time::error::Format),
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "submit IO at {}: {source}", path.display())
            }
            Self::MintMessageId(err) => write!(f, "submit failed to mint message id: {err}"),
            Self::Serialize(err) => write!(f, "submit JSON serialization failed: {err}"),
            Self::FormatTimestamp(err) => {
                write!(f, "submit timestamp formatting failed: {err}")
            }
        }
    }
}

impl std::error::Error for SubmitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::MintMessageId(err) => Some(err),
            Self::Serialize(err) => Some(err),
            Self::FormatTimestamp(err) => Some(err),
        }
    }
}

// ── submit_message ─────────────────────────────────────────────────────────────

/// Write `payload` to the lead agent's inbox using an atomic tmp → rename.
///
/// Creates a `NamedTempFile` in `inbox/tmp/`, writes and syncs the content,
/// sets permissions to 0o600, then persists it to `inbox/new/<id>.json`.
/// If any step fails the temp file is cleaned up automatically on drop.
///
/// # Errors
///
/// Returns [`SubmitError::MintMessageId`] when a fresh `UUIDv7` cannot be minted,
/// [`SubmitError::Serialize`] when JSON serialisation fails, or
/// [`SubmitError::Io`] on any filesystem error.
pub fn submit_message(payload: &str, dirs: &AgentDirs) -> Result<(), SubmitError> {
    let message_id = MessageId::new().map_err(SubmitError::MintMessageId)?;

    let envelope = build_skeleton_envelope(payload, message_id)?;

    let filename = format!("{message_id}.json");
    let new_path = dirs.inbox_root().join("new").join(&filename);
    let tmp_dir = dirs.inbox_root().join("tmp");

    let mut tmp = NamedTempFile::new_in(&tmp_dir).map_err(|source| SubmitError::Io {
        path: tmp_dir.clone(),
        source,
    })?;

    tmp.write_all(envelope.as_bytes())
        .map_err(|source| SubmitError::Io {
            path: tmp_dir.clone(),
            source,
        })?;

    tmp.as_file()
        .sync_data()
        .map_err(|source| SubmitError::Io {
            path: tmp_dir.clone(),
            source,
        })?;

    #[cfg(unix)]
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|source| SubmitError::Io {
            path: tmp_dir.clone(),
            source,
        })?;

    tmp.persist(&new_path).map_err(|e| SubmitError::Io {
        path: new_path,
        source: e.error,
    })?;

    Ok(())
}

/// Serialise a minimal skeleton envelope as a JSON string.
///
/// The caller supplies the `message_id` so that the envelope body and the
/// inbox filename share the same UUID (they were previously independent mints,
/// making the filename unverifiable from the envelope contents).
///
/// The payload is a plain JSON object:
/// `{"type":"inbound","message_id":"<id>","payload":"...","timestamp_utc":"..."}`
pub(crate) fn build_skeleton_envelope(
    payload: &str,
    message_id: MessageId,
) -> Result<String, SubmitError> {
    let timestamp = OffsetDateTime::now_utc();
    let ts_str = timestamp
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(SubmitError::FormatTimestamp)?;

    let value = serde_json::json!({
        "type": "inbound",
        "message_id": message_id.to_string(),
        "payload": payload,
        "timestamp_utc": ts_str,
    });

    serde_json::to_string(&value).map_err(SubmitError::Serialize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_skeleton_envelope_produces_valid_json() {
        let message_id = MessageId::new().unwrap();
        let payload = "hello world";
        let json = build_skeleton_envelope(payload, message_id).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "inbound");
        assert_eq!(parsed["payload"], payload);
        assert_eq!(
            parsed["message_id"].as_str().unwrap(),
            message_id.to_string().as_str()
        );
        assert!(parsed["timestamp_utc"].as_str().is_some());
    }

    #[test]
    fn build_skeleton_envelope_escapes_special_chars() {
        let message_id = MessageId::new().unwrap();
        let payload = r#"say "hello" \ world"#;
        let json = build_skeleton_envelope(payload, message_id).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["payload"], payload);
    }

    #[test]
    fn submit_message_filename_matches_envelope_message_id() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = AgentDirs::provision(tmp.path(), "lead").unwrap();
        submit_message("hello", &dirs).unwrap();
        let new_dir = dirs.inbox_root().join("new");
        let entries: Vec<_> = fs::read_dir(&new_dir).unwrap().collect();
        assert_eq!(entries.len(), 1);
        let entry = entries[0].as_ref().unwrap();
        let filename = entry.file_name();
        let stem = filename.to_string_lossy();
        let stem = stem.trim_end_matches(".json");
        let content = fs::read_to_string(entry.path()).unwrap();
        let value: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["message_id"].as_str().unwrap(), stem);
    }
}
