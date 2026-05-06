//! Persona and team configuration loaders for phase 7.
//!
//! Loads [`PersonaConfig`] and [`TeamConfig`] from TOML files on disk.
//! Provides [`install_defaults`] which writes the lead-persona and
//! default-team configs the first time the runtime starts, without
//! overwriting anything the operator has already placed there.
//!
//! Filesystem safety follows `specs/reeve-transport-security.md` §
//! Filesystem Safety: every file read uses `O_NOFOLLOW`, reads are
//! bounded at 64 KiB to guard against OOM, and writes are atomic
//! (temp-file → fsync → rename). Parent directories are created with
//! mode `0o700`; config files are created with mode `0o600`.

use std::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::fs_util::{ensure_directory, sync_directory};

// ── File-mode constants ───────────────────────────────────────────────────────

/// Mode for config subdirectories (`personas/`, `teams/`, and their children).
///
/// Restrictive: operator-owned, not world-readable.
const CONFIG_DIR_MODE: u32 = 0o700;

/// Mode for individual config TOML files.
///
/// Restrictive: operator-owned, not world-readable.
const CONFIG_FILE_MODE: u32 = 0o600;

// ── Default config content ────────────────────────────────────────────────────

/// Default persona TOML written to `{data_dir}/personas/lead/config.toml` if absent.
const DEFAULT_PERSONA_TOML: &str = r#"name = "lead"
system_prompt = "You are a helpful AI assistant."
model_preferences = ["claude-opus-4-7"]
capability_profile = "default"
display_name = "Lead"
"#;

/// Default team TOML written to `{data_dir}/teams/default.toml` if absent.
const DEFAULT_TEAM_TOML: &str = r#"name = "default"
version = 1
lead_role = "lead"

[[members]]
persona_name = "lead"
persona_version = 1
count = 1
role_label = "lead"
"#;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors produced by config load and install operations.
///
/// Not `Clone` or `PartialEq` because [`io::Error`] is neither.
/// `Display` and `Error::source` are implemented directly; no
/// `thiserror` dependency is needed.
#[derive(Debug)]
pub enum ConfigError {
    /// Underlying filesystem error (open, read, write, mkdir, rename).
    Io { path: PathBuf, source: io::Error },
    /// A config file could not be parsed as valid TOML.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// A config file was parsed successfully but violates a domain invariant.
    Validation { path: PathBuf, message: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "config IO at {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(f, "config parse at {}: {source}", path.display())
            }
            Self::Validation { path, message } => {
                write!(
                    f,
                    "config validation failed at {}: {message}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::Validation { .. } => None,
        }
    }
}

// ── Public types ──────────────────────────────────────────────────────────────

/// Persona configuration loaded from TOML.
///
/// Walking-skeleton scope: only the fields needed for phase 7 are parsed. The
/// spec allows many more fields (skill names, capability profile, etc.); they
/// are declared as optional here to allow forward compatibility without
/// breaking existing configs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PersonaConfig {
    /// Unique persona name within this Reeve installation.
    pub name: String,
    /// System prompt materialized into agents spawned from this persona.
    pub system_prompt: String,
    /// Ordered list of preferred model IDs (e.g., `["claude-opus-4-7"]`).
    pub model_preferences: Vec<String>,
    /// Name of the capability profile to reference (parsed but not enforced
    /// in this ladder).
    pub capability_profile: Option<String>,
    /// Display name override; falls back to `name` if absent.
    pub display_name: Option<String>,
}

/// A single team member entry in a team config.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TeamMember {
    /// Name of the persona to instantiate for this member.
    pub persona_name: String,
    /// Persona version; `1` for the walking skeleton default.
    pub persona_version: u32,
    /// Number of instances to spawn (1 for the lead).
    pub count: u32,
    /// Role label used to identify this member (e.g., `"lead"`).
    pub role_label: String,
}

/// Team configuration loaded from TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TeamConfig {
    /// Unique team name within this Reeve installation.
    pub name: String,
    /// Schema version; `1` for the walking skeleton default.
    pub version: u32,
    /// Role label of the team member the TUI attaches to by default.
    pub lead_role: String,
    /// Ordered list of persona instantiations that make up this team.
    pub members: Vec<TeamMember>,
}

