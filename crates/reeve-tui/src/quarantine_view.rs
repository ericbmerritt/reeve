//! Quarantine review view-model and snapshot reader.
//!
//! The quarantine review screen lets the operator triage envelopes that
//! failed verification at the watcher boundary. This module:
//!
//! - Defines the typed view-model the renderer reads ([`QuarantineSnapshot`]
//!   and [`QuarantineEntry`]).
//! - Exposes a pure builder ([`build_snapshot`]) that maps a list of raw
//!   per-file inputs into the renderer-ready snapshot. Pure so sorting and
//!   normalization can be tested without filesystem fixtures.
//! - Exposes an IO orchestrator ([`read_snapshot`]) that walks every
//!   agent's `inbox/quarantine/` directory under the data root, reads
//!   each file, and hands the result to [`build_snapshot`].
//!
//! Filename convention is set by `reeve-runtime::watcher`: quarantined
//! files are renamed to `<original_stem>.<reason_token>`. The stem holds
//! the original (UUIDv7-derived) inbox filename; the suffix is one of the
//! tokens emitted by [`reeve_runtime::QuarantineReason`]'s `Display` impl
//! (e.g. `parse_failure`, `signature_invalid`, `clock_skew`,
//! `recipient_not_found`).
//!
//! Errors are absorbed into safe defaults: an unparseable file shows as
//! [`EnvelopeMeta::ParseFailure`] with the raw filename so the operator
//! can still see *something* on the screen and choose to discard it. A
//! quarantine directory that doesn't exist (agent freshly registered, no
//! rejected mail yet) is skipped silently.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use time::OffsetDateTime;

use reeve_types::{Envelope, IdentityId, MessageId};

/// Maximum number of quarantine entries surfaced in a snapshot. The
/// review screen renders one row per entry and a body preview; reading
/// thousands of files on every refresh would dwarf every other reader
/// in this crate. If the operator's queue genuinely needs more, the
/// fix is housekeeping (discard old entries), not lifting the cap.
pub const MAX_ENTRIES: usize = 256;

// ── View-model types ──────────────────────────────────────────────────────────

/// One quarantined envelope. The renderer pulls the list column data
/// (arrived, recipient, sender label, reason) from the top fields and
/// the detail-pane fields from `meta` and `raw_body`.
#[derive(Debug, Clone, PartialEq)]
pub struct QuarantineEntry {
    /// Absolute path to the file inside `inbox/quarantine/`. The discard
    /// (`d`) keystroke deletes this exact path; nothing in the renderer
    /// or the keymap reconstructs it from other fields.
    pub path: PathBuf,
    /// Role name of the agent whose `quarantine/` directory the file is
    /// in. Always the *intended* recipient at watcher-time, even for
    /// `RecipientMismatch` quarantines.
    pub recipient: String,
    /// File-system mtime parsed as the "arrived" timestamp. The watcher
    /// doesn't write a separate received-at field; mtime is the closest
    /// signal the screen has.
    pub arrived: Option<OffsetDateTime>,
    /// Reason token parsed from the filename suffix
    /// (`<stem>.<reason_token>`). Stored as a String rather than an
    /// enum because new reason tokens may appear in older quarantine
    /// directories the screen has no schema knowledge of.
    pub reason: String,
    /// Envelope metadata when the file parsed successfully, or the
    /// `ParseFailure` variant otherwise.
    pub meta: EnvelopeMeta,
    /// Raw envelope body interpreted as UTF-8 if possible; lossy
    /// conversion otherwise. The display footnote tells the operator
    /// when a lossy conversion was used.
    pub raw_body: String,
    /// True when `raw_body` came from a lossy UTF-8 conversion — the
    /// detail pane warns the operator the bytes don't decode cleanly.
    pub body_lossy: bool,
}

/// Envelope detail-pane content. Either a fully-parsed envelope's
/// metadata or a parse-failure marker carrying just the on-disk
/// filename so the operator can still identify which file to discard.
#[derive(Debug, Clone, PartialEq)]
pub enum EnvelopeMeta {
    Parsed {
        message_id: MessageId,
        sender_id: IdentityId,
        recipient_id: IdentityId,
        created_at: OffsetDateTime,
    },
    ParseFailure {
        /// Filename for the operator's reference. Path is on the
        /// outer `QuarantineEntry`; storing the bare name here keeps
        /// the detail pane self-contained.
        filename: String,
    },
}

/// The full read-side snapshot for the quarantine review screen.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct QuarantineSnapshot {
    pub entries: Vec<QuarantineEntry>,
    /// True when [`MAX_ENTRIES`] capped the listing. Renderer surfaces a
    /// "+N more" hint so the operator knows the view isn't authoritative.
    pub truncated: bool,
}

// ── Raw input for the pure builder ────────────────────────────────────────────

