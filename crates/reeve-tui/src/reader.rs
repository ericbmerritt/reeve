//! Filesystem readers for the lead agent's on-disk state.
//!
//! Every function in this module is a pure read: no writes, no side effects
//! beyond the filesystem access itself. Errors are handled locally and
//! surfaced as safe default values (e.g., `AgentStatus::Unknown`, empty vec,
//! `0.0`) so the TUI always has a renderable state even during startup or
//! transient filesystem errors.
//!
//! The on-disk format is defined by `reeve-runtime::agent_fs`.

use std::path::Path;

use serde_json::Value;
use time::OffsetDateTime;

use crate::state::{AgentStatus, ConversationEntry, EntryKind};

// ── read_status ───────────────────────────────────────────────────────────────

/// Read `agents/lead/status` and parse it into an [`AgentStatus`].
///
/// Returns [`AgentStatus::Unknown`] when the file is absent, unreadable, or
/// contains an unrecognised token.
pub fn read_status(status_path: &Path) -> AgentStatus {
    let Ok(text) = std::fs::read_to_string(status_path) else {
        return AgentStatus::Unknown;
    };
    parse_status(text.trim())
}

/// Map a status token string to an [`AgentStatus`].
fn parse_status(s: &str) -> AgentStatus {
    match s.trim() {
        "idle" => AgentStatus::Idle,
        "working" => AgentStatus::Working,
        "error" | "crashed" => AgentStatus::Crashed,
        _ => AgentStatus::Unknown,
    }
}

// ── read_conversation ─────────────────────────────────────────────────────────

/// Read `agents/lead/log/conversation.jsonl` and parse it into display entries.
///
/// Each line is parsed as a JSON object with a `"type"` discriminator field:
/// - `"inbound"` → [`EntryKind::Inbound`], text from `payload`
/// - `"outbound"` → [`EntryKind::Outbound`], text from `payload`
/// - `"model_call"` → skipped (not shown in the chat view)
/// - `"system"` → [`EntryKind::System`], text from `message`
///
/// Lines that are empty, unparseable, or have unknown types are silently
/// skipped.
pub fn read_conversation(conv_path: &Path) -> Vec<ConversationEntry> {
    let Ok(text) = std::fs::read_to_string(conv_path) else {
        return Vec::new();
    };

    text.lines()
        .filter_map(|line| parse_conversation_line(line.trim()))
        .collect()
}

/// Parse one JSONL line into a [`ConversationEntry`], returning `None` to skip.
fn parse_conversation_line(line: &str) -> Option<ConversationEntry> {
    if line.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    let entry_type = value.get("type")?.as_str()?;

    match entry_type {
        "inbound" => {
            let text = value
                .get("payload")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let timestamp = parse_timestamp(&value);
            Some(ConversationEntry {
                kind: EntryKind::Inbound,
                text,
                timestamp,
            })
        }
        "outbound" => {
            let text = value
                .get("payload")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let timestamp = parse_timestamp(&value);
            Some(ConversationEntry {
                kind: EntryKind::Outbound,
                text,
                timestamp,
            })
        }
        "system" => {
            let text = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let timestamp = parse_timestamp(&value);
            Some(ConversationEntry {
                kind: EntryKind::System,
                text,
                timestamp,
            })
        }
        // "model_call" and any future variants are not shown in chat view.
        _ => None,
    }
}

/// Extract `timestamp_utc` as an [`OffsetDateTime`] from a JSON entry.
///
/// Returns `None` when the field is absent, not a string, or not a valid
/// RFC 3339 timestamp.
fn parse_timestamp(value: &Value) -> Option<OffsetDateTime> {
    let s = value.get("timestamp_utc")?.as_str()?;
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}

// ── read_cost ─────────────────────────────────────────────────────────────────

/// Read `agents/lead/cost` and parse it as a cumulative USD cost.
///
/// Returns `0.0` when the file is absent, unreadable, or not a valid float.
pub fn read_cost(cost_path: &Path) -> f64 {
    let Ok(text) = std::fs::read_to_string(cost_path) else {
        return 0.0;
    };
    text.trim().parse::<f64>().unwrap_or(0.0)
}

// ── heartbeat_fresh ───────────────────────────────────────────────────────────

