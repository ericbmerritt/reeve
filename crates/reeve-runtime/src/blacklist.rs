//! Blacklist registry: the operator's deterministic refusal floor.
//!
//! A [`BlacklistRegistry`] loads `<data_dir>/blacklist.toml`, parses its
//! entries, and vends a SHA-256 content hash that is recorded in every
//! `authority.decision` audit entry so historical decisions are
//! reconstructable.
//!
//! **Reload semantics.** The registry is reloaded on every call to
//! [`BlacklistRegistry::load_from_path`]. Callers that need live reloads
//! (the daemon) call this on every poll cycle; the registry is an immutable
//! value type and swapping it out is atomic from the caller's perspective.
//!
//! **Fail-closed.** If parsing fails the caller keeps the last-good
//! registry rather than replacing it with an empty one. The error is
//! returned so the caller can emit a `blacklist.reload_failed` audit event.
//!
//! **Pattern format.** Entries use a `Tool(specifier)` string that matches
//! the `canonical_action()` output produced by each tool actor:
//!
//! - `SpawnAgent(persona=worker)` — exact match on persona name
//! - `SendMessage(to=worker)` — exact match on recipient role name
//!
//! Match semantics in phase 2 are exact equality only. Future tool kinds
//! declare their own semantics when they register.

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ── On-disk format ────────────────────────────────────────────────────────────

/// Wire representation of `blacklist.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlacklistFile {
    schema_version: u32,
    #[serde(default)]
    entry: Vec<BlacklistEntry>,
}

/// One entry in the blacklist file.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BlacklistEntry {
    pattern: String,
    rationale: String,
}

const MAX_SCHEMA_VERSION: u32 = 1;

// ── Parse error ───────────────────────────────────────────────────────────────

/// Errors that can occur when loading or parsing a blacklist file.
#[derive(Debug)]
pub enum BlacklistError {
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: std::path::PathBuf,
        source: toml::de::Error,
    },
    UnsupportedVersion {
        found: u32,
        max: u32,
    },
    EmptyRationale {
        pattern: String,
    },
}

impl std::fmt::Display for BlacklistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "blacklist I/O at {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(f, "blacklist parse at {}: {source}", path.display())
            }
            Self::UnsupportedVersion { found, max } => write!(
                f,
                "blacklist schema_version {found} is newer than supported ({max})"
            ),
            Self::EmptyRationale { pattern } => {
                write!(f, "blacklist entry '{pattern}' has an empty rationale")
            }
        }
    }
}

impl std::error::Error for BlacklistError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::UnsupportedVersion { .. } | Self::EmptyRationale { .. } => None,
        }
    }
}

// ── BlacklistRegistry ─────────────────────────────────────────────────────────

/// An immutable snapshot of the blacklist state.
///
/// Contains the parsed entries and the SHA-256 content hash of the
/// canonical-serialized file contents (the `blacklist_version` value that
/// flows into every `authority.decision` audit entry, allowing historical
/// decisions to be reconstructed against the blacklist that was active at
/// the time).
#[derive(Debug, Clone)]
pub struct BlacklistRegistry {
    entries: Vec<(String, String)>, // (pattern, rationale)
    /// Hex-encoded SHA-256 of the canonical TOML serialization of all
    /// entries, ordered by their position in the file. Two identical files
    /// always produce the same hash; a single-character change produces a
    /// different hash.
    pub version_hash: String,
}

