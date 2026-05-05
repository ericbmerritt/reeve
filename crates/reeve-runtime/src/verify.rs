//! Verification pipeline per `specs/reeve-transport-security.md` § Signed
//! Message Envelope and the Phase 4 deliverable in
//! `specs/reeve-walking-skeleton.ladder.md`.
//!
//! [`VerificationPipeline::verify`] takes raw bytes from `inbox/new/`, walks
//! them through every check the spec requires, and returns a [`Verdict`]:
//! either [`Verdict::Deliver`] (all checks passed) or
//! [`Verdict::Quarantine`] (something failed, with a typed reason). The
//! pipeline is the decision layer; the watcher (Task 13) acts on the verdict
//! by moving files and emitting audit events.
//!
//! **Pipeline order** (per spec):
//! 1. Size bound — bytes exceeding [`MAX_ENVELOPE_BYTES`] are rejected with
//!    `ParseFailure` before any JSON parse begins; this bounds memory and CPU
//!    consumption from oversized payloads.
//! 2. Parse — `serde_json::from_slice::<Envelope>`. Unknown fields, wrong-
//!    length byte arrays, unsupported schema version, and non-v7 UUIDs are
//!    rejected by the `Envelope` deserializer (Tasks 5/8 enforce these).
//! 3. Recipient match — `envelope.recipient_id == recipient_inbox_id`.
//! 4. Clock skew — `envelope.created_at` within `±clock_skew_tolerance`.
//! 5. Sender lookup — registry must know `envelope.sender_id`.
//! 6. Key lookup — sender's key record must carry `envelope.sender_key_id`.
//! 7. Key state — `Active` passes; `Deprecated` passes only when
//!    `envelope.created_at <= valid_until`; `Revoked` always fails.
//! 8. Signature verify — `verify_envelope` (RFC 8032 strict path).
//! 9. Replay check — `replay_ledger.contains` then `observe`.
//! 10. Delivery deduplication — `delivery_ledger.contains`; already-delivered
//!     messages still produce `Verdict::Deliver` (the watcher skips the
//!     insertion, but moves the file to `cur/` either way).
//!
//! [`VerificationError`] covers system failures (disk, registry, audit) that
//! prevent the pipeline from running at all. Verification verdicts (what the
//! pipeline decides about a well-received message) are always `Ok(Verdict)`.
//!
//! ## Audit contract
//!
//! The pipeline returns [`Verdict`] without emitting any audit events itself.
//! Every `Quarantine` verdict **must** be paired with a call to
//! [`emit_quarantine_audit`] by the watcher, after the file has been durably
//! moved to `quarantine/`. Failure to do so silently drops security events.
//! The watcher (Task 13) owns this obligation; the pipeline does not.
//!
//! ## Replay-observe placement
//!
//! The spec states "a message rejected for any reason still updates the replay
//! ledger." This pipeline deliberately diverges from that wording: the replay
//! `observe` call fires only *after* signature verification succeeds (step 9),
//! not before. The reason is a burn-slot attack: if an attacker can submit an
//! envelope with a forged signature that shares a legitimate sender's
//! `(sender_id, message_id, nonce)` tuple, observing pre-signature would
//! permanently consume that replay slot, preventing the real message from ever
//! delivering. By placing `observe` after signature verify, only authenticated
//! senders can occupy replay slots.

use std::fmt;
use std::sync::Arc;

use reeve_transport::sign::verify_envelope;
use reeve_types::{Envelope, Identity, IdentityId, KeyId, KeyRecord, KeyState, MessageId};
use time::{Duration, OffsetDateTime};

use crate::audit::{AuditError, AuditEvent, AuditLog};
use crate::identity_registry::{IdentityRegistry, RegistryError};
use crate::ledger::{DeliveryKey, DeliveryLedger, LedgerError, ReplayKey, ReplayLedger};

/// Hard upper bound on envelope file size. Bytes beyond this limit are
/// rejected with [`QuarantineReason::ParseFailure`] before any JSON parse
/// begins, bounding memory and CPU consumption from oversized payloads dropped
/// into `inbox/new/`. Bounded by spec § Filesystem Safety: "bounded file size".
pub const MAX_ENVELOPE_BYTES: usize = 1024 * 1024; // 1 MiB

/// Default clock skew tolerance: five minutes in each direction. Matches
/// common practice for Kerberos and similar time-sensitive protocols.
pub const DEFAULT_CLOCK_SKEW: Duration = Duration::minutes(5);

/// The verdict returned by [`VerificationPipeline::verify`].
#[derive(Debug)]
pub enum Verdict {
    /// All checks passed. The watcher should move the file to `cur/` and
    /// record delivery in the delivery ledger. `already_delivered` is true
    /// when the delivery ledger already contains this `(recipient_id,
    /// message_id)`; the watcher still moves to `cur/` but skips context
    /// insertion.
    Deliver {
        envelope: Box<Envelope>,
        sender: Identity,
        key_record: Box<KeyRecord>,
        already_delivered: bool,
    },
    /// A check failed. The watcher should move the file to `quarantine/` and
    /// emit a `transport.quarantine` audit event. `identifying` carries
    /// whatever envelope fields were extracted before the failure.
    Quarantine {
        reason: QuarantineReason,
        identifying: Option<EnvelopeIds>,
    },
}