/// Pre-read filesystem state for a single quarantine file. The pure
/// builder consumes a list of these; the IO orchestrator produces them
/// from the on-disk tree.
#[derive(Debug, Clone, PartialEq)]
pub struct QuarantineFile {
    pub path: PathBuf,
    pub recipient: String,
    pub mtime: Option<SystemTime>,
    /// Filename component after the last `.`; the watcher writes this
    /// as the reason token. Empty when no dot is present (shouldn't
    /// happen for files the watcher wrote, but defensive).
    pub reason: String,
    /// Raw file bytes. Parsed by the builder, not by the orchestrator,
    /// so parse-failure behaviour stays test-friendly.
    pub body_bytes: Vec<u8>,
}

// ── Pure builder ──────────────────────────────────────────────────────────────

/// Map raw per-file inputs into a renderer-ready snapshot.
///
/// Sort order is `arrived` descending (newest first), with files
/// missing an mtime pushed to the bottom. This matches the wireframe's
/// "most recent at top" reading. The list is then capped at
/// [`MAX_ENTRIES`]; `truncated` records whether the cap kicked in.
#[must_use]
pub fn build_snapshot(files: Vec<QuarantineFile>) -> QuarantineSnapshot {
    let original_len = files.len();
    let mut entries: Vec<QuarantineEntry> = files.into_iter().map(parse_one).collect();
    // Sort newest first; `None` mtimes sink to the bottom.
    entries.sort_by(|a, b| match (a.arrived, b.arrived) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    let truncated = original_len > MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    QuarantineSnapshot { entries, truncated }
}

/// Parse a single raw file into a [`QuarantineEntry`]. Always returns
/// an entry — parse failures surface as [`EnvelopeMeta::ParseFailure`]
/// rather than dropping the row, so the operator can still see and
/// discard the file from the review screen.
fn parse_one(file: QuarantineFile) -> QuarantineEntry {
    let arrived = file.mtime.map(OffsetDateTime::from);
    let filename = file
        .path
        .file_name()
        .map(|os| os.to_string_lossy().into_owned())
        .unwrap_or_default();

    let (meta, raw_body, body_lossy) = match serde_json::from_slice::<Envelope>(&file.body_bytes) {
        Ok(env) => {
            let (raw_body, lossy) = decode_body(&env.body);
            (
                EnvelopeMeta::Parsed {
                    message_id: env.message_id,
                    sender_id: env.sender_id,
                    recipient_id: env.recipient_id,
                    created_at: env.created_at,
                },
                raw_body,
                lossy,
            )
        }
        Err(_) => (
            EnvelopeMeta::ParseFailure {
                filename: filename.clone(),
            },
            String::new(),
            false,
        ),
    };

    QuarantineEntry {
        path: file.path,
        recipient: file.recipient,
        arrived,
        reason: file.reason,
        meta,
        raw_body,
        body_lossy,
    }
}

/// UTF-8 decode the envelope body. Returns `(text, lossy)` where
/// `lossy` is `true` when the bytes didn't decode cleanly and the
/// renderer should surface a `[non-UTF-8 body]` footnote.
fn decode_body(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_owned(), false),
        Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
    }
}

// ── IO orchestrator ───────────────────────────────────────────────────────────

/// Walk every agent's quarantine directory and read the files into raw
/// inputs.
///
/// `data_dir` is the runtime data root; the orchestrator computes
/// `<data_dir>/agents/<name>/inbox/quarantine/` for every immediate
/// subdirectory of `<data_dir>/agents/`. Missing directories are
/// silently skipped — a fresh agent with no rejected mail simply
/// contributes no entries.
///
/// File reads use `fs::read` which fails on unreadable files; those are
/// also skipped silently rather than aborting the whole snapshot.
#[must_use]
pub fn read_snapshot(data_dir: &Path) -> QuarantineSnapshot {
    let files = read_files(data_dir);
    build_snapshot(files)
}

