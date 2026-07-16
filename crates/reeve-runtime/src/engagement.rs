//! Durable engagement records.
//!
//! An engagement is a named piece of work the estate has taken on: a
//! purpose, an optional working root (its context), a lifecycle state, and
//! — in later phases — staffing and a memory file. See
//! `specs/reeve-organization.md` § Engagement.
//!
//! Records live at `<data-root>/engagements/<name>/record.toml`, one
//! directory per engagement. Directories persist after close: engagement
//! names are never reused, and directory existence doubles as the
//! name-permanence check. Context is immutable after open — no API on this
//! type can change a recorded root; work on a different root is a different
//! engagement.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::fs_util::{atomic_write_file, ensure_directory, read_nofollow_bounded, FsCheckError};

const ENGAGEMENT_DIR_MODE: u32 = 0o700;
const ENGAGEMENT_FILE_MODE: u32 = 0o600;
const MAX_RECORD_BYTES: u64 = 64 * 1024;

// ── State ─────────────────────────────────────────────────────────────────────

/// Lifecycle state of an engagement.
///
/// There is no terminal state: a closed engagement can be reopened with its
/// record (and, later, its memory file) intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngagementState {
    Open,
    Closed,
}

/// The unit currently staffed to a top-level engagement: a standing team, or
/// a lone teamless agent (the degenerate unit of one). Per
/// `specs/reeve-organization.md` § Engagement, "a team member is never a
/// top-level unit on its own" — a `Team` variant names the roster, not one
/// of its members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum StaffedUnit {
    Team { name: String },
    Agent { name: String },
}

// ── Record ────────────────────────────────────────────────────────────────────

/// Persisted engagement record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngagementRecord {
    /// Unique-per-estate engagement name. Never reused, even after close.
    pub name: String,
    /// What the work is, in prose.
    pub purpose: String,
    /// The working root when the work is repo- or directory-bound; `None`
    /// for rootless work. Immutable after open.
    #[serde(default)]
    pub root: Option<PathBuf>,
    /// Lifecycle state.
    pub state: EngagementState,
    /// Wall-clock time the engagement was first opened.
    #[serde(with = "time::serde::rfc3339")]
    pub opened_at: OffsetDateTime,
    /// Parent engagement name for sub-engagements. Always `None` until the
    /// delegation phase ships nesting.
    #[serde(default)]
    pub parent: Option<String>,
    /// The unit (team or lone teamless agent) currently staffed here, if
    /// any — at most one at a time, serially re-staffable. This is the
    /// single source of truth for staffing (not mirrored on the team
    /// record): "is this team already staffed elsewhere" is answered by
    /// scanning engagements, not by a second pointer that could drift out
    /// of sync with this one. `#[serde(default)]`: absent in records
    /// written before staffing existed, which is the same as unstaffed.
    #[serde(default)]
    pub staffed_unit: Option<StaffedUnit>,
}

// ── Error ─────────────────────────────────────────────────────────────────────

/// Errors produced by the engagement store.
#[derive(Debug)]
pub enum EngagementError {
    /// The name failed validation (empty, path separators, control chars).
    InvalidName { name: String },
    /// The name was ever used before — names are never reused.
    NameTaken { name: String },
    /// No engagement with this name exists.
    NotFound { name: String },
    /// The operation requires a different lifecycle state (e.g. closing an
    /// already-closed engagement).
    WrongState {
        name: String,
        actual: EngagementState,
    },
    /// A supplied root was not an absolute path.
    RelativeRoot { path: PathBuf },
    /// Underlying filesystem error.
    Io { path: PathBuf, source: io::Error },
    /// A record file failed to parse or serialize.
    Toml { path: PathBuf, message: String },
}

impl fmt::Display for EngagementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name } => write!(f, "invalid engagement name {name:?}"),
            Self::NameTaken { name } => write!(
                f,
                "engagement name {name:?} was already used; names are never reused"
            ),
            Self::NotFound { name } => write!(f, "no engagement named {name:?}"),
            Self::WrongState { name, actual } => {
                write!(f, "engagement {name:?} is {actual:?}")
            }
            Self::RelativeRoot { path } => {
                write!(
                    f,
                    "engagement root must be absolute, got {}",
                    path.display()
                )
            }
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Toml { path, message } => write!(f, "{}: {message}", path.display()),
        }
    }
}

impl std::error::Error for EngagementError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InvalidName { .. }
            | Self::NameTaken { .. }
            | Self::NotFound { .. }
            | Self::WrongState { .. }
            | Self::RelativeRoot { .. }
            | Self::Toml { .. } => None,
        }
    }
}