/// The reason an envelope was quarantined. Each variant corresponds to a
/// distinct pipeline check, giving audit consumers enough information to
/// diagnose operator misconfiguration, implementation bugs, or replay attacks
/// without re-reading the raw bytes.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuarantineReason {
    /// The bytes were not valid Envelope JSON, or a field failed its own
    /// structural invariant (wrong-length nonce, non-v7 UUID, unsupported
    /// schema version, or file size exceeding [`MAX_ENVELOPE_BYTES`]).
    ParseFailure,
    /// `envelope.recipient_id` does not match the inbox being watched.
    RecipientMismatch { expected: IdentityId },
    /// `envelope.created_at` is further than `clock_skew_tolerance` from
    /// `now` in either direction.
    ClockSkew {
        envelope_at: OffsetDateTime,
        now: OffsetDateTime,
    },
    /// `envelope.sender_id` is not in the registry.
    SenderUnknown,
    /// `envelope.sender_key_id` is not among the sender's key records.
    KeyUnknown,
    /// The key was deprecated and `envelope.created_at` is after
    /// `valid_until`. The deprecation window has closed for this message.
    KeyExpired,
    /// The key was revoked. Revoked keys verify nothing regardless of
    /// `created_at`.
    KeyRevoked,
    /// The signature bytes did not verify against the sender's public key.
    SignatureInvalid,
    /// The replay ledger already contains this `(sender_id, message_id,
    /// nonce)` tuple.
    Replay,
}

impl fmt::Display for QuarantineReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParseFailure => f.write_str("parse_failure"),
            Self::RecipientMismatch { .. } => f.write_str("recipient_mismatch"),
            Self::ClockSkew { .. } => f.write_str("clock_skew"),
            Self::SenderUnknown => f.write_str("sender_unknown"),
            Self::KeyUnknown => f.write_str("key_unknown"),
            Self::KeyExpired => f.write_str("key_expired"),
            Self::KeyRevoked => f.write_str("key_revoked"),
            Self::SignatureInvalid => f.write_str("signature_invalid"),
            Self::Replay => f.write_str("replay"),
        }
    }
}

/// Envelope fields extracted before a pipeline failure, for audit-log and
/// quarantine-metadata purposes. All fields are `Option` because earlier
/// stages may not have extracted them yet.
#[derive(Debug, Clone)]
pub struct EnvelopeIds {
    pub sender_id: Option<IdentityId>,
    pub sender_key_id: Option<KeyId>,
    pub recipient_id: Option<IdentityId>,
    pub message_id: Option<MessageId>,
}

impl EnvelopeIds {
    fn from_envelope(env: &Envelope) -> Self {
        Self {
            sender_id: Some(env.sender_id),
            sender_key_id: Some(env.sender_key_id),
            recipient_id: Some(env.recipient_id),
            message_id: Some(env.message_id),
        }
    }
}

/// System errors that prevent the pipeline from running. These differ from
/// [`QuarantineReason`] values, which are decisions about user-supplied data.
/// A [`VerificationError`] means the runtime infrastructure failed.
#[non_exhaustive]
#[derive(Debug)]
pub enum VerificationError {
    /// The identity registry could not be queried.
    Registry(RegistryError),
    /// The replay ledger could not be read or written.
    Replay(LedgerError),
    /// The delivery ledger could not be read or written.
    Delivery(LedgerError),
    /// The audit log could not be appended to.
    Audit(AuditError),
    /// A fallback `MessageId` could not be minted because the host clock is
    /// set before 1970. The watcher should log and continue rather than
    /// crashing; the audit event is dropped for this envelope.
    MessageIdMint,
}

impl fmt::Display for VerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(source) => write!(f, "verification pipeline: registry: {source}"),
            Self::Replay(source) => write!(f, "verification pipeline: replay ledger: {source}"),
            Self::Delivery(source) => write!(f, "verification pipeline: delivery ledger: {source}"),
            Self::Audit(source) => write!(f, "verification pipeline: audit log: {source}"),
            Self::MessageIdMint => {
                f.write_str("verification pipeline: host clock is before 1970 — cannot mint fallback MessageId for audit event")
            }
        }
    }
}

impl std::error::Error for VerificationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Registry(source) => Some(source),
            Self::Replay(source) | Self::Delivery(source) => Some(source),
            Self::Audit(source) => Some(source),
            Self::MessageIdMint => None,
        }
    }
}

/// Walks raw envelope bytes through every check defined in
/// `specs/reeve-transport-security.md` and returns a [`Verdict`].
///
/// The pipeline does not emit audit events itself: quarantine events are the
/// watcher's responsibility and must fire after the file has been durably
/// moved to `cur/` or `quarantine/`. Use [`emit_quarantine_audit`] from the
/// watcher to record rejections. The replay `observe` call does mutate the
/// ledger before returning, which is an intentional side effect — it records
/// the tuple so a retry cannot slip through as a new first-seen message.
///
/// `Clone` is intentionally not derived. Callers that need shared access
/// should wrap in `Arc<VerificationPipeline>`.
#[derive(Debug)]
pub struct VerificationPipeline {
    registry: Arc<IdentityRegistry>,
    replay: Arc<ReplayLedger>,
    delivery: Arc<DeliveryLedger>,
    clock_skew_tolerance: Duration,
}

