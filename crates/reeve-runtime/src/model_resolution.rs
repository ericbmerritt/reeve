//! Model resolution and spawn snapshot for agent lifecycle.
//!
//! Walks a persona's `model_preferences` list against a slice of registered
//! adapters to find the first match, then records the resolved (adapter,
//! model) pair as a [`SpawnSnapshot`] written to `agents/{name}/agent.toml`.
//!
//! The snapshot is immutable for the agent's lifetime. Enforcement of
//! capability profiles is deferred to `reeve-authority`.

use std::fmt;
use std::path::PathBuf;

use crate::agent_fs::AgentDirs;
use crate::config::PersonaConfig;

// ── SpawnSnapshot ─────────────────────────────────────────────────────────────

/// Configuration snapshot recorded at agent spawn time.
///
/// Immutable for the agent's lifetime; stored at `agents/{name}/agent.toml`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SpawnSnapshot {
    pub persona_name: String,
    /// `1` until `reeve-authority` introduces versioned artifacts.
    pub persona_version: u32,
    pub capability_profile: Option<String>,
    /// Resolved adapter identifier (e.g., `"claude-opus-4-7@anthropic-direct"`).
    pub adapter_id: String,
    // model is derived from adapter_id — not stored separately
    /// The transient [`reeve_types::IdentityId`] generated for this boot session.
    ///
    /// Written by the daemon at spawn time so that external senders (e.g. the
    /// TUI) can address signed envelopes to the correct watcher slot.
    pub agent_id: String,
}

impl SpawnSnapshot {
    /// The model segment of the resolved adapter ID (before the `@`).
    ///
    /// Returns an empty string if `adapter_id` is malformed — this should
    /// never happen since `resolve_model` constructs `adapter_id` from a
    /// validated adapter.
    pub fn model(&self) -> &str {
        parse_adapter_model(&self.adapter_id).unwrap_or("")
    }

    /// Parse `agent_id` as a `UUIDv7` and return a typed [`reeve_types::IdentityId`].
    ///
    /// Returns `None` when the stored string is absent or malformed — callers
    /// should treat this as a stale snapshot written before this field existed.
    pub fn agent_identity_id(&self) -> Option<reeve_types::IdentityId> {
        let uuid: uuid::Uuid = self.agent_id.parse().ok()?;
        reeve_types::IdentityId::try_from(uuid).ok()
    }
}

// ── ModelResolveError ────────────────────────────────────────────────────────

/// Errors produced by model resolution and snapshot write operations.
#[derive(Debug)]
pub enum ModelResolveError {
    /// No registered adapter serves any of the persona's preferred models.
    NoMatchingAdapter {
        /// Name of the persona that could not be resolved.
        persona: String,
        /// The full preference list that was exhausted without a match.
        preferences: Vec<String>,
    },
    /// Snapshot could not be written to disk.
    Io {
        /// Path of the target file.
        path: PathBuf,
        /// Underlying OS error.
        source: std::io::Error,
    },
    /// Snapshot could not be serialized to TOML.
    Serialize(toml::ser::Error),
}

impl fmt::Display for ModelResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoMatchingAdapter {
                persona,
                preferences,
            } => write!(
                f,
                "no adapter matches any model preference for persona '{persona}': {preferences:?}",
            ),
            Self::Io { path, source } => {
                write!(f, "snapshot IO at {}: {source}", path.display())
            }
            Self::Serialize(source) => write!(f, "snapshot serialization error: {source}"),
        }
    }
}

impl std::error::Error for ModelResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoMatchingAdapter { .. } => None,
            Self::Io { source, .. } => Some(source),
            Self::Serialize(source) => Some(source),
        }
    }
}

// ── parse_adapter_model ───────────────────────────────────────────────────────