// ── Public loaders ────────────────────────────────────────────────────────────

/// Load a [`PersonaConfig`] from `path`.
///
/// # Errors
///
/// Any open failure — including `ENOENT` — maps to [`ConfigError::Io`].
/// Parse failure maps to [`ConfigError::Parse`]. Empty `model_preferences`
/// maps to [`ConfigError::Validation`].
pub fn load_persona_config(path: &Path) -> Result<PersonaConfig, ConfigError> {
    let body = crate::fs_util::read_nofollow_bounded(path, 64 * 1024).map_err(|source| {
        ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let config: PersonaConfig = toml::from_str(&body).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    if config.model_preferences.is_empty() {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: String::from("model_preferences must not be empty"),
        });
    }
    if config.name.trim().is_empty() {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: String::from("name must not be empty"),
        });
    }
    Ok(config)
}

/// Load a [`TeamConfig`] from `path`.
///
/// # Errors
///
/// Any open failure — including `ENOENT` — maps to [`ConfigError::Io`].
/// Parse failure maps to [`ConfigError::Parse`]. Empty `members` or a
/// `lead_role` that matches no member maps to [`ConfigError::Validation`].
pub fn load_team_config(path: &Path) -> Result<TeamConfig, ConfigError> {
    let body = crate::fs_util::read_nofollow_bounded(path, 64 * 1024).map_err(|source| {
        ConfigError::Io {
            path: path.to_path_buf(),
            source,
        }
    })?;
    let config: TeamConfig = toml::from_str(&body).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    if config.members.is_empty() {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: String::from("members must not be empty"),
        });
    }
    if !config
        .members
        .iter()
        .any(|m| m.role_label == config.lead_role)
    {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: format!(
                "lead_role '{}' does not match any member role_label",
                config.lead_role
            ),
        });
    }
    if config.name.trim().is_empty() {
        return Err(ConfigError::Validation {
            path: path.to_path_buf(),
            message: String::from("name must not be empty"),
        });
    }
    Ok(config)
}

// ── Default installation ──────────────────────────────────────────────────────

/// Install the default lead persona and default team configs if they do not
/// already exist.
///
/// Paths written:
/// - `{data_dir}/personas/lead/config.toml`
/// - `{data_dir}/teams/default.toml`
///
/// Idempotent: existing files are never overwritten. Parent directories are
/// created with mode `0o700` if absent; files are created with mode `0o600`.
pub fn install_defaults(data_dir: &Path) -> Result<(), ConfigError> {
    let persona_path = data_dir.join("personas").join("lead").join("config.toml");
    let team_path = data_dir.join("teams").join("default.toml");

    write_if_absent(&persona_path, DEFAULT_PERSONA_TOML)?;
    write_if_absent(&team_path, DEFAULT_TEAM_TOML)?;

    Ok(())
}

// ── Crate-private helpers ─────────────────────────────────────────────────────

/// Write `content` to `path` only if the file does not already exist.
///
/// If `path` exists (as any file type, including a symlink), returns
/// immediately: a regular file is left untouched; a symlink causes
/// `ConfigError::Io` so the caller surfaces a clear error rather than
/// silently failing later when `O_NOFOLLOW` reads reject it. If absent,
/// creates the parent directory (and any missing ancestors) with mode
/// `0o700`, then writes atomically: temp file → fsync → rename, so a crash
/// mid-write never leaves a partial file at `path`.
pub(crate) fn write_if_absent(path: &Path, content: &str) -> Result<(), ConfigError> {
    // Check existence first so that a pre-existing file never causes a
    // spurious directory-mode error on the parent.
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source: io::Error::new(
                        io::ErrorKind::InvalidData,
                        "config path is a symlink; cannot install default",
                    ),
                });
            }
            return Ok(());
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    }

    let parent = parent_of(path)?;
    ensure_config_dir(parent)?;
    atomic_write_str(parent, path, content)
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Extract the parent directory of `path`, returning `ConfigError::Io` if
/// `path` has no parent (i.e., it is the filesystem root).
fn parent_of(path: &Path) -> Result<&Path, ConfigError> {
    path.parent().ok_or_else(|| ConfigError::Io {
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "config path has no parent directory",
        ),
    })
}