impl BlacklistRegistry {
    /// An empty registry with a deterministic "empty" hash.
    ///
    /// Used as the startup state when no `blacklist.toml` exists. The hash
    /// is the SHA-256 of an empty byte string, making it stable and
    /// distinguishable from any real blacklist.
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
            version_hash: hex_sha256(b""),
        }
    }

    /// Load and parse `path` into a registry.
    ///
    /// Returns `Err` if the file cannot be read, cannot be parsed, has an
    /// unsupported schema version, or has any entry with an empty rationale.
    /// The caller decides whether to fall back to the last-good registry.
    pub fn load_from_path(path: &Path) -> Result<Self, BlacklistError> {
        let body = crate::fs_util::read_nofollow_bounded(path, 64 * 1024).map_err(|source| {
            BlacklistError::Io {
                path: path.to_path_buf(),
                source,
            }
        })?;
        Self::parse_toml(&body, path)
    }

    /// Parse a TOML string into a registry. `path` is used only for error context.
    fn parse_toml(body: &str, path: &Path) -> Result<Self, BlacklistError> {
        let file: BlacklistFile = toml::from_str(body).map_err(|source| BlacklistError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if file.schema_version > MAX_SCHEMA_VERSION {
            return Err(BlacklistError::UnsupportedVersion {
                found: file.schema_version,
                max: MAX_SCHEMA_VERSION,
            });
        }
        let mut entries = Vec::with_capacity(file.entry.len());
        for e in &file.entry {
            if e.rationale.trim().is_empty() {
                return Err(BlacklistError::EmptyRationale {
                    pattern: e.pattern.clone(),
                });
            }
            entries.push((e.pattern.clone(), e.rationale.clone()));
        }

        // Hash the canonical representation: entries serialized back to TOML
        // in file order. Using the original body for hashing would mean
        // whitespace-only changes bump the version; using canonical
        // serialization means only semantic changes do.
        let canonical = canonical_bytes(&file.entry);
        let version_hash = hex_sha256(&canonical);

        Ok(Self {
            entries,
            version_hash,
        })
    }

    /// Check whether `action` matches any blacklist entry.
    ///
    /// Returns the matched `(pattern, rationale)` on the first match;
    /// later entries are not checked. Returns `None` if no entry matches.
    ///
    /// Phase 2 uses exact-equality matching. Future phases introduce
    /// glob and prefix semantics per tool type.
    pub fn check(&self, action: &str) -> Option<(&str, &str)> {
        for (pattern, rationale) in &self.entries {
            if pattern == action {
                return Some((pattern, rationale));
            }
        }
        None
    }

    /// Number of entries in this registry.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when the registry has no entries (startup state or empty file).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Serialize entries to a canonical byte string for hashing.
/// Uses TOML so the format is stable and human-readable in error messages.
fn canonical_bytes(entries: &[BlacklistEntry]) -> Vec<u8> {
    if entries.is_empty() {
        return Vec::new();
    }
    // Serialize just the entries array; omit schema_version from the hash so
    // a version-only change doesn't invalidate audit history.
    let mut out = String::new();
    for e in entries {
        use std::fmt::Write as _;
        let _ = write!(
            out,
            "[[entry]]\npattern = {}\nrationale = {}\n",
            toml::Value::String(e.pattern.clone()),
            toml::Value::String(e.rationale.clone()),
        );
    }
    out.into_bytes()
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_TOML: &str = r#"
schema_version = 1

[[entry]]
pattern = "SendMessage(to=worker)"
rationale = "direct messages to worker are not allowed in this context"

[[entry]]
pattern = "SpawnAgent(persona=untrusted)"
rationale = "untrusted persona is not approved for spawning"
"#;

    // BL1: valid file parses correctly.
    #[test]
    fn valid_file_parses() {
        let r = BlacklistRegistry::parse_toml(VALID_TOML, Path::new("test.toml")).unwrap();
        assert_eq!(r.len(), 2);
        assert!(!r.version_hash.is_empty());
    }

    // BL2: exact match returns the entry.
    #[test]
    fn check_exact_match() {
        let r = BlacklistRegistry::parse_toml(VALID_TOML, Path::new("test.toml")).unwrap();
        let hit = r.check("SendMessage(to=worker)");
        assert!(hit.is_some());
        let (pattern, rationale) = hit.unwrap();
        assert_eq!(pattern, "SendMessage(to=worker)");
        assert!(!rationale.is_empty());
    }

    // BL3: non-matching action returns None.
    #[test]
    fn check_no_match() {
        let r = BlacklistRegistry::parse_toml(VALID_TOML, Path::new("test.toml")).unwrap();
        assert!(r.check("SendMessage(to=lead)").is_none());
        assert!(r.check("SpawnAgent(persona=worker)").is_none());
        assert!(r.check("").is_none());
    }

    // BL4: unknown field rejected.
    #[test]
    fn unknown_field_rejected() {
        let bad =
            "schema_version = 1\n[[entry]]\npattern = \"x\"\nrationale = \"y\"\nsurprise = 1\n";
        assert!(BlacklistRegistry::parse_toml(bad, Path::new("t")).is_err());
    }

    // BL5: empty rationale is rejected.
    #[test]
    fn empty_rationale_rejected() {
        let bad = "schema_version = 1\n[[entry]]\npattern = \"x\"\nrationale = \"   \"\n";
        let err = BlacklistRegistry::parse_toml(bad, Path::new("t")).unwrap_err();
        assert!(matches!(err, BlacklistError::EmptyRationale { .. }));
    }

    // BL6: future schema version rejected.
    #[test]
    fn future_schema_rejected() {
        let bad = "schema_version = 999\n";
        let err = BlacklistRegistry::parse_toml(bad, Path::new("t")).unwrap_err();
        assert!(matches!(
            err,
            BlacklistError::UnsupportedVersion { found: 999, .. }
        ));
    }

    // BL7: empty file (no entries) is valid.
    #[test]
    fn empty_file_valid() {
        let r = BlacklistRegistry::parse_toml("schema_version = 1\n", Path::new("t")).unwrap();
        assert!(r.is_empty());
    }

    // BL8: version hash is stable for identical content.
    #[test]
    fn version_hash_stable() {
        let r1 = BlacklistRegistry::parse_toml(VALID_TOML, Path::new("t")).unwrap();
        let r2 = BlacklistRegistry::parse_toml(VALID_TOML, Path::new("t")).unwrap();
        assert_eq!(r1.version_hash, r2.version_hash);
    }

    // BL9: version hash changes when an entry changes.
    #[test]
    fn version_hash_changes_on_edit() {
        let other = "schema_version = 1\n[[entry]]\npattern = \"X\"\nrationale = \"y\"\n";
        let r1 = BlacklistRegistry::parse_toml(VALID_TOML, Path::new("t")).unwrap();
        let r2 = BlacklistRegistry::parse_toml(other, Path::new("t")).unwrap();
        assert_ne!(r1.version_hash, r2.version_hash);
    }

    // BL10: empty() has a stable deterministic hash distinct from any entry.
    #[test]
    fn empty_registry_has_deterministic_hash() {
        let e1 = BlacklistRegistry::empty();
        let e2 = BlacklistRegistry::empty();
        assert_eq!(e1.version_hash, e2.version_hash);
        let nonempty = BlacklistRegistry::parse_toml(VALID_TOML, Path::new("t")).unwrap();
        assert_ne!(e1.version_hash, nonempty.version_hash);
    }

    // BL11: round-trip write+load preserves entries and hash.
    #[test]
    fn round_trip_write_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("blacklist.toml");
        std::fs::write(&path, VALID_TOML).unwrap();
        let loaded = BlacklistRegistry::load_from_path(&path).unwrap();
        let direct = BlacklistRegistry::parse_toml(VALID_TOML, &path).unwrap();
        assert_eq!(loaded.version_hash, direct.version_hash);
        assert_eq!(loaded.len(), direct.len());
    }
}
