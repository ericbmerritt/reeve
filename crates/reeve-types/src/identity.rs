//! Identity types per `specs/reeve-domain-model.md` § Security Layer §
//! Identity.
//!
//! An identity is a participant in the Reeve runtime — operator, agent, or
//! external. Identities are durable; once retired, an identity ID is never
//! reused (domain-model invariant 1).

use std::fmt;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;

use crate::id_newtype::uuid_v7_newtype;

uuid_v7_newtype! {
    /// Stable, opaque identifier for an identity. `UUIDv7` for chronological
    /// sortability per domain-model § Identifiers.
    ///
    /// `IdentityId` always wraps a `UUIDv7`. Construct fresh IDs with
    /// [`IdentityId::new`]; convert wire-form UUIDs through
    /// [`IdentityId::try_from`], which rejects any other UUID version.
    pub IdentityId,
    /// Errors that can occur when minting or wrapping an [`IdentityId`].
    error IdentityIdError,
    noun "identity id",
}

/// The category of a participant. Determines trust tier and capability scope
/// when combined with a verified envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdentityType {
    /// A human user. Created via interactive enrollment.
    Operator,
    /// A running agent instance. Created at spawn by the runtime.
    Agent,
    /// A process outside the runtime. Created via operator-approved enrollment.
    External,
}

/// Lifecycle state of an [`Identity`].
///
/// An identity is exactly one of: active with no scheduled expiry, active
/// with a scheduled expiry, or revoked. "Active but expired" — i.e. the
/// scheduled expiry is in the past — is the same enum value as
/// [`IdentityLifecycle::ActiveUntil`]; whether the expiry has elapsed is a
/// runtime question against the wall clock, not a property of the record.
/// Combinations like "active with revoked timestamp" are not representable.
///
/// The on-disk shape is the flat pair `expires_at` / `revoked_at` defined in
/// `specs/reeve-domain-model.md` § Identity. Custom serde on [`Identity`]
/// reconciles the flat form with this sum type and rejects illegal flat
/// combinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityLifecycle {
    /// Active with no scheduled expiry.
    Active,
    /// Active with a scheduled expiry at this instant. Whether the expiry
    /// has already elapsed is a runtime check, not a record property.
    ActiveUntil { expires_at: OffsetDateTime },
    /// Revoked at this instant. Once revoked, stays revoked.
    Revoked { revoked_at: OffsetDateTime },
}

/// Errors that can occur when reconciling on-disk identity lifecycle fields
/// with the in-memory [`IdentityLifecycle`] sum type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityLifecycleError {
    /// Both `expires_at` and `revoked_at` are present. Revocation supersedes
    /// expiry; only one may appear at a time.
    ExpiryAndRevocation,
    /// `expires_at` is not strictly after `created_at`. The active-until
    /// window must be a real (non-empty, forward-running) interval.
    ExpiryNotAfterCreation {
        created_at: OffsetDateTime,
        expires_at: OffsetDateTime,
    },
    /// `revoked_at` is strictly before `created_at`. An identity cannot be
    /// revoked before it was created; equality is allowed.
    RevocationBeforeCreation {
        created_at: OffsetDateTime,
        revoked_at: OffsetDateTime,
    },
}

impl fmt::Display for IdentityLifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExpiryAndRevocation => {
                f.write_str("identity has both expires_at and revoked_at; only one may be set")
            }
            Self::ExpiryNotAfterCreation {
                created_at,
                expires_at,
            } => write!(
                f,
                "identity expiry is not after creation: expires_at ({expires_at}) must be strictly after created_at ({created_at})",
            ),
            Self::RevocationBeforeCreation {
                created_at,
                revoked_at,
            } => write!(
                f,
                "identity revocation precedes creation: revoked_at ({revoked_at}) is before created_at ({created_at})",
            ),
        }
    }
}

impl std::error::Error for IdentityLifecycleError {}