impl VerificationPipeline {
    /// Construct a pipeline.
    ///
    /// `clock_skew_tolerance` is applied symmetrically: an envelope is
    /// rejected if `|envelope.created_at − now| > clock_skew_tolerance`. Use
    /// [`DEFAULT_CLOCK_SKEW`] for the standard five-minute window.
    pub fn new(
        registry: Arc<IdentityRegistry>,
        replay: Arc<ReplayLedger>,
        delivery: Arc<DeliveryLedger>,
        clock_skew_tolerance: Duration,
    ) -> Self {
        Self {
            registry,
            replay,
            delivery,
            clock_skew_tolerance,
        }
    }

    /// Run the full pipeline on `bytes`.
    ///
    /// `recipient_inbox_id` is the identity whose `new/` directory the file
    /// came from. The pipeline verifies that `envelope.recipient_id` matches
    /// it. `now` is the caller-supplied wall-clock instant; callers use
    /// `OffsetDateTime::now_utc()` in production and a fixed value in tests.
    ///
    /// # Errors
    ///
    /// Returns [`VerificationError`] only for infrastructure failures (registry
    /// unavailable, ledger disk error, audit log write failure). A verdict of
    /// [`Verdict::Quarantine`] is returned as `Ok(Verdict::Quarantine { .. })`,
    /// not as an error.
    pub fn verify(
        &self,
        bytes: &[u8],
        recipient_inbox_id: IdentityId,
        now: OffsetDateTime,
    ) -> Result<Verdict, VerificationError> {
        if bytes.len() > MAX_ENVELOPE_BYTES {
            return Ok(Verdict::Quarantine {
                reason: QuarantineReason::ParseFailure,
                identifying: None,
            });
        }

        let Ok(envelope) = serde_json::from_slice::<Envelope>(bytes) else {
            return Ok(Verdict::Quarantine {
                reason: QuarantineReason::ParseFailure,
                identifying: None,
            });
        };

        let ids = EnvelopeIds::from_envelope(&envelope);

        if let Some(v) = self.check_header(&envelope, &ids, recipient_inbox_id, now) {
            return Ok(v);
        }

        let (sender, key_record) = match self.check_identity(&envelope, &ids)? {
            Ok(pair) => pair,
            Err(v) => return Ok(v),
        };

        if let Some(v) = Self::check_key_state(&envelope, &key_record, &ids) {
            return Ok(v);
        }

        if verify_envelope(&envelope, &key_record.public_key).is_err() {
            return Ok(Verdict::Quarantine {
                reason: QuarantineReason::SignatureInvalid,
                identifying: Some(ids),
            });
        }

        if let Some(v) = self.check_replay(&envelope, &ids, now)? {
            return Ok(v);
        }

        let delivery_key = DeliveryKey {
            recipient_id: envelope.recipient_id,
            message_id: envelope.message_id,
        };
        let already_delivered = self
            .delivery
            .contains(&delivery_key)
            .map_err(VerificationError::Delivery)?;

        Ok(Verdict::Deliver {
            envelope: Box::new(envelope),
            sender,
            key_record: Box::new(key_record),
            already_delivered,
        })
    }

    fn check_header(
        &self,
        envelope: &Envelope,
        ids: &EnvelopeIds,
        recipient_inbox_id: IdentityId,
        now: OffsetDateTime,
    ) -> Option<Verdict> {
        if envelope.recipient_id != recipient_inbox_id {
            return Some(Verdict::Quarantine {
                reason: QuarantineReason::RecipientMismatch {
                    expected: recipient_inbox_id,
                },
                identifying: Some(ids.clone()),
            });
        }

        let skew = envelope.created_at - now;
        let abs_skew = if skew < Duration::ZERO { -skew } else { skew };
        if abs_skew > self.clock_skew_tolerance {
            return Some(Verdict::Quarantine {
                reason: QuarantineReason::ClockSkew {
                    envelope_at: envelope.created_at,
                    now,
                },
                identifying: Some(ids.clone()),
            });
        }

        None
    }

    fn check_identity(
        &self,
        envelope: &Envelope,
        ids: &EnvelopeIds,
    ) -> Result<Result<(Identity, KeyRecord), Verdict>, VerificationError> {
        let Some(stored) = self
            .registry
            .lookup(envelope.sender_id)
            .map_err(VerificationError::Registry)?
        else {
            return Ok(Err(Verdict::Quarantine {
                reason: QuarantineReason::SenderUnknown,
                identifying: Some(ids.clone()),
            }));
        };

        let Some(key_record) = stored
            .key_records()
            .iter()
            .find(|kr| kr.key_id == envelope.sender_key_id)
            .cloned()
        else {
            return Ok(Err(Verdict::Quarantine {
                reason: QuarantineReason::KeyUnknown,
                identifying: Some(ids.clone()),
            }));
        };

        Ok(Ok((stored.identity().clone(), key_record)))
    }

    fn check_key_state(
        envelope: &Envelope,
        key_record: &KeyRecord,
        ids: &EnvelopeIds,
    ) -> Option<Verdict> {
        match key_record.state {
            KeyState::Active => None,
            KeyState::Deprecated { valid_until } => {
                if envelope.created_at > valid_until {
                    Some(Verdict::Quarantine {
                        reason: QuarantineReason::KeyExpired,
                        identifying: Some(ids.clone()),
                    })
                } else {
                    None
                }
            }
            KeyState::Revoked { .. } => Some(Verdict::Quarantine {
                reason: QuarantineReason::KeyRevoked,
                identifying: Some(ids.clone()),
            }),
        }
    }

