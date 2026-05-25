//! Per-operator session memory for the Reeve TUI.
//!
//! A small TOML file at `<state_dir>/session.toml` records what the operator
//! was doing when they last quit the TUI, so the next `reeve` / `reeve
//! attach` invocation can resume in the right place instead of always
//! landing on the panopticon.
//!
//! Scope is deliberately minimal — one field today (the role name of the
//! last agent the operator was chatting with), room for additional fields
//! later (last panopticon focus row, last screen mode, last operator-set
//! filter). The file is best-effort: a missing, unreadable, or malformed
//! session is treated as "no prior session" and the TUI falls back to the
//! panopticon. There is never a user-visible failure from a session-file
//! problem.
//!
//! Writes happen only on **clean exit from a chat screen**. Quitting from
//! the panopticon does not write — that would overwrite a valid prior chat
//! record with a panopticon visit and the next invocation would land
//! somewhere the operator did not ask for.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Filename inside `<state_dir>/` where session memory lives.
const SESSION_FILE: &str = "session.toml";

/// On-disk session record. New optional fields can be added with
/// `#[serde(default)]` so older files keep parsing.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Role name (`"lead"`, `"worker-abc12345"`, …) of the agent the
    /// operator was chatting with when they last quit. `None` when the
    /// operator's last action was on the panopticon or no session file
    /// exists.
    #[serde(default)]
    pub last_agent: Option<String>,
}

/// Path to the session file given a runtime state directory. Plumbs to
/// `<state_dir>/session.toml` — sibling to `runtime.lock`, `runtime.pid`,
/// `daemon.log`. The state directory itself is conventionally
/// `$XDG_STATE_HOME/reeve` (see `reeve_runtime::default_state_dir`).
#[must_use]
pub fn default_path(state_dir: &Path) -> PathBuf {
    state_dir.join(SESSION_FILE)
}

/// Read the session file at `path`. Any error — missing file, permission
/// denied, malformed TOML — returns the default empty [`Session`]. The
/// TUI is never blocked by a session-file problem; the worst-case
/// presentation is "no prior session, open the panopticon".
#[must_use]
pub fn read(path: &Path) -> Session {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Session::default();
    };
    toml::from_str(&text).unwrap_or_default()
}

/// Write the session record atomically. Stages to a temp file in the same
/// directory, fsyncs, then renames over the target — the
/// `tmp + fsync + rename` idiom `reeve_runtime::fs_util::atomic_write_file`
/// uses for `inbox/new/` deposits and `agent.toml` writes, replicated here
/// because that helper is `pub(crate)` in the runtime. Creates the parent
/// directory if it does not exist.
///
/// # Errors
///
/// Returns an `io::Error` when the parent directory cannot be created,
/// the temp file cannot be written or fsynced, or the rename fails.
/// Callers in the TUI exit path log and swallow — the operator should not
/// see a TUI exit fail because of a session-file write.
pub fn write(path: &Path, session: &Session) -> std::io::Result<()> {
    use std::io::Write as _;

    let body = toml::to_string(session)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?;

    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "session path has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;

    // `NamedTempFile::new_in(parent)` produces a uniquely-named temp file
    // in the same filesystem as `path`, so `persist` becomes an atomic
    // rename(2). Same shape as `atomic_write_file` in the runtime: tmp →
    // fsync(file) → persist → fsync(parent). The trailing parent-dir
    // fsync makes the rename itself durable; without it a crash between
    // `persist` and the OS flush could revert to the prior session
    // record (best-effort behaviour, but the runtime's atomic-write
    // precedent does the full belt-and-suspenders so we do too).
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(body.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    // Best-effort dir fsync — non-Unix platforms or open() failure here
    // does not invalidate the rename, so swallow the error.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    // S1: round-trip — write then read produces the same session.
    #[test]
    fn round_trip_preserves_last_agent() {
        let tmp = tempdir().unwrap();
        let path = default_path(tmp.path());
        let session = Session {
            last_agent: Some("worker-abc12345".to_owned()),
        };
        write(&path, &session).unwrap();
        let back = read(&path);
        assert_eq!(back, session);
    }

    // S2: missing file reads as the default (empty) session — no error.
    // This is the "no prior session" path; the TUI lands on the panopticon.
    #[test]
    fn missing_file_reads_as_default() {
        let tmp = tempdir().unwrap();
        let path = default_path(tmp.path());
        assert_eq!(read(&path), Session::default());
        assert!(read(&path).last_agent.is_none());
    }

    // S3: malformed TOML reads as default rather than panicking — the
    // session file is best-effort, not load-bearing.
    #[test]
    fn malformed_file_reads_as_default() {
        let tmp = tempdir().unwrap();
        let path = default_path(tmp.path());
        std::fs::write(&path, "this is not toml === =").unwrap();
        assert_eq!(read(&path), Session::default());
    }

    // S4: `last_agent` is an explicit Option so the file can record
    // "no agent" without erroring. Lets a future `clear-session`
    // operation work by writing `Session::default()` rather than
    // deleting the file.
    #[test]
    fn empty_session_serializes_and_round_trips() {
        let tmp = tempdir().unwrap();
        let path = default_path(tmp.path());
        write(&path, &Session::default()).unwrap();
        assert_eq!(read(&path), Session::default());
    }

    // S5: write creates the state directory if it doesn't exist. The
    // first `reeve` invocation may run before the daemon has created
    // anything under state_dir.
    #[test]
    fn write_creates_parent_directory() {
        let tmp = tempdir().unwrap();
        let nested = tmp.path().join("does").join("not").join("exist");
        let path = nested.join(SESSION_FILE);
        write(
            &path,
            &Session {
                last_agent: Some("lead".to_owned()),
            },
        )
        .unwrap();
        assert!(path.exists());
    }

    // S6: the rendered TOML carries the field name the spec documents —
    // `last_agent = "lead"`. Pinned so a future field rename catches the
    // operator-facing contract change.
    #[test]
    fn serialized_uses_documented_field_name() {
        let session = Session {
            last_agent: Some("lead".to_owned()),
        };
        let text = toml::to_string(&session).unwrap();
        assert!(
            text.contains(r#"last_agent = "lead""#),
            "expected `last_agent = \"lead\"`; got: {text:?}"
        );
    }
}