/// A participant in the Reeve runtime.
///
/// Mirrors domain-model § Security Layer § Identity. The lifecycle fields
/// `expires_at` and `revoked_at` are reconciled into a [`IdentityLifecycle`]
/// sum type at parse time so impossible flat combinations are rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub identity_id: IdentityId,
    pub identity_type: IdentityType,
    pub display_name: String,
    /// References another `identity_id` when the identity was minted by an
    /// existing one (e.g., the runtime creating an agent identity). `None`
    /// when self-created — the first operator on a fresh machine.
    pub created_by: Option<IdentityId>,
    pub created_at: OffsetDateTime,
    pub lifecycle: IdentityLifecycle,
    /// Agent names this identity may address. Meaningful primarily for
    /// `External`; empty for `Operator` and `Agent` defaults.
    ///
    /// Empty has the asymmetric meaning "no allowlist constraint at this
    /// layer" (the runtime applies its own policy). For `External` identities
    /// the allowlist is enforced by the gatekeeper layer; an empty list there
    /// effectively grants no targets — populate it deliberately at enrollment.
    pub allowed_targets: Vec<String>,
    /// Message kinds this identity may send. Empty means "no allowlist
    /// constraint at this layer", parallel to `allowed_targets`.
    pub allowed_message_kinds: Vec<String>,
    /// Free-form capability scope tag interpreted by the authority layer in
    /// a later phase; format TBD. Kept as `Option<String>` so the on-disk
    /// shape is forward-compatible.
    pub capability_scope: Option<String>,
}

impl Identity {
    /// Construct a fresh operator identity. Operators are self-created on a
    /// new machine, so `created_by` is `None`.
    pub fn new_operator(display_name: String) -> Result<Self, IdentityIdError> {
        Ok(Self {
            identity_id: IdentityId::new()?,
            identity_type: IdentityType::Operator,
            display_name,
            created_by: None,
            created_at: OffsetDateTime::now_utc(),
            lifecycle: IdentityLifecycle::Active,
            allowed_targets: Vec::new(),
            allowed_message_kinds: Vec::new(),
            capability_scope: None,
        })
    }

    /// Construct a fresh agent identity created by another identity (the
    /// runtime, acting under an operator).
    pub fn new_agent(
        display_name: String,
        created_by: IdentityId,
    ) -> Result<Self, IdentityIdError> {
        Ok(Self {
            identity_id: IdentityId::new()?,
            identity_type: IdentityType::Agent,
            display_name,
            created_by: Some(created_by),
            created_at: OffsetDateTime::now_utc(),
            lifecycle: IdentityLifecycle::Active,
            allowed_targets: Vec::new(),
            allowed_message_kinds: Vec::new(),
            capability_scope: None,
        })
    }