impl EngagementError {
    fn from_fs(err: FsCheckError) -> Self {
        match err {
            FsCheckError::Io { path, source } => Self::Io { path, source },
            FsCheckError::Symlink { path } => Self::Io {
                path,
                source: io::Error::other("path is a symlink; refusing to follow"),
            },
            FsCheckError::NotADirectory { path } => Self::Io {
                path,
                source: io::Error::other("path exists but is not a directory"),
            },
            FsCheckError::WrongMode {
                path,
                actual,
                expected,
            } => Self::Io {
                path,
                source: io::Error::other(format!(
                    "directory mode {actual:o} does not match required {expected:o}"
                )),
            },
        }
    }
}

// ── Context resolution ────────────────────────────────────────────────────────

/// Walk upward from `start` to the outermost directory containing a `.jj` or
/// `.git` entry. Falls back to `start` itself when no VCS marker is found —
/// the effectors spec's resolution rule: "the VCS toplevel of the launch
/// directory if one is detectable, else the launch directory itself".
/// Outermost (not first) match keeps a jj-colocated repo checked out inside
/// another repo resolving the same way jj itself would.
///
/// Shared by every front door that opens an engagement (CLI, TUI
/// slash-command) so comparable inputs receive comparable resolution.
pub fn resolve_vcs_toplevel(start: &std::path::Path) -> Result<PathBuf, io::Error> {
    let start = start.canonicalize()?;
    let mut toplevel: Option<PathBuf> = None;
    let mut cursor: Option<&std::path::Path> = Some(&start);
    while let Some(dir) = cursor {
        if dir.join(".jj").exists() || dir.join(".git").exists() {
            toplevel = Some(dir.to_path_buf());
        }
        cursor = dir.parent();
    }
    Ok(toplevel.unwrap_or(start))
}

// ── Store ─────────────────────────────────────────────────────────────────────

/// On-disk engagement store rooted at `<data-root>/engagements/`.
///
/// Stateless between calls: every operation reads and writes the record
/// file directly, so daemon restarts (and concurrent readers like the CLI's
/// `list`) always see the durable truth.
#[derive(Debug, Clone)]
pub struct EngagementRegistry {
    engagements_root: PathBuf,
}

impl EngagementRegistry {
    /// Open (or create) the store. `engagements_root` is created with mode
    /// `0o700` when absent; an existing directory must already carry it.
    pub fn open(engagements_root: PathBuf) -> Result<Self, EngagementError> {
        ensure_directory(&engagements_root, ENGAGEMENT_DIR_MODE)
            .map_err(EngagementError::from_fs)?;
        Ok(Self { engagements_root })
    }

    /// Open a new engagement. Refuses names that were ever used before —
    /// closed engagements keep their directories precisely so this check
    /// holds across the estate's whole history.
    pub fn open_engagement(
        &self,
        name: &str,
        purpose: &str,
        root: Option<PathBuf>,
        opened_at: OffsetDateTime,
    ) -> Result<EngagementRecord, EngagementError> {
        crate::agent_fs::validate_agent_name(name).map_err(|_| EngagementError::InvalidName {
            name: name.to_owned(),
        })?;
        if let Some(ref root) = root {
            if !root.is_absolute() {
                return Err(EngagementError::RelativeRoot { path: root.clone() });
            }
        }
        let dir = self.engagements_root.join(name);
        if dir.symlink_metadata().is_ok() {
            return Err(EngagementError::NameTaken {
                name: name.to_owned(),
            });
        }
        ensure_directory(&dir, ENGAGEMENT_DIR_MODE).map_err(EngagementError::from_fs)?;
        let record = EngagementRecord {
            name: name.to_owned(),
            purpose: purpose.to_owned(),
            root,
            state: EngagementState::Open,
            opened_at,
            parent: None,
            staffed_unit: None,
        };
        self.write_record(&record)?;
        Ok(record)
    }

    /// Close an open engagement. The record (and directory) persist; only
    /// the state changes. Context is untouched.
    pub fn close(&self, name: &str) -> Result<EngagementRecord, EngagementError> {
        self.transition(name, EngagementState::Open, EngagementState::Closed)
    }

    /// Reopen a closed engagement. The context is whatever it was at open —
    /// reopening restores, never rewrites.
    pub fn reopen(&self, name: &str) -> Result<EngagementRecord, EngagementError> {
        self.transition(name, EngagementState::Closed, EngagementState::Open)
    }

    /// Set (or clear, with `None`) the staffed unit on an engagement
    /// record. No state precondition here — staffing and lifecycle state
    /// are independent axes at this layer; the estate coordinator enforces
    /// "must be `Open` and not already staffed" before calling this.
    pub fn set_staffed_unit(
        &self,
        name: &str,
        staffed_unit: Option<StaffedUnit>,
    ) -> Result<EngagementRecord, EngagementError> {
        let mut record = self.get(name)?;
        record.staffed_unit = staffed_unit;
        self.write_record(&record)?;
        Ok(record)
    }