    fn check_replay(
        &self,
        envelope: &Envelope,
        ids: &EnvelopeIds,
        now: OffsetDateTime,
    ) -> Result<Option<Verdict>, VerificationError> {
        let replay_key = ReplayKey {
            sender_id: envelope.sender_id,
            message_id: envelope.message_id,
            nonce: envelope.nonce,
        };
        if self
            .replay
            .contains(&replay_key)
            .map_err(VerificationError::Replay)?
        {
            return Ok(Some(Verdict::Quarantine {
                reason: QuarantineReason::Replay,
                identifying: Some(ids.clone()),
            }));
        }
        self.replay
            .observe(replay_key, now)
            .map_err(VerificationError::Replay)?;
        Ok(None)
    }
}

/// Emit a `transport.quarantine` audit event. Quarantine emission is factored
/// out because the watcher calls it after moving the file, but tests that
/// exercise the pipeline in isolation may call it directly.
///
/// Falls back to `None` fields when `identifying` is absent — `ParseFailure`
/// verdicts carry no extractable sender or message id.
///
/// # Errors
///
/// Returns [`VerificationError::Audit`] when the log append fails.
/// Returns [`VerificationError::MessageIdMint`] when `identifying` contains no
/// `message_id` and the host clock is set before 1970 (pre-epoch clock prevents
/// fallback `MessageId` generation). The watcher should log and continue.
pub fn emit_quarantine_audit(
    audit: &AuditLog,
    reason: &QuarantineReason,
    identifying: Option<&EnvelopeIds>,
    recipient_inbox_id: IdentityId,
    at: OffsetDateTime,
) -> Result<(), VerificationError> {
    let (sender_id, sender_key_id, message_id) = match identifying {
        Some(ids) => (ids.sender_id, ids.sender_key_id, ids.message_id),
        None => (None, None, None),
    };
    let fallback_message_id = match message_id {
        Some(id) => id,
        None => MessageId::new().map_err(|_| VerificationError::MessageIdMint)?,
    };
    audit
        .append(&AuditEvent::TransportQuarantine {
            sender_id,
            sender_key_id,
            recipient_id: recipient_inbox_id,
            message_id: fallback_message_id,
            reason: reason.to_string(),
            at,
        })
        .map_err(VerificationError::Audit)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use reeve_transport::sign::sign_envelope;
    use reeve_types::{
        Envelope, EnvelopeSignature, Identity, IdentityId, KeyId, KeyRecord, KeyState, Keypair,
        MessageId, Nonce, PayloadHash, SchemaVersion, NONCE_LEN, PAYLOAD_HASH_LEN, SIGNATURE_LEN,
    };
    use time::macros::datetime;
    use time::OffsetDateTime;

    use crate::identity_registry::{IdentityRegistry, StoredIdentity};
    use crate::ledger::{DeliveryLedger, ReplayLedger};

    struct TestContext {
        pipeline: VerificationPipeline,
        keypair: Keypair,
        sender_id: IdentityId,
        key_record: KeyRecord,
        recipient_id: IdentityId,
    }

    fn build_context() -> TestContext {
        let reg_dir = crate::test_support::secure_dir();
        let replay_dir = crate::test_support::secure_dir();
        let delivery_dir = crate::test_support::secure_dir();

        let registry = Arc::new(IdentityRegistry::open(reg_dir.keep()).unwrap());
        let replay = Arc::new(ReplayLedger::open(replay_dir.keep()).unwrap());
        let delivery = Arc::new(DeliveryLedger::open(delivery_dir.keep()).unwrap());

        let keypair = Keypair::generate();
        let identity = Identity::new_operator("test-sender".to_owned()).unwrap();
        let sender_id = identity.identity_id;
        let key_record = KeyRecord::new(sender_id, *keypair.public()).unwrap();
        let stored = StoredIdentity::new(identity, key_record.clone()).unwrap();
        registry.write(&stored).unwrap();

        let recipient_id = IdentityId::new().unwrap();

        let pipeline = VerificationPipeline::new(registry, replay, delivery, DEFAULT_CLOCK_SKEW);

        TestContext {
            pipeline,
            keypair,
            sender_id,
            key_record,
            recipient_id,
        }
    }

    /// Convenience wrapper returning only the [`Envelope`]; the serialized
    /// bytes from [`crate::test_support::signed_envelope_at`] are discarded
    /// because verify-layer tests don't need them.
    fn signed_envelope(
        keypair: &Keypair,
        sender_id: IdentityId,
        key_id: KeyId,
        recipient_id: IdentityId,
        now: OffsetDateTime,
    ) -> Envelope {
        crate::test_support::signed_envelope_at(keypair, sender_id, key_id, recipient_id, now).0
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    // VP1: garbage bytes yield ParseFailure with no identifying info.
    #[test]
    fn parse_failure_on_garbage_bytes() {
        let ctx = build_context();
        let result = ctx
            .pipeline
            .verify(b"not json at all", ctx.recipient_id, now())
            .unwrap();
        assert!(
            matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::ParseFailure,
                    identifying: None
                }
            ),
            "expected ParseFailure, got {result:?}",
        );
    }

    // VP2: schema_version 99 yields ParseFailure (Envelope deserializer rejects it).
    #[test]
    fn unsupported_schema_version_yields_parse_failure() {
        let ctx = build_context();
        let env = signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
            now(),
        );
        let mut value: serde_json::Value = serde_json::to_value(&env).unwrap();
        value["schema_version"] = serde_json::Value::Number(99.into());
        let bytes = serde_json::to_vec(&value).unwrap();

        let result = ctx
            .pipeline
            .verify(&bytes, ctx.recipient_id, now())
            .unwrap();
        assert!(
            matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::ParseFailure,
                    ..
                }
            ),
            "expected ParseFailure for schema_version 99, got {result:?}",
        );
    }

    // VP3: envelope to recipient A, inbox owned by B → RecipientMismatch.
    #[test]
    fn recipient_mismatch_yields_quarantine() {
        let ctx = build_context();
        let other_recipient = IdentityId::new().unwrap();
        let env = signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
            now(),
        );
        let bytes = serde_json::to_vec(&env).unwrap();
        let result = ctx.pipeline.verify(&bytes, other_recipient, now()).unwrap();
        assert!(
            matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::RecipientMismatch { expected },
                    ..
                } if expected == other_recipient
            ),
            "expected RecipientMismatch, got {result:?}",
        );
    }

    // VP4a: envelope timestamp far in the past → ClockSkew.
    #[test]
    fn clock_skew_far_past_yields_quarantine() {
        let ctx = build_context();
        let past = datetime!(2000-01-01 00:00:00 UTC);
        let env = signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
            past,
        );
        let bytes = serde_json::to_vec(&env).unwrap();
        let result = ctx
            .pipeline
            .verify(&bytes, ctx.recipient_id, now())
            .unwrap();
        assert!(
            matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::ClockSkew { .. },
                    ..
                }
            ),
            "expected ClockSkew for far-past timestamp, got {result:?}",
        );
    }

    // VP4b: envelope timestamp far in the future → ClockSkew.
    #[test]
    fn clock_skew_far_future_yields_quarantine() {
        let ctx = build_context();
        let future = datetime!(2099-01-01 00:00:00 UTC);
        let env = signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
            future,
        );
        let bytes = serde_json::to_vec(&env).unwrap();
        let result = ctx
            .pipeline
            .verify(&bytes, ctx.recipient_id, now())
            .unwrap();
        assert!(
            matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::ClockSkew { .. },
                    ..
                }
            ),
            "expected ClockSkew for far-future timestamp, got {result:?}",
        );
    }

    // VP4c: envelope timestamp just within tolerance → not quarantined on that
    // basis (passes clock skew; may fail other checks).
    #[test]
    fn clock_skew_within_tolerance_passes() {
        let ctx = build_context();
        let reference = now();
        let just_within = reference - Duration::minutes(4);
        let env = signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
            just_within,
        );
        let bytes = serde_json::to_vec(&env).unwrap();
        let result = ctx
            .pipeline
            .verify(&bytes, ctx.recipient_id, reference)
            .unwrap();
        assert!(
            !matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::ClockSkew { .. },
                    ..
                }
            ),
            "4-min-old envelope must not trigger ClockSkew with 5-min tolerance: {result:?}",
        );
    }

    // VP5: sender_id not in registry → SenderUnknown.
    #[test]
    fn sender_unknown_yields_quarantine() {
        let ctx = build_context();
        let phantom_keypair = Keypair::generate();
        let phantom_id = IdentityId::new().unwrap();
        let phantom_key_id = KeyId::new().unwrap();
        let env = signed_envelope(
            &phantom_keypair,
            phantom_id,
            phantom_key_id,
            ctx.recipient_id,
            now(),
        );
        let bytes = serde_json::to_vec(&env).unwrap();
        let result = ctx
            .pipeline
            .verify(&bytes, ctx.recipient_id, now())
            .unwrap();
        assert!(
            matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::SenderUnknown,
                    ..
                }
            ),
            "expected SenderUnknown, got {result:?}",
        );
    }

    // VP6: sender exists but sender_key_id not on their identity → KeyUnknown.
    #[test]
    fn key_unknown_yields_quarantine() {
        let ctx = build_context();
        let wrong_key_id = KeyId::new().unwrap();
        let env = signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            wrong_key_id,
            ctx.recipient_id,
            now(),
        );
        let bytes = serde_json::to_vec(&env).unwrap();
        let result = ctx
            .pipeline
            .verify(&bytes, ctx.recipient_id, now())
            .unwrap();
        assert!(
            matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::KeyUnknown,
                    ..
                }
            ),
            "expected KeyUnknown, got {result:?}",
        );
    }

    // VP7: sender's key state is Revoked → KeyRevoked.
    #[test]
    fn key_revoked_yields_quarantine() {
        let reg_dir = crate::test_support::secure_dir();
        let replay_dir = crate::test_support::secure_dir();
        let delivery_dir = crate::test_support::secure_dir();

        let registry = Arc::new(IdentityRegistry::open(reg_dir.keep()).unwrap());
        let replay = Arc::new(ReplayLedger::open(replay_dir.keep()).unwrap());
        let delivery = Arc::new(DeliveryLedger::open(delivery_dir.keep()).unwrap());

        let keypair = Keypair::generate();
        let identity = Identity::new_operator("revoked-sender".to_owned()).unwrap();
        let sender_id = identity.identity_id;
        let mut key_record = KeyRecord::new(sender_id, *keypair.public()).unwrap();
        key_record.state = KeyState::Revoked {
            revoked_at: OffsetDateTime::now_utc(),
        };
        let stored = StoredIdentity::new(identity, key_record.clone()).unwrap();
        registry.write(&stored).unwrap();

        let recipient_id = IdentityId::new().unwrap();
        let pipeline = VerificationPipeline::new(registry, replay, delivery, DEFAULT_CLOCK_SKEW);

        let env = signed_envelope(&keypair, sender_id, key_record.key_id, recipient_id, now());
        let bytes = serde_json::to_vec(&env).unwrap();
        let result = pipeline.verify(&bytes, recipient_id, now()).unwrap();
        assert!(
            matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::KeyRevoked,
                    ..
                }
            ),
            "expected KeyRevoked, got {result:?}",
        );
    }

    // VP8: deprecated key, envelope.created_at after valid_until → KeyExpired.
    #[test]
    fn key_expired_deprecated_past_window_yields_quarantine() {
        let reg_dir = crate::test_support::secure_dir();
        let replay_dir = crate::test_support::secure_dir();
        let delivery_dir = crate::test_support::secure_dir();

        let registry = Arc::new(IdentityRegistry::open(reg_dir.keep()).unwrap());
        let replay = Arc::new(ReplayLedger::open(replay_dir.keep()).unwrap());
        let delivery = Arc::new(DeliveryLedger::open(delivery_dir.keep()).unwrap());

        let keypair = Keypair::generate();
        let identity = Identity::new_operator("deprecated-sender".to_owned()).unwrap();
        let sender_id = identity.identity_id;
        let mut key_record = KeyRecord::new(sender_id, *keypair.public()).unwrap();
        // Set valid_from in the past so the deprecation window [valid_from, valid_until]
        // is not inverted. The key's window ends before the envelope creation date.
        key_record.valid_from = datetime!(2019-01-01 00:00:00 UTC);
        let valid_until = datetime!(2020-01-01 00:00:00 UTC);
        key_record.state = KeyState::Deprecated { valid_until };
        let stored = StoredIdentity::new(identity, key_record.clone()).unwrap();
        registry.write(&stored).unwrap();

        let recipient_id = IdentityId::new().unwrap();
        let pipeline = VerificationPipeline::new(registry, replay, delivery, DEFAULT_CLOCK_SKEW);

        // Envelope created at is after valid_until; use a "now" close to this
        // past date to avoid triggering the clock-skew check.
        let envelope_at = datetime!(2020-01-02 00:00:00 UTC);
        let now_ref = datetime!(2020-01-02 00:00:01 UTC);
        let env = signed_envelope(
            &keypair,
            sender_id,
            key_record.key_id,
            recipient_id,
            envelope_at,
        );
        let bytes = serde_json::to_vec(&env).unwrap();
        let result = pipeline.verify(&bytes, recipient_id, now_ref).unwrap();
        assert!(
            matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::KeyExpired,
                    ..
                }
            ),
            "expected KeyExpired, got {result:?}",
        );
    }

    // VP9: deprecated key, envelope.created_at within window → passes key
    // check (may succeed overall if other checks pass).
    #[test]
    fn key_deprecated_within_window_passes_key_check() {
        let reg_dir = crate::test_support::secure_dir();
        let replay_dir = crate::test_support::secure_dir();
        let delivery_dir = crate::test_support::secure_dir();

        let registry = Arc::new(IdentityRegistry::open(reg_dir.keep()).unwrap());
        let replay = Arc::new(ReplayLedger::open(replay_dir.keep()).unwrap());
        let delivery = Arc::new(DeliveryLedger::open(delivery_dir.keep()).unwrap());

        let keypair = Keypair::generate();
        let identity = Identity::new_operator("deprecated-ok-sender".to_owned()).unwrap();
        let sender_id = identity.identity_id;
        let mut key_record = KeyRecord::new(sender_id, *keypair.public()).unwrap();
        // Set valid_from before valid_until so the deprecation window is valid.
        key_record.valid_from = datetime!(2019-01-01 00:00:00 UTC);
        let valid_until = datetime!(2020-06-01 00:00:00 UTC);
        key_record.state = KeyState::Deprecated { valid_until };
        let stored = StoredIdentity::new(identity, key_record.clone()).unwrap();
        registry.write(&stored).unwrap();

        let recipient_id = IdentityId::new().unwrap();
        let pipeline = VerificationPipeline::new(registry, replay, delivery, DEFAULT_CLOCK_SKEW);

        // created_at is before valid_until, so key is still within its window.
        let envelope_at = datetime!(2020-01-01 00:00:00 UTC);
        let now_ref = datetime!(2020-01-01 00:00:01 UTC);
        let env = signed_envelope(
            &keypair,
            sender_id,
            key_record.key_id,
            recipient_id,
            envelope_at,
        );
        let bytes = serde_json::to_vec(&env).unwrap();
        let result = pipeline.verify(&bytes, recipient_id, now_ref).unwrap();
        assert!(
            !matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::KeyExpired,
                    ..
                }
            ) && !matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::KeyRevoked,
                    ..
                }
            ),
            "deprecated-but-within-window key must not be rejected for key state: {result:?}",
        );
    }

    // VP10: tampered signature → SignatureInvalid.
    #[test]
    fn tampered_signature_yields_quarantine() {
        let ctx = build_context();
        let reference = now();
        let mut env = signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
            reference,
        );
        let mut sig_bytes = *env.signature.as_bytes();
        sig_bytes[0] ^= 0x01;
        env.signature = EnvelopeSignature::from_bytes(sig_bytes);

        let bytes = serde_json::to_vec(&env).unwrap();
        let result = ctx
            .pipeline
            .verify(&bytes, ctx.recipient_id, reference)
            .unwrap();
        assert!(
            matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::SignatureInvalid,
                    ..
                }
            ),
            "expected SignatureInvalid, got {result:?}",
        );
    }

    // VP11: run verify twice on the same envelope → second call returns Replay.
    #[test]
    fn replay_second_call_yields_quarantine() {
        let ctx = build_context();
        let reference = now();
        let env = signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
            reference,
        );
        let bytes = serde_json::to_vec(&env).unwrap();

        let first = ctx
            .pipeline
            .verify(&bytes, ctx.recipient_id, reference)
            .unwrap();
        assert!(
            matches!(first, Verdict::Deliver { .. }),
            "first verify must deliver, got {first:?}",
        );

        let second = ctx
            .pipeline
            .verify(&bytes, ctx.recipient_id, reference)
            .unwrap();
        assert!(
            matches!(
                second,
                Verdict::Quarantine {
                    reason: QuarantineReason::Replay,
                    ..
                }
            ),
            "second verify must be Replay, got {second:?}",
        );
    }

    // VP12: delivery ledger already contains (recipient_id, message_id) →
    // Deliver with already_delivered = true.
    #[test]
    fn delivery_deduplication_returns_deliver_with_flag() {
        let ctx = build_context();
        let reference = now();
        let env = signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
            reference,
        );

        let delivery_key = DeliveryKey {
            recipient_id: env.recipient_id,
            message_id: env.message_id,
        };
        ctx.pipeline
            .delivery
            .record(delivery_key, reference)
            .unwrap();

        let bytes = serde_json::to_vec(&env).unwrap();
        let result = ctx
            .pipeline
            .verify(&bytes, ctx.recipient_id, reference)
            .unwrap();
        assert!(
            matches!(
                result,
                Verdict::Deliver {
                    already_delivered: true,
                    ..
                }
            ),
            "expected Deliver{{already_delivered: true}}, got {result:?}",
        );
    }

    // VP13: full happy path — all checks pass, Deliver returned.
    #[test]
    fn happy_path_returns_deliver() {
        let ctx = build_context();
        let reference = now();
        let env = signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
            reference,
        );
        let bytes = serde_json::to_vec(&env).unwrap();
        let result = ctx
            .pipeline
            .verify(&bytes, ctx.recipient_id, reference)
            .unwrap();
        assert!(
            matches!(
                result,
                Verdict::Deliver {
                    already_delivered: false,
                    ..
                }
            ),
            "expected Deliver, got {result:?}",
        );
    }

    // IT1: integration — enroll operator, sign envelope, verify → Deliver.
    #[test]
    fn integration_enroll_sign_verify_deliver() {
        let reg_dir = crate::test_support::secure_dir();
        let replay_dir = crate::test_support::secure_dir();
        let delivery_dir = crate::test_support::secure_dir();

        let registry = Arc::new(IdentityRegistry::open(reg_dir.keep()).unwrap());
        let replay = Arc::new(ReplayLedger::open(replay_dir.keep()).unwrap());
        let delivery = Arc::new(DeliveryLedger::open(delivery_dir.keep()).unwrap());

        // Enroll operator.
        let keypair = Keypair::generate();
        let identity = Identity::new_operator("operator-alice".to_owned()).unwrap();
        let sender_id = identity.identity_id;
        let key_record = KeyRecord::new(sender_id, *keypair.public()).unwrap();
        let stored = StoredIdentity::new(identity, key_record.clone()).unwrap();
        registry.write(&stored).unwrap();

        let recipient_id = IdentityId::new().unwrap();
        let pipeline = VerificationPipeline::new(
            Arc::clone(&registry),
            Arc::clone(&replay),
            Arc::clone(&delivery),
            DEFAULT_CLOCK_SKEW,
        );

        // Sign.
        let reference = OffsetDateTime::now_utc();
        let placeholder = EnvelopeSignature::from_bytes([0u8; SIGNATURE_LEN]);
        let mut env = Envelope::new(
            SchemaVersion::V1,
            MessageId::new().unwrap(),
            sender_id,
            key_record.key_id,
            recipient_id,
            reference,
            Nonce::from_bytes([0xCCu8; NONCE_LEN]),
            PayloadHash::from_bytes([0xDDu8; PAYLOAD_HASH_LEN]),
            b"integration test body".to_vec(),
            placeholder,
        );
        let sig = sign_envelope(&env, keypair.private()).unwrap();
        env.signature = sig;

        // Verify.
        let bytes = serde_json::to_vec(&env).unwrap();
        let result = pipeline.verify(&bytes, recipient_id, reference).unwrap();

        let (delivered_env, delivered_sender, delivered_key) = match result {
            Verdict::Deliver {
                envelope,
                sender,
                key_record,
                already_delivered,
            } => {
                assert!(!already_delivered, "should not be already delivered");
                (envelope, sender, key_record)
            }
            Verdict::Quarantine { reason, .. } => {
                panic!("integration test must deliver, got quarantine: {reason}");
            }
        };

        assert_eq!(delivered_env.sender_id, sender_id);
        assert_eq!(delivered_env.recipient_id, recipient_id);
        assert_eq!(delivered_sender.identity_id, sender_id);
        assert_eq!(delivered_key.key_id, key_record.key_id);
    }

    // VP14: QuarantineReason Display produces machine-readable tokens.
    #[test]
    fn quarantine_reason_display_tokens() {
        assert_eq!(QuarantineReason::ParseFailure.to_string(), "parse_failure");
        assert_eq!(
            QuarantineReason::RecipientMismatch {
                expected: IdentityId::new().unwrap()
            }
            .to_string(),
            "recipient_mismatch"
        );
        assert_eq!(
            QuarantineReason::ClockSkew {
                envelope_at: now(),
                now: now()
            }
            .to_string(),
            "clock_skew"
        );
        assert_eq!(
            QuarantineReason::SenderUnknown.to_string(),
            "sender_unknown"
        );
        assert_eq!(QuarantineReason::KeyUnknown.to_string(), "key_unknown");
        assert_eq!(QuarantineReason::KeyExpired.to_string(), "key_expired");
        assert_eq!(QuarantineReason::KeyRevoked.to_string(), "key_revoked");
        assert_eq!(
            QuarantineReason::SignatureInvalid.to_string(),
            "signature_invalid"
        );
        assert_eq!(QuarantineReason::Replay.to_string(), "replay");
    }

    // VP15: VerificationError Display and source chain are non-empty.
    #[test]
    fn verification_error_display_and_source() {
        use std::error::Error as _;

        let registry_err = VerificationError::Registry(RegistryError::MissingHome);
        let msg = registry_err.to_string();
        assert!(!msg.is_empty());
        assert!(msg.contains("registry"), "Registry: {msg}");
        assert!(registry_err.source().is_some());

        let mint_err = VerificationError::MessageIdMint;
        let mint_msg = mint_err.to_string();
        assert!(!mint_msg.is_empty());
        assert!(mint_err.source().is_none());
    }

    // VP16: bytes exceeding MAX_ENVELOPE_BYTES are rejected as ParseFailure
    // before any JSON parsing occurs.
    #[test]
    fn oversize_input_yields_parse_failure() {
        let ctx = build_context();
        let oversized = vec![0u8; MAX_ENVELOPE_BYTES + 1];
        let result = ctx
            .pipeline
            .verify(&oversized, ctx.recipient_id, now())
            .unwrap();
        assert!(
            matches!(
                result,
                Verdict::Quarantine {
                    reason: QuarantineReason::ParseFailure,
                    identifying: None,
                }
            ),
            "expected ParseFailure for oversize input, got {result:?}",
        );
    }

    // VP17: a forged-signature envelope does not burn the replay slot for the
    // legitimate (sender_id, message_id, nonce) tuple. After a
    // SignatureInvalid rejection, the same envelope with a valid signature
    // must still deliver.
    #[test]
    fn forged_signature_does_not_poison_replay_slot() {
        let ctx = build_context();
        let reference = now();
        let mut env = signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
            reference,
        );

        // Tamper the signature to produce a forged-signature envelope that
        // shares the same (sender_id, message_id, nonce) as the valid one.
        let mut sig_bytes = *env.signature.as_bytes();
        sig_bytes[0] ^= 0x01;
        env.signature = EnvelopeSignature::from_bytes(sig_bytes);

        let tampered_bytes = serde_json::to_vec(&env).unwrap();
        let forged_result = ctx
            .pipeline
            .verify(&tampered_bytes, ctx.recipient_id, reference)
            .unwrap();
        assert!(
            matches!(
                forged_result,
                Verdict::Quarantine {
                    reason: QuarantineReason::SignatureInvalid,
                    ..
                }
            ),
            "expected SignatureInvalid for tampered envelope, got {forged_result:?}",
        );

        // Restore the valid signature. The replay slot must not have been
        // consumed by the forged attempt, so this must deliver. `env` still
        // carries the original content fields (message_id, nonce, sender_id),
        // so re-signing it produces a valid envelope sharing the same replay
        // key as the tampered attempt above.
        let original_bytes = {
            let mut rebuilt = env.clone();
            let valid_sig = sign_envelope(&rebuilt, ctx.keypair.private()).unwrap();
            rebuilt.signature = valid_sig;
            serde_json::to_vec(&rebuilt).unwrap()
        };

        let valid_result = ctx
            .pipeline
            .verify(&original_bytes, ctx.recipient_id, reference)
            .unwrap();
        assert!(
            matches!(valid_result, Verdict::Deliver { .. }),
            "legitimate envelope must deliver after a forged-signature attempt on the same replay key, got {valid_result:?}",
        );
    }
}