    /// Construct a fresh external identity, scoped to the agent names it is
    /// permitted to address.
    pub fn new_external(
        display_name: String,
        created_by: IdentityId,
        allowed_targets: Vec<String>,
    ) -> Result<Self, IdentityIdError> {
        Ok(Self {
            identity_id: IdentityId::new()?,
            identity_type: IdentityType::External,
            display_name,
            created_by: Some(created_by),
            created_at: OffsetDateTime::now_utc(),
            lifecycle: IdentityLifecycle::Active,
            allowed_targets,
            allowed_message_kinds: Vec::new(),
            capability_scope: None,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct IdentityFlat {
    identity_id: IdentityId,
    identity_type: IdentityType,
    display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    created_by: Option<IdentityId>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    expires_at: Option<OffsetDateTime>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    revoked_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allowed_targets: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    allowed_message_kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capability_scope: Option<String>,
}

impl Serialize for Identity {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let (expires_at, revoked_at) = match self.lifecycle {
            IdentityLifecycle::Active => (None, None),
            IdentityLifecycle::ActiveUntil { expires_at } => (Some(expires_at), None),
            IdentityLifecycle::Revoked { revoked_at } => (None, Some(revoked_at)),
        };
        let flat = IdentityFlat {
            identity_id: self.identity_id,
            identity_type: self.identity_type,
            display_name: self.display_name.clone(),
            created_by: self.created_by,
            created_at: self.created_at,
            expires_at,
            revoked_at,
            allowed_targets: self.allowed_targets.clone(),
            allowed_message_kinds: self.allowed_message_kinds.clone(),
            capability_scope: self.capability_scope.clone(),
        };
        flat.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Identity {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let flat = IdentityFlat::deserialize(deserializer)?;
        let lifecycle = match (flat.expires_at, flat.revoked_at) {
            (None, None) => IdentityLifecycle::Active,
            (Some(expires_at), None) => {
                if expires_at <= flat.created_at {
                    return Err(de::Error::custom(
                        IdentityLifecycleError::ExpiryNotAfterCreation {
                            created_at: flat.created_at,
                            expires_at,
                        },
                    ));
                }
                IdentityLifecycle::ActiveUntil { expires_at }
            }
            (None, Some(revoked_at)) => {
                if revoked_at < flat.created_at {
                    return Err(de::Error::custom(
                        IdentityLifecycleError::RevocationBeforeCreation {
                            created_at: flat.created_at,
                            revoked_at,
                        },
                    ));
                }
                IdentityLifecycle::Revoked { revoked_at }
            }
            (Some(_), Some(_)) => {
                return Err(de::Error::custom(
                    IdentityLifecycleError::ExpiryAndRevocation,
                ));
            }
        };
        Ok(Self {
            identity_id: flat.identity_id,
            identity_type: flat.identity_type,
            display_name: flat.display_name,
            created_by: flat.created_by,
            created_at: flat.created_at,
            lifecycle,
            allowed_targets: flat.allowed_targets,
            allowed_message_kinds: flat.allowed_message_kinds,
            capability_scope: flat.capability_scope,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use time::Duration;
    use uuid::Uuid;

    #[test]
    fn identity_id_is_uuid_v7() {
        let id = IdentityId::new().unwrap();
        assert_eq!(id.as_uuid().get_version_num(), 7);
    }

    #[test]
    fn identity_id_display_round_trips_through_uuid_string() {
        let id = IdentityId::new().unwrap();
        let rendered = id.to_string();
        let parsed: Uuid = rendered.parse().unwrap();
        assert_eq!(parsed, *id.as_uuid());
    }

    #[test]
    fn identity_id_serde_round_trip_uses_string_form() {
        let id = IdentityId::new().unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{}\"", id.as_uuid()));
        let decoded: IdentityId = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, id);
    }

    #[test]
    fn identity_id_try_from_rejects_v4_uuid() {
        let v4 = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let result = IdentityId::try_from(v4);
        assert_eq!(result, Err(IdentityIdError::NotV7 { actual_version: 4 }));
    }

    #[test]
    fn identity_id_deserialize_rejects_v4_uuid() {
        let json = "\"550e8400-e29b-41d4-a716-446655440000\"";
        let result: Result<IdentityId, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "expected v4 UUID to be rejected on deserialize"
        );
    }

    #[test]
    fn identity_id_try_from_accepts_v7_uuid() {
        let original = IdentityId::new().unwrap();
        let recovered = IdentityId::try_from(*original.as_uuid()).unwrap();
        assert_eq!(recovered, original);
    }

    #[test]
    fn new_operator_sets_expected_initial_values() {
        let before = OffsetDateTime::now_utc();
        let identity = Identity::new_operator("Ada".to_owned()).unwrap();
        let after = OffsetDateTime::now_utc();

        assert_eq!(identity.identity_type, IdentityType::Operator);
        assert_eq!(identity.display_name, "Ada");
        assert!(identity.created_by.is_none());
        assert_eq!(identity.lifecycle, IdentityLifecycle::Active);
        assert!(identity.allowed_targets.is_empty());
        assert!(identity.allowed_message_kinds.is_empty());
        assert!(identity.capability_scope.is_none());
        assert!(identity.created_at >= before - Duration::seconds(1));
        assert!(identity.created_at <= after + Duration::seconds(1));
    }

    #[test]
    fn new_agent_sets_expected_initial_values() {
        let creator = IdentityId::new().unwrap();
        let identity = Identity::new_agent("worker-1".to_owned(), creator).unwrap();

        assert_eq!(identity.identity_type, IdentityType::Agent);
        assert_eq!(identity.display_name, "worker-1");
        assert_eq!(identity.created_by, Some(creator));
        assert_eq!(identity.lifecycle, IdentityLifecycle::Active);
        assert!(identity.allowed_targets.is_empty());
        assert!(identity.allowed_message_kinds.is_empty());
        assert!(identity.capability_scope.is_none());
    }

    #[test]
    fn new_external_sets_expected_initial_values() {
        let creator = IdentityId::new().unwrap();
        let targets = vec!["lead".to_owned(), "tester".to_owned()];
        let identity =
            Identity::new_external("ci-bot".to_owned(), creator, targets.clone()).unwrap();

        assert_eq!(identity.identity_type, IdentityType::External);
        assert_eq!(identity.display_name, "ci-bot");
        assert_eq!(identity.created_by, Some(creator));
        assert_eq!(identity.lifecycle, IdentityLifecycle::Active);
        assert_eq!(identity.allowed_targets, targets);
        assert!(identity.allowed_message_kinds.is_empty());
        assert!(identity.capability_scope.is_none());
    }

    #[test]
    fn new_external_accepts_empty_allowed_targets() {
        let creator = IdentityId::new().unwrap();
        let identity = Identity::new_external("scoped".to_owned(), creator, Vec::new()).unwrap();
        assert!(identity.allowed_targets.is_empty());
    }

    #[test]
    fn two_identities_have_distinct_ids() {
        let a = Identity::new_operator("a".to_owned()).unwrap();
        let b = Identity::new_operator("b".to_owned()).unwrap();
        assert_ne!(a.identity_id, b.identity_id);
    }

    #[test]
    fn identity_serde_round_trip_active() {
        let creator = IdentityId::new().unwrap();
        let identity =
            Identity::new_external("external".to_owned(), creator, vec!["lead".to_owned()])
                .unwrap();

        let json = serde_json::to_string(&identity).unwrap();
        assert!(!json.contains("expires_at"));
        assert!(!json.contains("revoked_at"));
        let decoded: Identity = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, identity);
    }

    #[test]
    fn identity_serde_round_trip_active_until_preserves_some_branch() {
        let creator = IdentityId::new().unwrap();
        let mut identity = Identity::new_agent("worker".to_owned(), creator).unwrap();
        let expiry = OffsetDateTime::now_utc() + Duration::days(30);
        identity.lifecycle = IdentityLifecycle::ActiveUntil { expires_at: expiry };

        let json = serde_json::to_string(&identity).unwrap();
        assert!(json.contains("expires_at"));
        let decoded: Identity = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, identity);
    }

    #[test]
    fn identity_serde_round_trip_revoked_preserves_some_branch() {
        let creator = IdentityId::new().unwrap();
        let mut identity = Identity::new_agent("worker".to_owned(), creator).unwrap();
        let revoked = OffsetDateTime::now_utc();
        identity.lifecycle = IdentityLifecycle::Revoked {
            revoked_at: revoked,
        };

        let json = serde_json::to_string(&identity).unwrap();
        assert!(json.contains("revoked_at"));
        let decoded: Identity = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, identity);
    }