    /// Read the record for `name`.
    pub fn get(&self, name: &str) -> Result<EngagementRecord, EngagementError> {
        crate::agent_fs::validate_agent_name(name).map_err(|_| EngagementError::InvalidName {
            name: name.to_owned(),
        })?;
        let path = self.record_path(name);
        let body = read_nofollow_bounded(&path, MAX_RECORD_BYTES).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                EngagementError::NotFound {
                    name: name.to_owned(),
                }
            } else {
                EngagementError::Io {
                    path: path.clone(),
                    source,
                }
            }
        })?;
        toml::from_str(&body).map_err(|e| EngagementError::Toml {
            path,
            message: e.to_string(),
        })
    }

    /// All records, sorted by name. Unparseable record files surface as
    /// errors rather than being skipped — a torn engagement record is
    /// operator-visible, not silently absent.
    pub fn list(&self) -> Result<Vec<EngagementRecord>, EngagementError> {
        let entries =
            fs::read_dir(&self.engagements_root).map_err(|source| EngagementError::Io {
                path: self.engagements_root.clone(),
                source,
            })?;
        let mut records = BTreeMap::new();
        for entry in entries {
            let entry = entry.map_err(|source| EngagementError::Io {
                path: self.engagements_root.clone(),
                source,
            })?;
            // file_type() does not follow symlinks: a symlinked entry is
            // skipped rather than traversed, so a link planted inside
            // engagements/ cannot make list() read outside the store root.
            let file_type = entry.file_type().map_err(|source| EngagementError::Io {
                path: entry.path(),
                source,
            })?;
            if !file_type.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let record = self.get(&name)?;
            records.insert(name, record);
        }
        Ok(records.into_values().collect())
    }

    fn transition(
        &self,
        name: &str,
        expected: EngagementState,
        next: EngagementState,
    ) -> Result<EngagementRecord, EngagementError> {
        let mut record = self.get(name)?;
        if record.state != expected {
            return Err(EngagementError::WrongState {
                name: name.to_owned(),
                actual: record.state,
            });
        }
        record.state = next;
        self.write_record(&record)?;
        Ok(record)
    }

    fn record_path(&self, name: &str) -> PathBuf {
        self.engagements_root.join(name).join("record.toml")
    }

    fn write_record(&self, record: &EngagementRecord) -> Result<(), EngagementError> {
        let dir = self.engagements_root.join(&record.name);
        let path = self.record_path(&record.name);
        let body = toml::to_string(record).map_err(|e| EngagementError::Toml {
            path: path.clone(),
            message: e.to_string(),
        })?;
        atomic_write_file(&path, &dir, body.as_bytes(), ENGAGEMENT_FILE_MODE)
            .map_err(|source| EngagementError::Io { path, source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    use crate::test_support::secure_dir;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap()
    }

    fn store(root: &Path) -> EngagementRegistry {
        EngagementRegistry::open(root.join("engagements")).unwrap()
    }

    #[test]
    fn open_writes_durable_record_visible_to_fresh_store() {
        let tmp = secure_dir();
        let registry = store(tmp.path());
        let record = registry
            .open_engagement(
                "reconciler",
                "modernize it",
                Some(PathBuf::from("/repo")),
                now(),
            )
            .unwrap();
        assert_eq!(record.state, EngagementState::Open);

        let reloaded = store(tmp.path()).get("reconciler").unwrap();
        assert_eq!(reloaded.name, "reconciler");
        assert_eq!(reloaded.purpose, "modernize it");
        assert_eq!(reloaded.root, Some(PathBuf::from("/repo")));
        assert_eq!(reloaded.state, EngagementState::Open);
        assert_eq!(reloaded.parent, None);
    }

    #[test]
    fn open_engagement_starts_unstaffed() {
        let tmp = secure_dir();
        let record = store(tmp.path())
            .open_engagement("reconciler", "modernize it", None, now())
            .unwrap();
        assert_eq!(record.staffed_unit, None);
    }

    // A record written before staffing existed (no `staffed_unit` key at
    // all) still parses, defaulting to unstaffed — the same
    // backward-compatibility guarantee `root`/`parent` already established.
    #[test]
    fn record_without_staffed_unit_key_parses_as_unstaffed() {
        let tmp = secure_dir();
        let registry = store(tmp.path());
        registry
            .open_engagement("legacy", "pre-staffing record", None, now())
            .unwrap();
        let path = tmp
            .path()
            .join("engagements")
            .join("legacy")
            .join("record.toml");
        let pre_staffing_toml = r#"
name = "legacy"
purpose = "pre-staffing record"
state = "open"
opened_at = "2025-10-09T15:33:20Z"
"#;
        fs::write(&path, pre_staffing_toml).unwrap();

        let reloaded = registry.get("legacy").unwrap();
        assert_eq!(reloaded.staffed_unit, None);
    }

    #[test]
    fn names_are_never_reused_even_after_close() {
        let tmp = secure_dir();
        let registry = store(tmp.path());
        registry
            .open_engagement("once", "first", None, now())
            .unwrap();
        registry.close("once").unwrap();

        let err = registry
            .open_engagement("once", "second", None, now())
            .unwrap_err();
        assert!(matches!(err, EngagementError::NameTaken { name } if name == "once"));
    }

    #[test]
    fn reopen_restores_identical_context() {
        let tmp = secure_dir();
        let registry = store(tmp.path());
        registry
            .open_engagement("work", "purpose", Some(PathBuf::from("/repo/a")), now())
            .unwrap();
        registry.close("work").unwrap();
        let reopened = registry.reopen("work").unwrap();
        assert_eq!(reopened.state, EngagementState::Open);
        assert_eq!(reopened.root, Some(PathBuf::from("/repo/a")));
        assert_eq!(reopened.purpose, "purpose");
    }

    #[test]
    fn transitions_require_the_expected_state() {
        let tmp = secure_dir();
        let registry = store(tmp.path());
        registry.open_engagement("work", "p", None, now()).unwrap();

        let err = registry.reopen("work").unwrap_err();
        assert!(matches!(
            err,
            EngagementError::WrongState {
                actual: EngagementState::Open,
                ..
            }
        ));
        registry.close("work").unwrap();
        let err = registry.close("work").unwrap_err();
        assert!(matches!(
            err,
            EngagementError::WrongState {
                actual: EngagementState::Closed,
                ..
            }
        ));
    }

    #[test]
    fn relative_root_is_refused() {
        let tmp = secure_dir();
        let registry = store(tmp.path());
        let err = registry
            .open_engagement("rel", "p", Some(PathBuf::from("repo/a")), now())
            .unwrap_err();
        assert!(matches!(err, EngagementError::RelativeRoot { .. }));
    }

    #[test]
    fn invalid_names_are_refused() {
        let tmp = secure_dir();
        let registry = store(tmp.path());
        for bad in ["", "a/b", ".", "..", "nul\0byte"] {
            let err = registry.open_engagement(bad, "p", None, now()).unwrap_err();
            assert!(
                matches!(err, EngagementError::InvalidName { .. }),
                "expected InvalidName for {bad:?}"
            );
        }
    }

    #[test]
    fn vcs_toplevel_resolves_outermost_marker_from_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".jj")).unwrap();
        fs::create_dir_all(repo.join("crates").join("deep")).unwrap();

        let resolved = resolve_vcs_toplevel(&repo.join("crates").join("deep")).unwrap();
        assert_eq!(resolved, repo.canonicalize().unwrap());
    }

    #[test]
    fn vcs_toplevel_falls_back_to_start_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        let resolved = resolve_vcs_toplevel(&plain).unwrap();
        assert_eq!(resolved, plain.canonicalize().unwrap());
    }

    #[test]
    fn vcs_toplevel_detects_git_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("gitrepo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        let resolved = resolve_vcs_toplevel(&repo.join("src")).unwrap();
        assert_eq!(resolved, repo.canonicalize().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn list_skips_symlinked_entries() {
        let tmp = secure_dir();
        let registry = store(tmp.path());
        registry.open_engagement("real", "p", None, now()).unwrap();

        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(outside.path().join("planted")).unwrap();
        fs::write(
            outside.path().join("planted").join("record.toml"),
            b"name = \"planted\"\npurpose = \"x\"\nstate = \"open\"\nopened_at = \"2026-01-01T00:00:00Z\"",
        )
        .unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("planted"),
            tmp.path().join("engagements").join("planted"),
        )
        .unwrap();

        let all = registry.list().unwrap();
        let names: Vec<&str> = all.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["real"], "symlinked entries must be skipped");
    }

    #[test]
    fn list_returns_all_records_sorted() {
        let tmp = secure_dir();
        let registry = store(tmp.path());
        registry.open_engagement("beta", "b", None, now()).unwrap();
        registry.open_engagement("alpha", "a", None, now()).unwrap();
        registry.close("beta").unwrap();

        let all = registry.list().unwrap();
        let names: Vec<&str> = all.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(all[1].state, EngagementState::Closed);
    }
}