/// Walk the agents tree and return one [`QuarantineFile`] per
/// quarantined envelope. Kept separate from [`build_snapshot`] so the
/// builder can be tested with hand-rolled fixtures.
fn read_files(data_dir: &Path) -> Vec<QuarantineFile> {
    let agents_root = data_dir.join("agents");
    let Ok(entries) = fs::read_dir(&agents_root) else {
        return Vec::new();
    };

    let mut out: Vec<QuarantineFile> = Vec::new();
    for entry in entries.flatten() {
        let agent_dir = entry.path();
        let Some(agent_name) = agent_dir.file_name().and_then(|o| o.to_str()) else {
            continue;
        };
        // Files at the agents root that are not directories (e.g.
        // `registry.toml`) are skipped naturally — read_dir on a
        // non-directory below yields Err.
        let quar_dir = agent_dir.join("inbox").join("quarantine");
        let Ok(quar_entries) = fs::read_dir(&quar_dir) else {
            continue;
        };
        for q in quar_entries.flatten() {
            let path = q.path();
            if !path.is_file() {
                continue;
            }
            let reason = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.rsplit_once('.'))
                .map(|(_, r)| r.to_owned())
                .unwrap_or_default();
            let mtime = q.metadata().and_then(|m| m.modified()).ok();
            let Ok(body_bytes) = fs::read(&path) else {
                continue;
            };
            out.push(QuarantineFile {
                path,
                recipient: agent_name.to_owned(),
                mtime,
                reason,
                body_bytes,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// Helper: write a quarantine file at the expected layout.
    fn seed(data_dir: &Path, agent: &str, filename: &str, contents: &[u8]) -> PathBuf {
        let dir = data_dir
            .join("agents")
            .join(agent)
            .join("inbox")
            .join("quarantine");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(filename);
        fs::write(&path, contents).unwrap();
        path
    }

    // R1: empty agents tree returns an empty snapshot — the IO orchestrator
    // tolerates a fresh data dir with no agents at all.
    #[test]
    fn read_snapshot_empty_data_dir_returns_empty() {
        let dir = tmp();
        let snap = read_snapshot(dir.path());
        assert!(snap.entries.is_empty());
        assert!(!snap.truncated);
    }

    // R2: an unparseable file surfaces as ParseFailure rather than dropping
    // the row. The operator needs to see the row to discard it.
    #[test]
    fn parse_failure_keeps_entry_with_filename_marker() {
        let dir = tmp();
        let path = seed(
            dir.path(),
            "lead",
            "garbage-stem.signature_invalid",
            b"not json at all",
        );
        let snap = read_snapshot(dir.path());
        assert_eq!(snap.entries.len(), 1);
        let e = &snap.entries[0];
        assert_eq!(e.path, path);
        assert_eq!(e.recipient, "lead");
        assert_eq!(e.reason, "signature_invalid");
        match &e.meta {
            EnvelopeMeta::ParseFailure { filename } => {
                assert!(filename.contains("garbage-stem"));
            }
            other @ EnvelopeMeta::Parsed { .. } => {
                panic!("expected ParseFailure, got {other:?}")
            }
        }
    }

    // R3: a file with no `.` in the name produces an empty reason field
    // rather than panicking. Defensive: the watcher always writes a
    // suffix, but the operator could `cp` a file in by hand.
    #[test]
    fn no_dot_in_filename_yields_empty_reason() {
        let dir = tmp();
        seed(dir.path(), "lead", "stem-no-suffix", b"x");
        let snap = read_snapshot(dir.path());
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].reason, "");
    }

    // R4: build_snapshot sorts newest first, with mtime-less entries
    // sinking to the bottom.
    #[test]
    fn build_snapshot_sorts_newest_first_none_at_end() {
        use std::time::{Duration, UNIX_EPOCH};
        let mk = |stem: &str, secs: Option<u64>| QuarantineFile {
            path: PathBuf::from(format!("/x/{stem}")),
            recipient: "lead".to_owned(),
            mtime: secs.map(|s| UNIX_EPOCH + Duration::from_secs(s)),
            reason: "signature_invalid".to_owned(),
            body_bytes: b"x".to_vec(),
        };
        let snap = build_snapshot(vec![
            mk("a", Some(100)),
            mk("b", None),
            mk("c", Some(300)),
            mk("d", Some(200)),
        ]);
        let stems: Vec<String> = snap
            .entries
            .iter()
            .map(|e| e.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(stems, vec!["c", "d", "a", "b"]);
    }

    // R5: build_snapshot caps at MAX_ENTRIES and reports the truncation.
    #[test]
    fn build_snapshot_caps_at_max_entries() {
        use std::time::{Duration, UNIX_EPOCH};
        let files: Vec<QuarantineFile> = (0..(MAX_ENTRIES + 5))
            .map(|i| QuarantineFile {
                path: PathBuf::from(format!("/x/{i}")),
                recipient: "lead".to_owned(),
                mtime: Some(UNIX_EPOCH + Duration::from_secs(u64::try_from(i).unwrap_or(0))),
                reason: "replay".to_owned(),
                body_bytes: b"x".to_vec(),
            })
            .collect();
        let snap = build_snapshot(files);
        assert_eq!(snap.entries.len(), MAX_ENTRIES);
        assert!(snap.truncated);
    }

    // R6: agents/registry.toml at the agents root is gracefully ignored
    // (it's a file, not a dir; read_dir on `inbox/quarantine` inside a
    // non-directory fails and we skip).
    #[test]
    fn registry_toml_alongside_agent_dirs_is_skipped() {
        let dir = tmp();
        seed(dir.path(), "lead", "abc.replay", b"x");
        let registry = dir.path().join("agents").join("registry.toml");
        fs::write(&registry, "name = \"lead\"\n").unwrap();
        let snap = read_snapshot(dir.path());
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].recipient, "lead");
    }
}