    #[test]
    fn identity_deserialize_rejects_expires_and_revoked_together() {
        let creator = IdentityId::new().unwrap();
        let identity = Identity::new_operator("op".to_owned()).unwrap();
        let json = serde_json::json!({
            "identity_id": identity.identity_id,
            "identity_type": "operator",
            "display_name": "op",
            "created_by": creator,
            "created_at": "2026-01-01T00:00:00Z",
            "expires_at": "2026-06-01T00:00:00Z",
            "revoked_at": "2026-03-01T00:00:00Z",
        });
        let result: Result<Identity, _> = serde_json::from_value(json);
        let err = result
            .expect_err("expected identity with both expires_at and revoked_at to be rejected");
        assert!(
            err.to_string()
                .contains(&IdentityLifecycleError::ExpiryAndRevocation.to_string()),
            "deserialize error did not surface IdentityLifecycleError: {err}"
        );
    }

    #[test]
    fn identity_deserialize_rejects_expiry_not_after_creation() {
        let identity = Identity::new_operator("op".to_owned()).unwrap();
        let json = serde_json::json!({
            "identity_id": identity.identity_id,
            "identity_type": "operator",
            "display_name": "op",
            "created_at": "2026-06-01T00:00:00Z",
            "expires_at": "2026-06-01T00:00:00Z",
        });
        let result: Result<Identity, _> = serde_json::from_value(json);
        let err = result.expect_err("expected expires_at == created_at to be rejected");
        let expected = IdentityLifecycleError::ExpiryNotAfterCreation {
            created_at: time::macros::datetime!(2026-06-01 00:00:00 UTC),
            expires_at: time::macros::datetime!(2026-06-01 00:00:00 UTC),
        };
        assert!(
            err.to_string().contains(&expected.to_string()),
            "deserialize error did not surface ExpiryNotAfterCreation: {err}"
        );
    }

