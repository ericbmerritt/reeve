//! Durable team rosters.
//!
//! A team is a standing roster of durable agents, formed from a team
//! template (`teams/<name>.toml`, the shippable configuration artifact this
//! module does *not* own). Template : team :: persona : agent — forming
//! instantiates the template by minting member agents as new durable
//! identities and binding them into the roster recorded here. See
//! `specs/reeve-organization.md` § Team.
//!
//! Rosters live at `<data-root>/rosters/<name>.toml`. Dissolved rosters
//! persist (names are never reused — same permanence rule as agents and
//! engagements); dissolution records the per-member disposition the
//! operator chose.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::fs_util::{atomic_write_file, ensure_directory, read_nofollow_bounded, FsCheckError};

const ROSTER_DIR_MODE: u32 = 0o700;
const ROSTER_FILE_MODE: u32 = 0o600;
const MAX_ROSTER_BYTES: u64 = 256 * 1024;

/// Lifecycle state of a team.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TeamState {
    Formed,
    Dissolved,
}

/// One member of a standing roster.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamMemberRecord {
    /// The member's durable agent name in the agent registry.
    pub agent_name: String,
    /// Role label from the template (e.g. `"lead"`, `"reviewer"`).
    pub role_label: String,
    /// Persona the member was minted from.
    pub persona_name: String,
}

/// Per-member outcome recorded at dissolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberDisposition {
    /// The identity ends with the team: incarnation wound down, record
    /// archival, name never reusable.
    Retired,
    /// The agent continues as a teamless standing agent.
    Released,
}

/// Persisted team roster.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamRecord {
    /// Unique-per-estate team name. Never reused, even after dissolution.
    pub name: String,
    /// Template this roster was formed from.
    pub template_name: String,
    /// Role label of the member who is the operator's first point of
    /// contact; `reeve attach` with no agent argument resolves through it.
    pub lead_role: String,
    pub members: Vec<TeamMemberRecord>,
    pub state: TeamState,
    #[serde(with = "time::serde::rfc3339")]
    pub formed_at: OffsetDateTime,
    /// Per-member dispositions recorded at dissolution, keyed by agent
    /// name. Empty while the team stands.
    #[serde(default)]
    pub dispositions: BTreeMap<String, MemberDisposition>,
}

impl TeamRecord {
    pub fn lead_member_name(&self) -> Option<&str> {
        self.members
            .iter()
            .find(|m| m.role_label == self.lead_role)
            .map(|m| m.agent_name.as_str())
    }
}

/// Errors produced by the roster store.
#[derive(Debug)]
pub enum TeamError {
    InvalidName {
        name: String,
    },
    /// The name was ever used before — team names are never reused.
    NameTaken {
        name: String,
    },
    NotFound {
        name: String,
    },
    WrongState {
        name: String,
        actual: TeamState,
    },
    Io {
        path: PathBuf,
        source: io::Error,
    },
    Toml {
        path: PathBuf,
        message: String,
    },
}

impl fmt::Display for TeamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName { name } => write!(f, "invalid team name {name:?}"),
            Self::NameTaken { name } => write!(
                f,
                "team name {name:?} was already used; names are never reused"
            ),
            Self::NotFound { name } => write!(f, "no team named {name:?}"),
            Self::WrongState { name, actual } => write!(f, "team {name:?} is {actual:?}"),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Toml { path, message } => write!(f, "{}: {message}", path.display()),
        }
    }
}

impl std::error::Error for TeamError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InvalidName { .. }
            | Self::NameTaken { .. }
            | Self::NotFound { .. }
            | Self::WrongState { .. }
            | Self::Toml { .. } => None,
        }
    }
}

impl TeamError {
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

/// On-disk roster store rooted at `<data-root>/rosters/`.
///
/// Stateless between calls, like the engagement store: every operation
/// reads and writes the record file directly so restarts and concurrent
/// readers see the durable truth.
#[derive(Debug)]
pub struct TeamRegistry {
    rosters_root: PathBuf,
}

impl TeamRegistry {
    pub fn open(rosters_root: PathBuf) -> Result<Self, TeamError> {
        ensure_directory(&rosters_root, ROSTER_DIR_MODE).map_err(TeamError::from_fs)?;
        Ok(Self { rosters_root })
    }

    /// Record a newly formed team. Refuses names ever used before —
    /// dissolved rosters keep their files precisely so this check holds.
    ///
    /// The write itself is no-clobber, not just a preceding existence
    /// check: a check-then-clobbering-write pair leaves a TOCTOU window
    /// where two concurrent `form` calls for the same name can both pass
    /// the check and the later `persist` silently overwrites the
    /// earlier one, defeating name permanence.
    pub fn form(&self, record: &TeamRecord) -> Result<(), TeamError> {
        crate::agent_fs::validate_agent_name(&record.name).map_err(|_| TeamError::InvalidName {
            name: record.name.clone(),
        })?;
        let path = self.roster_path(&record.name);
        let body = toml::to_string(record).map_err(|e| TeamError::Toml {
            path: path.clone(),
            message: e.to_string(),
        })?;
        match crate::fs_util::atomic_write_file_no_clobber(
            &path,
            &self.rosters_root,
            body.as_bytes(),
            ROSTER_FILE_MODE,
        ) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => {
                Err(TeamError::NameTaken {
                    name: record.name.clone(),
                })
            }
            Err(source) => Err(TeamError::Io { path, source }),
        }
    }