/// Parse the model segment from an adapter ID of the form `"{model}@{route}"`.
///
/// Returns `None` for IDs that do not contain exactly one `@`, or where
/// either the model or route segment is empty.
fn parse_adapter_model(adapter_id: &str) -> Option<&str> {
    let mut parts = adapter_id.splitn(2, '@');
    let model = parts.next().filter(|s| !s.is_empty())?;
    parts.next().filter(|s| !s.is_empty())?;
    Some(model)
}

// ── resolve_model ─────────────────────────────────────────────────────────────

/// Resolve the model adapter for a persona.
///
/// Walks `persona.model_preferences` in order. For each model ID, searches
/// `adapters` for an adapter whose id has the form `{model_id}@{route}`.
/// The first match wins. Returns [`ModelResolveError::NoMatchingAdapter`] if
/// the preference list is exhausted with no match.
///
/// Capability profile fields are copied to the snapshot but not enforced;
/// enforcement is handled by `reeve-authority`.
pub fn resolve_model(
    persona: &PersonaConfig,
    adapters: &[&dyn reeve_adapter::Adapter],
    agent_id: reeve_types::IdentityId,
) -> Result<SpawnSnapshot, ModelResolveError> {
    for model_id in &persona.model_preferences {
        if let Some(adapter) = adapters
            .iter()
            .find(|a| parse_adapter_model(a.id()) == Some(model_id.as_str()))
        {
            return Ok(SpawnSnapshot {
                persona_name: persona.name.clone(),
                persona_version: 1,
                capability_profile: persona.capability_profile.clone(),
                adapter_id: adapter.id().to_owned(),
                agent_id: agent_id.to_string(),
            });
        }
    }

    Err(ModelResolveError::NoMatchingAdapter {
        persona: persona.name.clone(),
        preferences: persona.model_preferences.clone(),
    })
}

// ── write_spawn_snapshot ─────────────────────────────────────────────────────