/// Ensure `dir` exists with mode `0o700`, mapping [`FsCheckError`] to
/// [`ConfigError::Io`].
fn ensure_config_dir(dir: &Path) -> Result<(), ConfigError> {
    ensure_directory(dir, CONFIG_DIR_MODE).map_err(|e| {
        use crate::fs_util::FsCheckError;
        match e {
            FsCheckError::Io { path, source } => ConfigError::Io { path, source },
            FsCheckError::Symlink { path } => ConfigError::Io {
                path,
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "config directory is a symlink; runtime refuses to follow it",
                ),
            },
            FsCheckError::NotADirectory { path } => ConfigError::Io {
                path,
                source: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "config path exists but is not a directory",
                ),
            },
            FsCheckError::WrongMode {
                path,
                actual,
                expected,
            } => ConfigError::Io {
                path,
                source: io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("config directory has mode 0o{actual:o}, expected 0o{expected:o}"),
                ),
            },
        }
    })
}

/// Atomically write `content` to `target`, using `dir` as the temp directory.
///
/// Pattern: `NamedTempFile::new_in(dir)` → set mode → write → fsync → persist.
/// A crash at any point before persist leaves `target` unchanged.
fn atomic_write_str(dir: &Path, target: &Path, content: &str) -> Result<(), ConfigError> {
    let mut tmp = NamedTempFile::new_in(dir).map_err(|source| ConfigError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    crate::fs_util::apply_file_perms(tmp.as_file(), CONFIG_FILE_MODE).map_err(|source| {
        ConfigError::Io {
            path: tmp.path().to_path_buf(),
            source,
        }
    })?;

    tmp.write_all(content.as_bytes())
        .map_err(|source| ConfigError::Io {
            path: tmp.path().to_path_buf(),
            source,
        })?;

    tmp.as_file().sync_all().map_err(|source| ConfigError::Io {
        path: tmp.path().to_path_buf(),
        source,
    })?;

    tmp.persist(target).map_err(|err| ConfigError::Io {
        path: target.to_path_buf(),
        source: err.error,
    })?;

    sync_directory(dir);
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use std::fs;

    use tempfile::tempdir;

    // Helper: write a string to a path, creating parent dirs as needed.
    fn write_str(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    // C1: install_defaults creates both files at the expected paths.
    #[test]
    fn install_defaults_creates_files() {
        let tmp = tempdir().unwrap();
        install_defaults(tmp.path()).unwrap();

        let persona_path = tmp.path().join("personas").join("lead").join("config.toml");
        let team_path = tmp.path().join("teams").join("default.toml");

        assert!(persona_path.is_file(), "persona config missing");
        assert!(team_path.is_file(), "team config missing");

        let persona_content = fs::read_to_string(&persona_path).unwrap();
        assert_eq!(persona_content, DEFAULT_PERSONA_TOML);

        let team_content = fs::read_to_string(&team_path).unwrap();
        assert_eq!(team_content, DEFAULT_TEAM_TOML);
    }

    // C2: install_defaults is idempotent — second call returns Ok and does not
    // overwrite.
    #[test]
    fn install_defaults_is_idempotent() {
        let tmp = tempdir().unwrap();
        install_defaults(tmp.path()).unwrap();

        // Overwrite the persona file with sentinel content.
        let persona_path = tmp.path().join("personas").join("lead").join("config.toml");
        fs::write(&persona_path, "sentinel").unwrap();

        // Second call must not overwrite.
        install_defaults(tmp.path()).unwrap();

        let content = fs::read_to_string(&persona_path).unwrap();
        assert_eq!(
            content, "sentinel",
            "install_defaults overwrote existing file"
        );
    }

    // C3: load_persona_config round-trip — write a persona TOML, load it, assert
    // all fields match.
    #[test]
    fn load_persona_config_round_trip() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("persona.toml");
        write_str(
            &path,
            r#"
name = "analyst"
system_prompt = "You are a data analyst."
model_preferences = ["claude-opus-4-7", "claude-sonnet-3-5"]
capability_profile = "restricted"
display_name = "Analyst"
"#,
        );

        let cfg = load_persona_config(&path).unwrap();
        assert_eq!(cfg.name, "analyst");
        assert_eq!(cfg.system_prompt, "You are a data analyst.");
        assert_eq!(
            cfg.model_preferences,
            vec!["claude-opus-4-7", "claude-sonnet-3-5"]
        );
        assert_eq!(cfg.capability_profile.as_deref(), Some("restricted"));
        assert_eq!(cfg.display_name.as_deref(), Some("Analyst"));
    }

    // C4: load_team_config round-trip — write a team TOML, load it, assert all
    // fields match including members[0].role_label.
    #[test]
    fn load_team_config_round_trip() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("team.toml");
        write_str(
            &path,
            r#"
name = "core"
version = 2
lead_role = "architect"

[[members]]
persona_name = "architect"
persona_version = 1
count = 1
role_label = "architect"

[[members]]
persona_name = "reviewer"
persona_version = 1
count = 2
role_label = "reviewer"
"#,
        );

        let cfg = load_team_config(&path).unwrap();
        assert_eq!(cfg.name, "core");
        assert_eq!(cfg.version, 2);
        assert_eq!(cfg.lead_role, "architect");
        assert_eq!(cfg.members.len(), 2);
        assert_eq!(cfg.members[0].role_label, "architect");
        assert_eq!(cfg.members[0].persona_name, "architect");
        assert_eq!(cfg.members[1].count, 2);
        assert_eq!(cfg.members[1].role_label, "reviewer");
    }

    // C5: load_persona_config returns ConfigError::Io when the file is absent.
    #[test]
    fn load_persona_config_returns_error_on_missing() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("does_not_exist.toml");
        let err = load_persona_config(&path).unwrap_err();
        assert!(
            matches!(err, ConfigError::Io { .. }),
            "expected Io error for missing file, got {err:?}",
        );
    }

    // C6: load_persona_config returns ConfigError::Parse for malformed TOML.
    #[test]
    fn load_persona_config_returns_error_on_invalid_toml() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("bad.toml");
        write_str(&path, "this is not valid TOML ===");

        let err = load_persona_config(&path).unwrap_err();
        assert!(
            matches!(err, ConfigError::Parse { .. }),
            "expected Parse error for invalid TOML, got {err:?}",
        );
    }

    // D1: ConfigError::Display impls are non-empty and contain the path.
    #[test]
    fn config_error_display_impls() {
        let path = PathBuf::from("synthetic/test-path.toml");

        let io_err = ConfigError::Io {
            path: path.clone(),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        let rendered = io_err.to_string();
        assert!(!rendered.is_empty());
        assert!(
            rendered.contains("synthetic/test-path.toml"),
            "Io: {rendered}"
        );

        let toml_err = toml::from_str::<PersonaConfig>("not toml ===").unwrap_err();
        let parse_err = ConfigError::Parse {
            path: path.clone(),
            source: toml_err,
        };
        let rendered = parse_err.to_string();
        assert!(!rendered.is_empty());
        assert!(
            rendered.contains("synthetic/test-path.toml"),
            "Parse: {rendered}"
        );
    }

    // D2: load_persona_config handles optional fields absent from TOML.
    #[test]
    fn load_persona_config_optional_fields_absent() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("minimal.toml");
        write_str(
            &path,
            r#"
name = "minimal"
system_prompt = "Be helpful."
model_preferences = ["claude-opus-4-7"]
"#,
        );

        let cfg = load_persona_config(&path).unwrap();
        assert_eq!(cfg.name, "minimal");
        assert!(cfg.capability_profile.is_none());
        assert!(cfg.display_name.is_none());
    }

    // D3: write_if_absent does not overwrite when path already exists.
    #[test]
    fn write_if_absent_does_not_overwrite() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("existing.toml");
        write_str(&path, "original");
        write_if_absent(&path, "replacement").unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "original");
    }

    // D4: DEFAULT_PERSONA_TOML and DEFAULT_TEAM_TOML round-trip through their
    // respective parsers without error.
    #[test]
    fn default_configs_are_valid_toml() {
        let persona: PersonaConfig = toml::from_str(DEFAULT_PERSONA_TOML).unwrap();
        assert_eq!(persona.name, "lead");
        assert_eq!(persona.model_preferences, vec!["claude-opus-4-7"]);

        let team: TeamConfig = toml::from_str(DEFAULT_TEAM_TOML).unwrap();
        assert_eq!(team.name, "default");
        assert_eq!(team.lead_role, "lead");
        assert_eq!(team.members.len(), 1);
        assert_eq!(team.members[0].role_label, "lead");
    }

    // E/F: write_if_absent returns Err(ConfigError::Io) when a symlink exists
    // at the target path, because symlink_metadata detects the symlink and
    // rejects it rather than silently skipping with Ok(()).
    #[cfg(unix)]
    #[test]
    fn write_if_absent_rejects_symlink_target() {
        use std::os::unix::fs::symlink;
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("real_file.toml");
        let link = tmp.path().join("symlink.toml");

        // Create the target file with known content.
        fs::write(&target, "original content").unwrap();
        // Create a symlink at link pointing to target.
        symlink(&target, &link).unwrap();

        // write_if_absent must detect the symlink and return Err.
        let result = write_if_absent(&link, "new content");
        assert!(
            matches!(result, Err(ConfigError::Io { .. })),
            "expected Io error for symlink target, got {result:?}",
        );

        // The original file content must be unchanged (we did not write through).
        let content = fs::read_to_string(&target).unwrap();
        assert_eq!(content, "original content");
    }

    // I1: load_persona_config rejects empty model_preferences.
    #[test]
    fn load_persona_config_rejects_empty_model_preferences() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("empty_prefs.toml");
        fs::write(
            &path,
            "name = \"x\"\nsystem_prompt = \"y\"\nmodel_preferences = []\n",
        )
        .unwrap();
        let err = load_persona_config(&path).unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { .. }),
            "expected Validation, got {err:?}",
        );
    }

    // I2: load_team_config rejects empty members list.
    #[test]
    fn load_team_config_rejects_empty_members() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("empty_team.toml");
        // members = [] is required to produce a Validation error rather than a Parse
        // error ("missing field `members`").
        fs::write(
            &path,
            "name = \"t\"\nversion = 1\nlead_role = \"nobody\"\nmembers = []\n",
        )
        .unwrap();
        let err = load_team_config(&path).unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { .. }),
            "expected Validation, got {err:?}",
        );
    }

    // I4: load_persona_config rejects empty name.
    #[test]
    fn load_persona_config_rejects_empty_name() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("empty_name.toml");
        fs::write(
            &path,
            "name = \"\"\nsystem_prompt = \"y\"\nmodel_preferences = [\"claude-opus-4-7\"]\n",
        )
        .unwrap();
        let err = load_persona_config(&path).unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { .. }),
            "expected Validation, got {err:?}"
        );
    }

    // I5: load_team_config rejects empty name.
    #[test]
    fn load_team_config_rejects_empty_name() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("empty_name.toml");
        let content = "name = \"\"\nversion = 1\nlead_role = \"lead\"\n\n[[members]]\npersona_name = \"lead\"\npersona_version = 1\ncount = 1\nrole_label = \"lead\"\n";
        fs::write(&path, content).unwrap();
        let err = load_team_config(&path).unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { .. }),
            "expected Validation, got {err:?}"
        );
    }

    // I3: load_team_config rejects lead_role that matches no member.
    #[test]
    fn load_team_config_rejects_mismatched_lead_role() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("bad_lead.toml");
        let content = "name = \"t\"\nversion = 1\nlead_role = \"nonexistent\"\n\n\
            [[members]]\npersona_name = \"lead\"\npersona_version = 1\ncount = 1\n\
            role_label = \"lead\"\n";
        fs::write(&path, content).unwrap();
        let err = load_team_config(&path).unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { .. }),
            "expected Validation, got {err:?}",
        );
    }
}
