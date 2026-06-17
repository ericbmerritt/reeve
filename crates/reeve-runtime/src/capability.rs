//! Capability profiles for agent authority.
//!
//! A `CapabilityProfile` is a coarse policy artifact that answers "what
//! kinds of action may agents of this persona attempt at all?" It carries
//! category-level on/off filters and quantitative thresholds. Fine-grained
//! action allowlists live in the blacklist (phase 2) and classifier
//! (phase 4).
//!
//! Persona profiles live at `<data_dir>/personas/<name>/profile.toml` (next to
//! the persona's `config.toml`). At spawn time the coordinator snapshots the
//! profile verbatim to `<data_dir>/agents/<name>/profile.toml` via
//! [`crate::agent_fs::AgentDirs::profile_path`].
//!
//! The snapshot is immutable for a *running* agent: the in-memory thresholds
//! are loaded once at construction and are not reloaded mid-turn. However the
//! file may be updated externally (e.g., via the TUI Model tab) and will be
//! re-read on the next daemon restart or agent respawn.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fs_util;

// ── ToolCategory ───────────────────────────────────────────────────────────

/// The category a tool belongs to.
///
/// Each tool actor declares exactly one category; the authority check
/// verifies that category is enabled in the invoking agent's snapshotted
/// profile before executing the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCategory {
    ReadFiles,
    WriteFiles,
    ExecuteShell,
    GitRead,
    GitWrite,
    SpawnAgents,
    MessagePeers,
    NetworkEgress,
    WriteMemory,
    WriteConfiguration,
}

impl std::fmt::Display for ToolCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::ReadFiles => "read_files",
            Self::WriteFiles => "write_files",
            Self::ExecuteShell => "execute_shell",
            Self::GitRead => "git_read",
            Self::GitWrite => "git_write",
            Self::SpawnAgents => "spawn_agents",
            Self::MessagePeers => "message_peers",
            Self::NetworkEgress => "network_egress",
            Self::WriteMemory => "write_memory",
            Self::WriteConfiguration => "write_configuration",
        };
        f.write_str(s)
    }
}

// ── Thresholds ─────────────────────────────────────────────────────────────

/// Quantitative limits enforced by the runtime.
///
/// All fields are `Option`; absent means no limit for that threshold.
/// Phases 3 and 4 add the actual enforcement logic; this phase parses and
/// stores the values so the snapshot is complete from day one.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, Default)]
pub struct Thresholds {
    /// Maximum estimated model cost for a single agent session (USD).
    /// On trip: model calls refused; the event surfaces in the panopticon.
    pub cost_per_agent: Option<f64>,
    /// Maximum estimated cost for the entire agent tree across the session
    /// (USD). On trip: model calls refused across all agents in the tree.
    pub cost_per_session: Option<f64>,
    /// Maximum number of live subordinates this agent may have at once.
    /// On trip: spawn requests refused until a subordinate exits.
    pub max_concurrent_subordinates: Option<u32>,
    /// Maximum wall-clock seconds from task declaration to completion.
    /// On trip: agent transitions to `Exiting`; no new tool invocations or
    /// model calls accepted; in-flight work completes.
    pub max_task_duration_secs: Option<u64>,
}

// ── CapabilityProfile ──────────────────────────────────────────────────────

/// Wire representation of a capability profile on disk.
///
/// `enabled_categories` is optional. When absent the profile places no
/// category restriction — the agent may invoke any tool. When present, only
/// the listed categories are permitted; all others are denied. This is the
/// "opt-in restriction" model: most agents run unrestricted; focused agents
/// carry an explicit list.
///
/// New tool categories added in future ladders are automatically available
/// to unrestricted agents without requiring profile updates. Restricted
/// agents must explicitly add the new category if they need it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityProfile {
    /// Human-readable name for this profile (informational; not enforced).
    pub name: String,
    /// Schema version. Reject files with a version newer than supported.
    pub version: u32,
    /// Categories this profile permits. `None` (field absent in TOML) means
    /// all categories are allowed. An explicit list means only those
    /// categories are permitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_categories: Option<Vec<ToolCategory>>,
    /// Quantitative thresholds; absent fields mean no limit.
    #[serde(default)]
    pub thresholds: Thresholds,
}

impl CapabilityProfile {
    /// Returns `true` if `category` is permitted by this profile.
    ///
    /// When `enabled_categories` is `None`, all categories are allowed.
    /// When it is `Some`, only the listed categories are allowed.
    #[must_use]
    pub fn allows(&self, category: ToolCategory) -> bool {
        match &self.enabled_categories {
            None => true,
            Some(cats) => cats.contains(&category),
        }
    }
}

// ── Max supported schema version ───────────────────────────────────────────

/// Highest schema version this binary understands. Profiles with a higher
/// version are rejected at load time so a downgraded binary does not
/// silently misinterpret a newer format.
const MAX_SCHEMA_VERSION: u32 = 1;

// ── Load / write ───────────────────────────────────────────────────────────

/// Errors that can occur when loading a capability profile.
#[derive(Debug)]
pub enum ProfileError {
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: std::path::PathBuf,
        source: toml::de::Error,
    },
    UnsupportedVersion {
        path: std::path::PathBuf,
        found: u32,
        max: u32,
    },
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "profile I/O error at {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(f, "profile parse error at {}: {source}", path.display())
            }
            Self::UnsupportedVersion { path, found, max } => write!(
                f,
                "profile at {} has schema version {found}; max supported is {max}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for ProfileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::UnsupportedVersion { .. } => None,
        }
    }
}