    #[test]
    fn identity_deserialize_rejects_expiry_before_creation() {
        let identity = Identity::new_operator("op".to_owned()).unwrap();
        let json = serde_json::json!({
            "identity_id": identity.identity_id,
            "identity_type": "operator",
            "display_name": "op",
            "created_at": "2026-06-01T00:00:00Z",
            "expires_at": "2026-01-01T00:00:00Z",
        });
        let result: Result<Identity, _> = serde_json::from_value(json);
        let err = result.expect_err("expected expires_at < created_at to be rejected");
        assert!(
            err.to_string().contains("expiry is not after creation"),
            "deserialize error did not surface ExpiryNotAfterCreation: {err}"
        );
    }

    #[test]
    fn identity_deserialize_rejects_revocation_before_creation() {
        let identity = Identity::new_operator("op".to_owned()).unwrap();
        let json = serde_json::json!({
            "identity_id": identity.identity_id,
            "identity_type": "operator",
            "display_name": "op",
            "created_at": "2026-06-01T00:00:00Z",
            "revoked_at": "2026-01-01T00:00:00Z",
        });
        let result: Result<Identity, _> = serde_json::from_value(json);
        let err = result.expect_err("expected revoked_at < created_at to be rejected");
        let expected = IdentityLifecycleError::RevocationBeforeCreation {
            created_at: time::macros::datetime!(2026-06-01 00:00:00 UTC),
            revoked_at: time::macros::datetime!(2026-01-01 00:00:00 UTC),
        };
        assert!(
            err.to_string().contains(&expected.to_string()),
            "deserialize error did not surface RevocationBeforeCreation: {err}"
        );
    }

    #[test]
    fn identity_deserialize_accepts_revocation_at_same_instant_as_creation() {
        let identity = Identity::new_operator("op".to_owned()).unwrap();
        let json = serde_json::json!({
            "identity_id": identity.identity_id,
            "identity_type": "operator",
            "display_name": "op",
            "created_at": "2026-06-01T00:00:00Z",
            "revoked_at": "2026-06-01T00:00:00Z",
        });
        let decoded: Identity = serde_json::from_value(json).unwrap();
        assert_eq!(
            decoded.lifecycle,
            IdentityLifecycle::Revoked {
                revoked_at: time::macros::datetime!(2026-06-01 00:00:00 UTC),
            }
        );
    }

    #[test]
    fn identity_id_error_display_uses_identity_id_noun() {
        let err = IdentityIdError::NotV7 { actual_version: 4 };
        assert!(
            err.to_string().contains("identity id"),
            "expected 'identity id' in error: {err}"
        );
    }

    #[test]
    fn identity_type_serializes_lowercase() {
        let json = serde_json::to_string(&IdentityType::Operator).unwrap();
        assert_eq!(json, "\"operator\"");
        let json = serde_json::to_string(&IdentityType::Agent).unwrap();
        assert_eq!(json, "\"agent\"");
        let json = serde_json::to_string(&IdentityType::External).unwrap();
        assert_eq!(json, "\"external\"");
    }
}