/// Write `snapshot` to `dirs.agent_toml_path()` atomically.
///
/// Uses a temp-file → fsync → rename pattern so a crash mid-write leaves the
/// target either at its previous value or at the new value — never partial.
pub fn write_spawn_snapshot(
    dirs: &AgentDirs,
    snapshot: &SpawnSnapshot,
) -> Result<(), ModelResolveError> {
    let path = dirs.agent_toml_path();
    let content = toml::to_string(snapshot).map_err(ModelResolveError::Serialize)?;
    let parent = path
        .parent()
        .ok_or_else(|| ModelResolveError::Io {
            path: path.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "agent.toml path has no parent directory",
            ),
        })?
        .to_path_buf();
    crate::fs_util::atomic_write_file(&path, &parent, content.as_bytes(), 0o600)
        .map_err(|source| ModelResolveError::Io { path, source })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::tempdir;

    use crate::test_support::MockAdapter;

    fn minimal_persona(name: &str, prefs: Vec<String>) -> PersonaConfig {
        PersonaConfig {
            name: name.to_owned(),
            system_prompt: String::from("Be helpful."),
            model_preferences: prefs,
            capability_profile: None,
            display_name: None,
        }
    }

    // M1: resolve_model returns Ok with correct adapter_id and model when a
    // matching adapter is present.
    #[test]
    fn resolve_model_finds_matching_adapter() {
        let adapter = MockAdapter::new("claude-opus-4-7@anthropic-direct");
        let persona = minimal_persona("lead", vec![String::from("claude-opus-4-7")]);
        let adapters: &[&dyn reeve_adapter::Adapter] = &[&adapter];
        let agent_id = reeve_types::IdentityId::new().unwrap();

        let snapshot = resolve_model(&persona, adapters, agent_id).expect("should resolve");
        assert_eq!(snapshot.adapter_id, "claude-opus-4-7@anthropic-direct");
        assert_eq!(snapshot.model(), "claude-opus-4-7");
        assert_eq!(snapshot.persona_name, "lead");
        assert_eq!(snapshot.persona_version, 1);
        assert!(snapshot.capability_profile.is_none());
        assert_eq!(snapshot.agent_id, agent_id.to_string());
    }

    // M2: resolve_model returns NoMatchingAdapter when no adapter matches any
    // preference.
    #[test]
    fn resolve_model_returns_error_when_no_adapter_matches() {
        let persona = minimal_persona("solo", vec![String::from("nonexistent-model")]);
        let adapters: &[&dyn reeve_adapter::Adapter] = &[];
        let agent_id = reeve_types::IdentityId::new().unwrap();

        let err = resolve_model(&persona, adapters, agent_id).expect_err("should fail");
        assert!(
            matches!(err, ModelResolveError::NoMatchingAdapter { ref persona, .. } if persona == "solo"),
            "unexpected error variant: {err}",
        );
        assert!(err.to_string().contains("nonexistent-model"));
    }

    // M3: resolve_model picks the first matching preference when multiple
    // adapters are present.
    #[test]
    fn resolve_model_picks_first_matching_preference() {
        let adapter_a = MockAdapter::new("model-a@route");
        let adapter_b = MockAdapter::new("model-b@route");
        let persona = minimal_persona(
            "multi",
            vec![String::from("model-a"), String::from("model-b")],
        );
        let adapters: &[&dyn reeve_adapter::Adapter] = &[&adapter_a, &adapter_b];
        let agent_id = reeve_types::IdentityId::new().unwrap();

        let snapshot = resolve_model(&persona, adapters, agent_id).expect("should resolve");
        assert_eq!(snapshot.adapter_id, "model-a@route");
        assert_eq!(snapshot.model(), "model-a");
    }

    // M4: write_spawn_snapshot writes a valid TOML file that round-trips back
    // to the original snapshot fields.
    #[test]
    fn write_spawn_snapshot_writes_valid_toml() {
        let tmp = tempdir().expect("tempdir");
        let dirs = AgentDirs::provision(tmp.path(), "lead").expect("provision");

        let snapshot = SpawnSnapshot {
            persona_name: String::from("lead"),
            persona_version: 1,
            capability_profile: Some(String::from("default")),
            adapter_id: String::from("claude-opus-4-7@anthropic-direct"),
            agent_id: String::from("01930000-0000-7000-8000-000000000001"),
        };

        write_spawn_snapshot(&dirs, &snapshot).expect("write");

        let path = dirs.agent_toml_path();
        let content = std::fs::read_to_string(&path).expect("read back");
        let parsed: SpawnSnapshot = toml::from_str(&content).expect("parse toml");

        assert_eq!(parsed.persona_name, snapshot.persona_name);
        assert_eq!(parsed.persona_version, snapshot.persona_version);
        assert_eq!(parsed.capability_profile, snapshot.capability_profile);
        assert_eq!(parsed.adapter_id, snapshot.adapter_id);
        assert_eq!(parsed.agent_id, snapshot.agent_id);
        assert_eq!(parsed.model(), snapshot.model());
    }

    // M5: SpawnSnapshot serializes to TOML and deserializes back without loss.
    #[test]
    fn spawn_snapshot_round_trips_toml() {
        let original = SpawnSnapshot {
            persona_name: String::from("analyst"),
            persona_version: 1,
            capability_profile: None,
            adapter_id: String::from("claude-opus-4-7@anthropic-direct"),
            agent_id: String::from("01930000-0000-7000-8000-000000000002"),
        };

        let serialized = toml::to_string(&original).expect("serialize");
        let deserialized: SpawnSnapshot = toml::from_str(&serialized).expect("deserialize");

        assert_eq!(deserialized.persona_name, original.persona_name);
        assert_eq!(deserialized.persona_version, original.persona_version);
        assert_eq!(deserialized.capability_profile, original.capability_profile);
        assert_eq!(deserialized.adapter_id, original.adapter_id);
        assert_eq!(deserialized.agent_id, original.agent_id);
        assert_eq!(deserialized.model(), original.model());
    }

    // M6: resolve_model includes capability_profile from persona in snapshot.
    #[test]
    fn resolve_model_propagates_capability_profile() {
        let adapter = MockAdapter::new("claude-opus-4-7@anthropic-direct");
        let persona = PersonaConfig {
            name: String::from("lead"),
            system_prompt: String::from("Be helpful."),
            model_preferences: vec![String::from("claude-opus-4-7")],
            capability_profile: Some(String::from("default")),
            display_name: None,
        };
        let adapters: &[&dyn reeve_adapter::Adapter] = &[&adapter];
        let agent_id = reeve_types::IdentityId::new().unwrap();

        let snapshot = resolve_model(&persona, adapters, agent_id).expect("should resolve");
        assert_eq!(snapshot.capability_profile.as_deref(), Some("default"));
    }

    // M7: ModelResolveError Display impls are non-empty and informative.
    #[test]
    fn model_resolve_error_display_is_informative() {
        let no_match = ModelResolveError::NoMatchingAdapter {
            persona: String::from("test"),
            preferences: vec![String::from("model-x")],
        };
        let rendered = no_match.to_string();
        assert!(!rendered.is_empty());
        assert!(rendered.contains("test"), "missing persona: {rendered}");
        assert!(rendered.contains("model-x"), "missing pref: {rendered}");

        let io_err = ModelResolveError::Io {
            path: PathBuf::from("agents/lead/agent.toml"),
            source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        };
        let rendered = io_err.to_string();
        assert!(!rendered.is_empty());
        assert!(rendered.contains("agent.toml"), "missing path: {rendered}");
    }

    // M8: parse_adapter_model correctly handles valid and invalid adapter IDs.
    #[test]
    fn parse_adapter_model_handles_edge_cases() {
        // Valid
        assert_eq!(
            parse_adapter_model("claude-opus-4-7@anthropic-direct"),
            Some("claude-opus-4-7")
        );
        // Empty route: invalid
        assert_eq!(parse_adapter_model("claude-opus-4-7@"), None);
        // Empty model: invalid
        assert_eq!(parse_adapter_model("@anthropic-direct"), None);
        // No @: invalid
        assert_eq!(parse_adapter_model("claude-opus-4-7"), None);
        // Multiple @: splitn(2) — second part includes the extra @, treated as route
        assert_eq!(
            parse_adapter_model("model@route@extra"),
            Some("model") // route = "route@extra", non-empty
        );
    }

    // M9: An adapter with a malformed ID (no @) never matches any preference.
    #[test]
    fn resolve_model_ignores_malformed_adapter_ids() {
        let malformed = MockAdapter::new("claude-opus-4-7-no-at"); // no @ character
        let persona = minimal_persona("solo", vec![String::from("claude-opus-4-7")]);
        let adapters: &[&dyn reeve_adapter::Adapter] = &[&malformed];
        let agent_id = reeve_types::IdentityId::new().unwrap();
        let err = resolve_model(&persona, adapters, agent_id).unwrap_err();
        assert!(matches!(err, ModelResolveError::NoMatchingAdapter { .. }));
    }

    // M10: resolve_model falls through to the second preference when the first
    // has no matching adapter.
    #[test]
    fn resolve_model_falls_through_to_second_preference() {
        let adapter_b = MockAdapter::new("model-b@test-route");
        let persona = minimal_persona(
            "test",
            vec![String::from("model-a"), String::from("model-b")],
        );
        let adapters: &[&dyn reeve_adapter::Adapter] = &[&adapter_b];
        let agent_id = reeve_types::IdentityId::new().unwrap();
        let snapshot = resolve_model(&persona, adapters, agent_id).unwrap();
        assert_eq!(snapshot.adapter_id, "model-b@test-route");
        assert_eq!(snapshot.model(), "model-b");
    }
}