/// Load a `CapabilityProfile` from `path`.
///
/// Returns [`ProfileError::Io`] when the file cannot be read (including
/// `ENOENT`), [`ProfileError::Parse`] on TOML errors, and
/// [`ProfileError::UnsupportedVersion`] when the schema version is newer
/// than the max version this binary understands.
pub fn load_capability_profile(path: &Path) -> Result<CapabilityProfile, ProfileError> {
    let body =
        fs_util::read_nofollow_bounded(path, 64 * 1024).map_err(|source| ProfileError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let profile: CapabilityProfile =
        toml::from_str(&body).map_err(|source| ProfileError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
    if profile.version > MAX_SCHEMA_VERSION {
        return Err(ProfileError::UnsupportedVersion {
            path: path.to_path_buf(),
            found: profile.version,
            max: MAX_SCHEMA_VERSION,
        });
    }
    Ok(profile)
}

/// Serialize `profile` to TOML and write it atomically to `path`.
///
/// Uses the same atomic-rename pattern as other runtime file writers so a
/// crash mid-write leaves the file at its prior value, never partially
/// overwritten.
pub fn write_capability_profile(
    path: &Path,
    profile: &CapabilityProfile,
) -> Result<(), ProfileError> {
    let body = toml::to_string_pretty(profile).unwrap_or_else(|_| {
        // CapabilityProfile only contains primitive types and vecs of
        // enum variants; serialization failure is not reachable.
        panic!("CapabilityProfile serialization failed; this is a bug")
    });
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs_util::atomic_write_file(path, dir, body.as_bytes(), 0o600).map_err(|source| {
        ProfileError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn unrestricted_profile_toml() -> &'static str {
        r#"
name = "lead"
version = 1

[thresholds]
cost_per_agent = 10.0
cost_per_session = 50.0
max_concurrent_subordinates = 5
max_task_duration_secs = 3600
"#
    }

    fn restricted_profile_toml() -> &'static str {
        r#"
name = "worker"
version = 1
enabled_categories = ["read_files", "git_read", "message_peers", "spawn_agents"]

[thresholds]
cost_per_agent = 2.0
"#
    }

    // CP1: absent enabled_categories means all categories allowed.
    #[test]
    fn unrestricted_profile_allows_all() {
        let p: CapabilityProfile = toml::from_str(unrestricted_profile_toml()).unwrap();
        assert_eq!(p.name, "lead");
        assert_eq!(p.version, 1);
        assert!(p.enabled_categories.is_none());
        assert!(p.allows(ToolCategory::SpawnAgents));
        assert!(p.allows(ToolCategory::MessagePeers));
        assert!(p.allows(ToolCategory::WriteFiles));
        assert!(p.allows(ToolCategory::ExecuteShell));
        assert!(p.allows(ToolCategory::NetworkEgress));
    }

    // CP2: a restricted profile only allows listed categories.
    #[test]
    fn restricted_profile_denies_unlisted() {
        let p: CapabilityProfile = toml::from_str(restricted_profile_toml()).unwrap();
        assert!(p.allows(ToolCategory::ReadFiles));
        assert!(p.allows(ToolCategory::GitRead));
        assert!(p.allows(ToolCategory::MessagePeers));
        assert!(p.allows(ToolCategory::SpawnAgents));
        assert!(!p.allows(ToolCategory::WriteFiles));
        assert!(!p.allows(ToolCategory::ExecuteShell));
        assert!(!p.allows(ToolCategory::GitWrite));
        assert!(!p.allows(ToolCategory::NetworkEgress));
        assert!(!p.allows(ToolCategory::WriteMemory));
        assert!(!p.allows(ToolCategory::WriteConfiguration));
    }

    // CP3: unknown field is rejected (deny_unknown_fields).
    #[test]
    fn unknown_field_rejected() {
        let bad = r#"
name = "x"
version = 1
surprise_field = "boom"
"#;
        let err = toml::from_str::<CapabilityProfile>(bad);
        assert!(err.is_err(), "expected parse error for unknown field");
    }

    // CP4: schema version newer than MAX_SCHEMA_VERSION is rejected.
    #[test]
    fn future_schema_version_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("profile.toml");
        std::fs::write(
            &path,
            r#"name = "x"
version = 999
"#,
        )
        .unwrap();
        let err = load_capability_profile(&path).unwrap_err();
        assert!(
            matches!(err, ProfileError::UnsupportedVersion { found: 999, .. }),
            "expected UnsupportedVersion; got {err}"
        );
    }

    // CP5: missing file returns ProfileError::Io.
    #[test]
    fn missing_file_returns_io_error() {
        let err = load_capability_profile(Path::new("/no/such/profile.toml")).unwrap_err();
        assert!(matches!(err, ProfileError::Io { .. }));
    }

    // CP6: round-trip write + load preserves all fields.
    #[test]
    fn round_trip_write_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("profile.toml");
        let original: CapabilityProfile = toml::from_str(restricted_profile_toml()).unwrap();
        write_capability_profile(&path, &original).unwrap();
        let loaded = load_capability_profile(&path).unwrap();
        assert_eq!(original, loaded);
    }

    // CP7: ToolCategory Display produces the snake_case strings the spec
    // and the audit log use as the canonical representation.
    #[test]
    fn tool_category_display() {
        assert_eq!(ToolCategory::SpawnAgents.to_string(), "spawn_agents");
        assert_eq!(ToolCategory::MessagePeers.to_string(), "message_peers");
        assert_eq!(ToolCategory::ReadFiles.to_string(), "read_files");
    }
}