    /// Mark a standing team dissolved, recording each member's disposition.
    pub fn dissolve(
        &self,
        name: &str,
        dispositions: BTreeMap<String, MemberDisposition>,
    ) -> Result<TeamRecord, TeamError> {
        let mut record = self.get(name)?;
        if record.state != TeamState::Formed {
            return Err(TeamError::WrongState {
                name: name.to_owned(),
                actual: record.state,
            });
        }
        record.state = TeamState::Dissolved;
        record.dispositions = dispositions;
        self.write_record(&record)?;
        Ok(record)
    }

    pub fn get(&self, name: &str) -> Result<TeamRecord, TeamError> {
        crate::agent_fs::validate_agent_name(name).map_err(|_| TeamError::InvalidName {
            name: name.to_owned(),
        })?;
        let path = self.roster_path(name);
        let body = read_nofollow_bounded(&path, MAX_ROSTER_BYTES).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                TeamError::NotFound {
                    name: name.to_owned(),
                }
            } else {
                TeamError::Io {
                    path: path.clone(),
                    source,
                }
            }
        })?;
        toml::from_str(&body).map_err(|e| TeamError::Toml {
            path,
            message: e.to_string(),
        })
    }

    /// All rosters, sorted by name. Symlinked entries are skipped; a torn
    /// roster file surfaces as an error rather than being silently absent.
    /// Non-`.toml` entries (stray editor/OS files such as `.DS_Store`) are
    /// skipped rather than fed to `get`, which would otherwise fail the
    /// whole listing on an unrelated file.
    pub fn list(&self) -> Result<Vec<TeamRecord>, TeamError> {
        let entries = fs::read_dir(&self.rosters_root).map_err(|source| TeamError::Io {
            path: self.rosters_root.clone(),
            source,
        })?;
        let mut records = BTreeMap::new();
        for entry in entries {
            let entry = entry.map_err(|source| TeamError::Io {
                path: self.rosters_root.clone(),
                source,
            })?;
            let file_type = entry.file_type().map_err(|source| TeamError::Io {
                path: entry.path(),
                source,
            })?;
            if !file_type.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
                continue;
            };
            records.insert(name.clone(), self.get(&name)?);
        }
        Ok(records.into_values().collect())
    }

    fn roster_path(&self, name: &str) -> PathBuf {
        self.rosters_root.join(name).with_extension("toml")
    }

    fn write_record(&self, record: &TeamRecord) -> Result<(), TeamError> {
        let path = self.roster_path(&record.name);
        let body = toml::to_string(record).map_err(|e| TeamError::Toml {
            path: path.clone(),
            message: e.to_string(),
        })?;
        atomic_write_file(&path, &self.rosters_root, body.as_bytes(), ROSTER_FILE_MODE)
            .map_err(|source| TeamError::Io { path, source })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::secure_dir;

    fn record(name: &str) -> TeamRecord {
        TeamRecord {
            name: name.to_owned(),
            template_name: "default".to_owned(),
            lead_role: "lead".to_owned(),
            members: vec![
                TeamMemberRecord {
                    agent_name: format!("{name}-lead"),
                    role_label: "lead".to_owned(),
                    persona_name: "lead".to_owned(),
                },
                TeamMemberRecord {
                    agent_name: format!("{name}-reviewer"),
                    role_label: "reviewer".to_owned(),
                    persona_name: "reviewer".to_owned(),
                },
            ],
            state: TeamState::Formed,
            formed_at: OffsetDateTime::from_unix_timestamp(1_760_000_000).unwrap(),
            dispositions: BTreeMap::new(),
        }
    }

    fn store(root: &std::path::Path) -> TeamRegistry {
        TeamRegistry::open(root.join("rosters")).unwrap()
    }

    #[test]
    fn form_get_and_lead_resolution_round_trip() {
        let tmp = secure_dir();
        let registry = store(tmp.path());
        registry.form(&record("alpha")).unwrap();

        let loaded = store(tmp.path()).get("alpha").unwrap();
        assert_eq!(loaded.state, TeamState::Formed);
        assert_eq!(loaded.lead_member_name(), Some("alpha-lead"));
        assert_eq!(loaded.members.len(), 2);
    }

    #[test]
    fn team_names_are_never_reused_even_after_dissolution() {
        let tmp = secure_dir();
        let registry = store(tmp.path());
        registry.form(&record("once")).unwrap();
        let mut dispositions = BTreeMap::new();
        dispositions.insert("once-lead".to_owned(), MemberDisposition::Retired);
        dispositions.insert("once-reviewer".to_owned(), MemberDisposition::Released);
        let dissolved = registry.dissolve("once", dispositions).unwrap();
        assert_eq!(dissolved.state, TeamState::Dissolved);
        assert_eq!(
            dissolved.dispositions.get("once-reviewer"),
            Some(&MemberDisposition::Released)
        );

        let err = registry.form(&record("once")).unwrap_err();
        assert!(matches!(err, TeamError::NameTaken { name } if name == "once"));
    }

    #[test]
    fn dissolving_twice_is_a_wrong_state_error() {
        let tmp = secure_dir();
        let registry = store(tmp.path());
        registry.form(&record("team")).unwrap();
        registry.dissolve("team", BTreeMap::new()).unwrap();
        let err = registry.dissolve("team", BTreeMap::new()).unwrap_err();
        assert!(matches!(
            err,
            TeamError::WrongState {
                actual: TeamState::Dissolved,
                ..
            }
        ));
    }

    #[test]
    fn list_sorts_and_reflects_state() {
        let tmp = secure_dir();
        let registry = store(tmp.path());
        registry.form(&record("beta")).unwrap();
        registry.form(&record("alpha")).unwrap();
        registry.dissolve("beta", BTreeMap::new()).unwrap();
        let all = registry.list().unwrap();
        let names: Vec<&str> = all.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(all[1].state, TeamState::Dissolved);
    }
}
