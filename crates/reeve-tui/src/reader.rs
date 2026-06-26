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

use crate::state::{AgentStatus, AuthorityDecision, ConversationEntry, Disposition, EntryKind};

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
        "exiting" => AgentStatus::Exiting,
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

/// Read at most the last `tail_bytes` of `conv_path` and parse the
/// complete JSONL lines found there.
///
/// Cost-bounded variant of [`read_conversation`] for callers that only
/// need a small tail (the panopticon shows ~16 events per agent). With N
/// agents this caps per-tick IO at `N × tail_bytes` regardless of how
/// chatty an individual conversation gets, so the snapshot reader does
/// not regress to O(total-history) IO as the runtime grows.
///
/// If the file is smaller than `tail_bytes`, reads the whole file. If the
/// requested tail straddles a line boundary, the first (likely partial)
/// line is discarded — only complete lines after the first newline are
/// parsed.
///
/// File-open or read errors return an empty `Vec` (consistent with
/// [`read_conversation`]); the screen stays renderable during transient
/// filesystem hiccups.
#[must_use]
pub fn read_conversation_tail(conv_path: &Path, tail_bytes: u64) -> Vec<ConversationEntry> {
    let Some(text) = read_tail_text(conv_path, tail_bytes) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| parse_conversation_line(line.trim()))
        .collect()
}

/// Read at most the last `tail_bytes` of `path` and return the text of the
/// complete lines it contains.
///
/// When the window starts mid-file (a non-zero seek), the first — likely
/// partial — line is dropped so callers only ever parse whole lines. When the
/// file fits entirely in `tail_bytes`, every line is returned.
///
/// Returns `None` on any file-open/read error, and when the window contains no
/// complete line — i.e. after dropping the partial first line nothing remains
/// (no newline follows it, as when a single line is longer than the window).
/// Both collapse to the empty-result contract every reader in this module
/// honours, so the screen stays renderable during startup races and transient
/// filesystem hiccups.
fn read_tail_text(path: &Path, tail_bytes: u64) -> Option<String> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let offset = len.saturating_sub(tail_bytes);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::with_capacity(usize::try_from(tail_bytes.min(len)).unwrap_or(0));
    file.read_to_end(&mut bytes).ok()?;

    let slice: &[u8] = if offset == 0 {
        &bytes
    } else {
        let newline = bytes.iter().position(|b| *b == b'\n')?;
        &bytes[newline + 1..]
    };
    Some(String::from_utf8_lossy(slice).into_owned())
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
            let sender_id = parse_sender_id(&value);
            Some(ConversationEntry {
                kind: EntryKind::Inbound,
                text,
                timestamp,
                sender_id,
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
                sender_id: None,
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
                sender_id: None,
            })
        }
        // "model_call" and any future variants are not shown in chat view.
        _ => None,
    }
}

/// Extract `sender_id` from an inbound JSON entry. Returns `None` for legacy
/// entries (written before sender attribution was wired through) and for
/// malformed UUIDs (the renderer falls back to an "unknown" tag).
///
/// Deserializes through `IdentityId`'s own `serde::Deserialize` impl so the
/// TUI does not need a direct dependency on the `uuid` crate and so the
/// `UUIDv7` invariant `IdentityId` enforces (rejecting non-v7 UUIDs) is
/// shared between writer and reader.
fn parse_sender_id(value: &Value) -> Option<reeve_types::IdentityId> {
    let raw = value.get("sender_id")?;
    if raw.is_null() {
        return None;
    }
    serde_json::from_value(raw.clone()).ok()
}

/// Extract `timestamp_utc` as an [`OffsetDateTime`] from a conversation entry.
fn parse_timestamp(value: &Value) -> Option<OffsetDateTime> {
    parse_rfc3339_field(value, "timestamp_utc")
}

/// Extract an RFC 3339 timestamp from `field` of a JSON object.
///
/// Returns `None` when the field is absent, not a string, or not a valid
/// RFC 3339 timestamp. Shared by the conversation reader (`timestamp_utc`)
/// and the audit reader (`at`), which name the same concept differently.
fn parse_rfc3339_field(value: &Value, field: &str) -> Option<OffsetDateTime> {
    let s = value.get(field)?.as_str()?;
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339).ok()
}

// ── read_authority_decisions ───────────────────────────────────────────────────