/// Return `true` when the runtime heartbeat file is fresh (mtime within 2 s).
///
/// Delegates to [`reeve_runtime::heartbeat_fresh`], which reads
/// `{state_dir}/runtime/heartbeat` using `symlink_metadata`. A symlink at the
/// path is treated as absent. Returns `false` on any error.
pub fn heartbeat_fresh(state_dir: &Path) -> bool {
    reeve_runtime::heartbeat_fresh(state_dir)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    // R1: read_status returns Idle for "idle" content.
    #[test]
    fn read_status_returns_idle_for_idle_string() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("status");
        fs::write(&path, "idle").unwrap();
        assert_eq!(read_status(&path), AgentStatus::Idle);
    }

    // R2: read_status returns Working for "working" content.
    #[test]
    fn read_status_returns_working_for_working_string() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("status");
        fs::write(&path, "working").unwrap();
        assert_eq!(read_status(&path), AgentStatus::Working);
    }

    // R3: read_status returns Unknown for a missing file.
    #[test]
    fn read_status_returns_unknown_for_missing() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("status_missing");
        assert_eq!(read_status(&path), AgentStatus::Unknown);
    }

    // R4: read_status returns Crashed for "error" and "crashed" tokens.
    #[test]
    fn read_status_returns_crashed_for_error_and_crashed_tokens() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("status");
        fs::write(&path, "crashed").unwrap();
        assert_eq!(read_status(&path), AgentStatus::Crashed);
        fs::write(&path, "error").unwrap();
        assert_eq!(read_status(&path), AgentStatus::Crashed);
    }

    // R5: read_cost returns 0.0 on a missing cost file.
    #[test]
    fn read_cost_returns_zero_on_missing() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("cost_missing");
        let cost = read_cost(&path);
        // Float comparison: 0.0 == 0.0 is exact here.
        assert_eq!(cost.to_bits(), 0.0_f64.to_bits());
    }

    // R6: read_cost parses a valid float string.
    #[test]
    fn read_cost_parses_valid_float() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("cost");
        fs::write(&path, "0.0042").unwrap();
        let cost = read_cost(&path);
        assert!((cost - 0.0042_f64).abs() < 1e-9);
    }

    // R7: read_cost returns 0.0 for non-numeric content.
    #[test]
    fn read_cost_returns_zero_for_non_numeric_content() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("cost");
        fs::write(&path, "not-a-number").unwrap();
        let cost = read_cost(&path);
        assert_eq!(cost.to_bits(), 0.0_f64.to_bits());
    }

    // R8: heartbeat_fresh returns false when the heartbeat file is absent.
    #[test]
    fn heartbeat_fresh_returns_false_on_missing() {
        let tmp = tempdir().unwrap();
        // state_dir with no runtime/heartbeat subdirectory
        assert!(!heartbeat_fresh(tmp.path()));
    }

    // R9: heartbeat_fresh returns true for a freshly written heartbeat.
    #[test]
    fn heartbeat_fresh_returns_true_for_recent_file() {
        let tmp = tempdir().unwrap();
        let runtime_dir = tmp.path().join("runtime");
        fs::create_dir_all(&runtime_dir).unwrap();
        let heartbeat = runtime_dir.join("heartbeat");
        fs::write(&heartbeat, "1").unwrap();
        assert!(heartbeat_fresh(tmp.path()));
    }

    // R11: read_conversation returns empty vec on missing file.
    #[test]
    fn read_conversation_returns_empty_on_missing() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conversation.jsonl");
        let entries = read_conversation(&path);
        assert!(entries.is_empty());
    }

    // R12: read_conversation parses inbound and outbound entries.
    #[test]
    fn read_conversation_parses_inbound_and_outbound() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conversation.jsonl");
        let jsonl = concat!(
            r#"{"type":"inbound","message_id":"m1","payload":"hello","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            "\n",
            r#"{"type":"outbound","payload":"world","timestamp_utc":"2024-01-01T00:00:01Z"}"#,
            "\n",
            r#"{"type":"model_call","input_tokens":10,"output_tokens":5,"model":"x","timestamp_utc":"2024-01-01T00:00:02Z"}"#,
            "\n",
        );
        fs::write(&path, jsonl).unwrap();
        let entries = read_conversation(&path);
        // model_call is skipped, so only 2 entries expected.
        assert_eq!(entries.len(), 2, "expected 2 parsed entries");
        assert_eq!(entries[0].kind, EntryKind::Inbound);
        assert_eq!(
            entries[0].kind.speaker_label("lead"),
            "lead",
            "inbound speaker_label should return persona name"
        );
        assert_eq!(entries[0].text, "hello");
        assert_eq!(entries[1].kind, EntryKind::Outbound);
        assert_eq!(
            entries[1].kind.speaker_label("lead"),
            "you",
            "outbound speaker_label should return 'you'"
        );
        assert_eq!(entries[1].text, "world");
    }

    // R13: read_conversation parses system entries.
    #[test]
    fn read_conversation_parses_system_entries() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conversation.jsonl");
        let jsonl =
            r#"{"type":"system","message":"started","timestamp_utc":"2024-01-01T00:00:00Z"}"#;
        fs::write(&path, jsonl).unwrap();
        let entries = read_conversation(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, EntryKind::System);
        assert_eq!(
            entries[0].kind.speaker_label("lead"),
            "system",
            "system speaker_label should return 'system'"
        );
        assert_eq!(entries[0].text, "started");
    }

    // R14: read_conversation silently skips invalid JSON lines.
    #[test]
    fn read_conversation_skips_invalid_lines() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conversation.jsonl");
        let jsonl = concat!(
            "not valid json\n",
            r#"{"type":"system","message":"ok","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            "\n",
        );
        fs::write(&path, jsonl).unwrap();
        let entries = read_conversation(&path);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].text, "ok");
    }
}