/// Read at most the last `tail_bytes` of the audit log and parse the
/// `authority.decision` entries found there, in file (chronological) order.
///
/// The audit log interleaves every event kind (`identity.*`, `transport.*`,
/// `authority.decision`, …); this filters to authority decisions only. A
/// 64 KiB tail holds on the order of 500 decisions — the same order of
/// magnitude as [`read_conversation_tail`]'s window — so the Decisions tab
/// and the panopticon's pending panel stay O(1) per tick regardless of how
/// long the log grows.
///
/// File-open or read errors return an empty `Vec`, matching the safe-default
/// contract of every other reader in this module.
#[must_use]
pub fn read_authority_decisions_tail(audit_path: &Path, tail_bytes: u64) -> Vec<AuthorityDecision> {
    let Some(text) = read_tail_text(audit_path, tail_bytes) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| parse_authority_decision_line(line.trim()))
        .collect()
}

/// Parse one JSONL line into an [`AuthorityDecision`], returning `None` to
/// skip lines that are empty, unparseable, a non-`authority.decision` event,
/// or missing a required field (`agent_id`, `disposition`).
fn parse_authority_decision_line(line: &str) -> Option<AuthorityDecision> {
    if line.is_empty() {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("kind")?.as_str()? != "authority.decision" {
        return None;
    }
    // Deserialize through `IdentityId`'s own impl so the UUIDv7 invariant is
    // shared with the writer; a malformed id drops the row rather than
    // surfacing a bogus decision.
    let agent_id: reeve_types::IdentityId =
        serde_json::from_value(value.get("agent_id")?.clone()).ok()?;
    let disposition = match value.get("disposition")?.as_str()? {
        "allow" => Disposition::Allow,
        "refuse" => Disposition::Refuse,
        _ => return None,
    };
    let persona_name = value
        .get("persona_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let action = value
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let layer = value
        .get("layer")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let rationale = value
        .get("rationale")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some(AuthorityDecision {
        timestamp: parse_rfc3339_field(&value, "at"),
        agent_id,
        persona_name,
        action,
        disposition,
        layer,
        rationale,
    })
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
        // Legacy inbound entries (no sender_id field) render as "unknown".
        assert_eq!(
            entries[0].speaker_label("lead", None),
            "unknown",
            "inbound speaker_label without sender_id should be 'unknown'"
        );
        assert_eq!(entries[0].text, "hello");
        assert_eq!(entries[1].kind, EntryKind::Outbound);
        assert_eq!(
            entries[1].speaker_label("lead", None),
            "lead",
            "outbound speaker_label should return the persona name"
        );
        assert_eq!(entries[1].text, "world");
    }

    // R12b: speaker_label distinguishes operator-signed inbound from
    // peer-agent-signed inbound. Regression guard for the bug where every
    // inbound rendered as "you" regardless of who sent it.
    #[test]
    fn inbound_speaker_label_distinguishes_operator_from_peer() {
        let operator_id = reeve_types::IdentityId::new().unwrap();
        let worker_id = reeve_types::IdentityId::new().unwrap();

        let from_operator = ConversationEntry {
            kind: EntryKind::Inbound,
            text: String::from("hi from the operator"),
            timestamp: None,
            sender_id: Some(operator_id),
        };
        let from_worker = ConversationEntry {
            kind: EntryKind::Inbound,
            text: String::from("hi from the worker"),
            timestamp: None,
            sender_id: Some(worker_id),
        };

        assert_eq!(
            from_operator.speaker_label("lead", Some(operator_id)),
            "you"
        );
        let worker_label = from_worker.speaker_label("lead", Some(operator_id));
        assert_ne!(
            worker_label, "you",
            "peer-signed inbound must not say 'you'"
        );
        assert!(
            worker_id.to_string().starts_with(&worker_label),
            "worker label should be the leading UUID segment; got: {worker_label}"
        );
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
            entries[0].speaker_label("lead", None),
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

    // R15: read_conversation_tail returns the last K complete lines from a
    // long file; the first (partial) line inside the tail window is
    // discarded. This is the contract the panopticon's per-tick IO budget
    // depends on.
    #[test]
    fn read_conversation_tail_returns_only_lines_after_first_newline() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conversation.jsonl");
        // Twenty lines, each ~80 bytes. Total ~1.6 KB.
        let lines: Vec<String> = (0..20)
            .map(|i| {
                format!(
                    r#"{{"type":"system","message":"line {i:02}","timestamp_utc":"2024-01-01T00:00:00Z"}}"#,
                )
            })
            .collect();
        let mut body = lines.join("\n");
        body.push('\n');
        fs::write(&path, &body).unwrap();
        // Tail window much smaller than the file forces the partial-first-line
        // drop path.
        let entries = read_conversation_tail(&path, 200);
        assert!(!entries.is_empty(), "tail must return at least one line");
        assert!(
            entries.len() < 20,
            "tail must skip lines that fell outside the window; got {}",
            entries.len()
        );
        // The last entry should always be the last full line in the file.
        assert_eq!(entries.last().unwrap().text, "line 19");
        // First entry must NOT be `line 00` (that line is outside the tail
        // window); the partial line at the window's start was discarded.
        assert_ne!(entries.first().unwrap().text, "line 00");
    }

    // R16: when the file fits in the tail window, every line is returned.
    // The partial-first-line discard only fires when the seek was non-zero.
    #[test]
    fn read_conversation_tail_returns_everything_when_file_fits() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("conversation.jsonl");
        let body = concat!(
            r#"{"type":"system","message":"first","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            "\n",
            r#"{"type":"system","message":"second","timestamp_utc":"2024-01-01T00:00:00Z"}"#,
            "\n",
        );
        fs::write(&path, body).unwrap();
        let entries = read_conversation_tail(&path, 8192);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "first");
        assert_eq!(entries[1].text, "second");
    }

    // R17: missing file returns empty, matching the safe-default contract
    // every other reader in this crate honours.
    #[test]
    fn read_conversation_tail_missing_file_returns_empty() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("does_not_exist.jsonl");
        let entries = read_conversation_tail(&path, 8192);
        assert!(entries.is_empty());
    }

    // R18: the audit reader extracts only `authority.decision` entries from a
    // log that interleaves other kinds, and maps allow/refuse plus the
    // refuse-only `layer`/`rationale` fields.
    #[test]
    fn read_authority_decisions_extracts_only_authority_entries() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("log.jsonl");
        let agent = reeve_types::IdentityId::new().unwrap();
        let jsonl = format!(
            concat!(
                r#"{{"kind":"transport.delivered","at":"2024-01-01T00:00:00Z"}}"#,
                "\n",
                r#"{{"kind":"authority.decision","agent_id":"{id}","persona_name":"worker","profile_version":1,"action":"SpawnAgent(persona=worker)","disposition":"refuse","layer":"profile","rationale":"spawn_agents disabled","blacklist_version":null,"at":"2024-01-01T00:00:01Z"}}"#,
                "\n",
                r#"{{"kind":"authority.decision","agent_id":"{id}","persona_name":"worker","profile_version":1,"action":"SendMessage(to=lead)","disposition":"allow","blacklist_version":null,"at":"2024-01-01T00:00:02Z"}}"#,
                "\n",
            ),
            id = agent,
        );
        fs::write(&path, jsonl).unwrap();

        let decisions = read_authority_decisions_tail(&path, 64 * 1024);
        assert_eq!(decisions.len(), 2, "only authority.decision entries kept");

        assert_eq!(decisions[0].disposition, Disposition::Refuse);
        assert_eq!(decisions[0].agent_id, agent);
        assert_eq!(decisions[0].action, "SpawnAgent(persona=worker)");
        assert_eq!(decisions[0].layer.as_deref(), Some("profile"));
        assert_eq!(
            decisions[0].rationale.as_deref(),
            Some("spawn_agents disabled")
        );

        assert_eq!(decisions[1].disposition, Disposition::Allow);
        assert!(decisions[1].layer.is_none(), "allow carries no layer");
        assert!(
            decisions[1].rationale.is_none(),
            "allow carries no rationale"
        );
    }

    // R19: a missing audit log returns empty (safe-default contract).
    #[test]
    fn read_authority_decisions_missing_file_returns_empty() {
        let tmp = tempdir().unwrap();
        let decisions = read_authority_decisions_tail(&tmp.path().join("nope.jsonl"), 8192);
        assert!(decisions.is_empty());
    }

    // R20: non-JSON lines and entries with an unparseable `agent_id` are
    // dropped without poisoning the well-formed entries around them.
    #[test]
    fn read_authority_decisions_skips_malformed_lines() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("log.jsonl");
        let agent = reeve_types::IdentityId::new().unwrap();
        let jsonl = format!(
            concat!(
                "not json\n",
                r#"{{"kind":"authority.decision","agent_id":"not-a-uuid","disposition":"refuse","at":"2024-01-01T00:00:01Z"}}"#,
                "\n",
                r#"{{"kind":"authority.decision","agent_id":"{id}","persona_name":"w","profile_version":1,"action":"X","disposition":"refuse","layer":"blacklist","rationale":"r","blacklist_version":null,"at":"2024-01-01T00:00:02Z"}}"#,
                "\n",
            ),
            id = agent,
        );
        fs::write(&path, jsonl).unwrap();

        let decisions = read_authority_decisions_tail(&path, 64 * 1024);
        assert_eq!(decisions.len(), 1, "malformed id and non-json dropped");
        assert_eq!(decisions[0].agent_id, agent);
    }
}
