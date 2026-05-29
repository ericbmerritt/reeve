//! Notify-based watcher actor per `specs/reeve-walking-skeleton.ladder.md`
//! phase 4 and `specs/reeve-transport-security.md` § Delivery Model + §
//! Message State Machine + § At-Least-Once Pickup with Idempotent Delivery.
//!
//! [`Watcher::process_file`] is the per-file unit of work: it reads bytes
//! with `O_NOFOLLOW`, runs them through [`VerificationPipeline::verify`],
//! then atomically moves the file to `cur/` (deliver) or `quarantine/`
//! (reject), records the delivery in the ledger, and emits the audit event.
//!
//! [`Watcher::run`] wraps the per-file logic in a `notify`-driven event loop.
//! On startup it subscribes to filesystem create events on `new/` first, then
//! scans `new/` once (crash recovery: files left from a prior run are processed
//! normally; the delivery ledger supplies idempotency). Subscribing before
//! scanning closes the window where files arriving during watcher setup could be
//! silently lost.
//!
//! ## Quarantine filename convention
//!
//! Quarantined files are renamed to `<original_stem>.<reason_token>` inside
//! `quarantine/`. Example: a file `abc123.json` with reason `signature_invalid`
//! becomes `quarantine/abc123.json.signature_invalid`. The original UUID stem
//! is preserved; the reason suffix makes quarantined files self-describing
//! without opening them.
//!
//! ## Crash recovery
//!
//! Files remaining in `new/` after a crash are picked up by the initial scan
//! in [`Watcher::run`]. The delivery ledger deduplicates any file that was
//! already moved to `cur/` before the crash; the replay ledger prevents the
//! same envelope from being re-accepted if the file is somehow re-submitted.
//!
//! ### Crash window between rename and ledger write
//!
//! In the Deliver arm, the file is moved to `cur/` *before* the delivery
//! ledger entry is written. A crash between these two steps leaves the
//! file in `cur/` with no ledger entry and no `transport.delivered`
//! audit event. On restart, the scan only re-processes `new/` — the
//! orphaned file in `cur/` is silently retained without audit. This
//! window is a known residual; future phases (Phase 4 Task 14 `cur/`
//! rotation, or a stronger ordering: ledger-then-rename) may close it.
//!
//! ## Cross-filesystem rename detection
//!
//! The spec mandates atomic moves only within the same filesystem. `rename(2)`
//! fails with `EXDEV` when source and destination span a filesystem boundary.
//! This crate surfaces that as [`WatcherError::CrossFilesystemRename`] rather
//! than attempting a fallback copy.
//!
//! ## cur/ rotation
//!
//! [`Watcher::rotate_cur`] is a periodic housekeeping operation that moves
//! files older than a caller-supplied retention threshold from `cur/` into
//! `archive/`. The decision is mtime-based; archival is silent (no audit
//! event — see the method's doc for the reasoning). The cadence is the
//! caller's responsibility; Phase 6's runtime daemon owns the timer.
//!
//! ## Sender filename contract
//!
//! Senders are expected to use collision-resistant filenames (e.g., UUIDs or
//! `<message_id>.json`). Two well-formed envelopes deposited with the same
//! filename could race in `cur/` during concurrent processing. The watcher does
//! not address this; it is a sender-side convention, not a runtime invariant.
//!
//! ## Threat model: inbox/new/ access
//!
//! The watcher reads bytes from `inbox/new/`, runs verification, then
//! atomically renames into `cur/` or `quarantine/`. Between the read and
//! the rename there is a TOCTOU window during which an attacker with
//! write access to `inbox/new/` could delete-and-replace the file: the
//! verdict in the audit log corresponds to the original bytes; the file
//! installed in `cur/` is the replacement.
//!
//! This window is closed by mode 0o700 on the agent's inbox directory:
//! only the runtime's user can write to `new/`. Group- or world-writable
//! inbox directories void the delivery integrity guarantee. The
//! [`AgentInbox`] constructor validates the mode (see [`crate::inbox`]).
//!
//! Mode 0o700 on the agent's inbox excludes other UNIX users. It does NOT
//! exclude same-UID processes (other agents the runtime supervises, rogue
//! tools running as the developer, a compromised dev shell). Same-UID
//! actors remain a residual risk that the threat model accepts. The
//! runtime relies on the UID-perimeter as its integrity boundary — a
//! future privilege-separation phase (one OS user per agent, or
//! capability-based delivery) is the intended mitigation direction.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use notify;
use reeve_types::{Envelope, IdentityId, KeyId, MessageId};
use time::{Duration, OffsetDateTime};

use tracing::{debug, error, info, warn};

use crate::audit::{AuditError, AuditEvent, AuditLog};
use crate::fs_util::set_nofollow;
use crate::identity_registry::IdentityRegistry;
use crate::inbox::AgentInbox;
use crate::ledger::{DeliveryKey, DeliveryLedger, LedgerError, ReplayLedger};
use crate::verify::{
    emit_quarantine_audit, EnvelopeIds, QuarantineReason, Verdict, VerificationError,
    VerificationPipeline, DEFAULT_CLOCK_SKEW, MAX_ENVELOPE_BYTES,
};

/// Initial buffer capacity for envelope reads — bounded above by
/// `MAX_ENVELOPE_BYTES`.
const INITIAL_READ_BUF: usize = 64 * 1024;

/// Headroom reserved for the reason-token suffix appended on quarantine.
/// The longest current token is `.signature_invalid` (18 bytes); 55 bytes
/// reserves headroom for future reason variants and inbox path components.
/// `NAME_MAX` is 255 on Linux/macOS.
///
/// If a future `QuarantineReason` variant introduces a token longer than
/// 37 bytes (55 - 18 = headroom minus current longest token), revisit this
/// constant before adding.
const QUARANTINE_SUFFIX_HEADROOM: usize = 55;

/// Maximum byte length of a filename deposited in `inbox/new/`. Names longer
/// than this are rejected before any path construction so that appending a
/// reason suffix cannot overflow the filesystem `NAME_MAX` (255 bytes on most
/// Unix filesystems). Derived as `NAME_MAX - QUARANTINE_SUFFIX_HEADROOM`.
const MAX_INBOX_FILENAME_BYTES: usize = 255 - QUARANTINE_SUFFIX_HEADROOM;

/// Reasons that a filename in `inbox/new/` is structurally invalid and cannot
/// be processed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilenameError {
    /// The filename bytes are not valid UTF-8.
    NotUtf8,
    /// The filename is empty, `.`, or `..`.
    Reserved,
    /// The filename exceeds `MAX_INBOX_FILENAME_BYTES`.
    TooLong { len: usize },
    /// The filename contains a null byte (`\0`). Null bytes terminate C strings,
    /// so a filename containing one would be silently truncated by filesystem
    /// APIs that pass paths to the kernel as C strings.
    ContainsNull,
}

impl fmt::Display for FilenameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotUtf8 => f.write_str("filename is not valid UTF-8"),
            Self::Reserved => f.write_str("filename is empty, '.', or '..'"),
            Self::TooLong { len } => write!(
                f,
                "filename is {len} bytes, exceeds {MAX_INBOX_FILENAME_BYTES}-byte limit"
            ),
            Self::ContainsNull => f.write_str("filename contains a null byte"),
        }
    }
}

impl FilenameError {
    /// Machine-readable token for use in audit JSON. Distinct from the
    /// human-readable [`Display`] impl which is used in logs and error
    /// messages.
    ///
    /// Token format is contractual and stable. Audit consumers may parse
    /// `too_long(<N>)` for the length value; the parenthesized-decimal
    /// format is part of the audit schema.
    pub(crate) fn as_token(&self) -> String {
        match self {
            Self::NotUtf8 => "not_utf8".to_owned(),
            Self::Reserved => "reserved".to_owned(),
            Self::TooLong { len } => format!("too_long({len})"),
            Self::ContainsNull => "contains_null".to_owned(),
        }
    }
}

/// Serializes as the stable audit token returned by `FilenameError::as_token`.
/// Wire format is part of the audit schema; see `as_token` for stability
/// guarantees.
impl serde::Serialize for FilenameError {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_token())
    }
}

/// The outcome of processing one file from `inbox/new/`.
#[non_exhaustive]
#[derive(Debug)]
pub enum ProcessOutcome {
    /// The file passed all checks, was moved to `cur/`, and the delivery
    /// ledger was updated (or was already present from a prior run).
    Delivered {
        message_id: MessageId,
        sender_id: IdentityId,
        sender_key_id: KeyId,
    },
    /// The file failed verification and was moved to `quarantine/`.
    Quarantined { reason: QuarantineReason },
    /// The delivery ledger already recorded this `(recipient_id, message_id)`.
    /// The file was still moved to `cur/` so `new/` stays clean.
    ///
    /// No `transport.delivered` audit event is emitted on this path — the
    /// original delivery was already audited; replaying it is intentionally
    /// silent.
    AlreadyDelivered { message_id: MessageId },
    /// The file vanished between detection and processing — typically because
    /// the scan and a notify event saw the same arrival and the earlier
    /// processor handled it. No-op: `new/` remains clean and no audit event is
    /// emitted.
    AlreadyProcessed,
    /// The filename was structurally invalid (non-UTF-8, reserved name, or
    /// too long). The file remains in `new/` for operator inspection. A
    /// `transport.filename-rejected` audit event is emitted so operators have
    /// signal that bad files are accumulating.
    InvalidFilename { reason: FilenameError },
}

/// Outcome of a [`Watcher::rotate_cur`] call: counts of what happened to each
/// entry in `cur/` during the rotation pass.
///
/// No audit event is emitted by `rotate_cur`; see the method's
/// `## No audit event` section for the rationale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RotationOutcome {
    /// Number of files moved from `cur/` to `archive/` (aged past retention).
    pub archived: usize,
    /// Number of files left in `cur/` (not yet old enough).
    pub retained: usize,
    /// Number of entries skipped: symlinks, non-regular-files, or dotfiles.
    pub skipped: usize,
}

/// Errors that stop or degrade the watcher's ability to operate. System
/// failures — not pipeline verdicts — live here. A `Quarantine` verdict is
/// `Ok(ProcessOutcome::Quarantined { .. })`, never an error.
#[non_exhaustive]
#[derive(Debug)]
pub enum WatcherError {
    /// The `notify` filesystem watch could not be established.
    Notify(notify::Error),
    /// The verification infrastructure failed (registry, ledger, audit).
    Verification(VerificationError),
    /// A file I/O operation failed (read, rename).
    Io { path: PathBuf, source: io::Error },
    /// The audit log could not be appended to.
    Audit(AuditError),
    /// A ledger write failed.
    Ledger(LedgerError),
    /// The source and destination span a filesystem boundary; atomic rename is
    /// impossible. Per spec § Filesystem Safety: "atomic moves only within the
    /// same filesystem".
    CrossFilesystemRename { from: PathBuf, to: PathBuf },
}

impl fmt::Display for WatcherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Notify(source) => write!(f, "watcher: notify watch error: {source}"),
            Self::Verification(source) => {
                write!(f, "watcher: verification infrastructure: {source}")
            }
            Self::Io { path, source } => {
                write!(f, "watcher: IO at {}: {source}", path.display())
            }
            Self::Audit(source) => write!(f, "watcher: audit log: {source}"),
            Self::Ledger(source) => write!(f, "watcher: ledger: {source}"),
            Self::CrossFilesystemRename { from, to } => write!(
                f,
                "watcher: cannot atomically rename across filesystems: {} -> {}",
                from.display(),
                to.display(),
            ),
        }
    }
}

impl std::error::Error for WatcherError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Notify(source) => Some(source),
            Self::Verification(source) => Some(source),
            Self::Io { source, .. } => Some(source),
            Self::Audit(source) => Some(source),
            Self::Ledger(source) => Some(source),
            Self::CrossFilesystemRename { .. } => None,
        }
    }
}

/// Stateless watcher type. All per-file processing is driven through
/// [`Watcher::process_file`]; [`Watcher::run`] wraps it in a `notify` loop.
///
/// `Clone` is intentionally not derived. The watcher is constructed once and
/// either run directly or wrapped in an `Arc`. Phase 6 wires it into the
/// actix supervisor tree.
pub struct Watcher {
    /// Verification pipeline: runs all envelope checks and returns a verdict.
    pipeline: Arc<VerificationPipeline>,
    /// Held separately so [`process_file`] can call [`DeliveryLedger::record`]
    /// after a successful rename — the pipeline only exposes `verify`, not the
    /// ledger's write path.
    delivery: Arc<DeliveryLedger>,
    /// Audit log: receives `transport.delivered` and `transport.quarantine`
    /// events after each file is processed.
    audit: Arc<AuditLog>,
    /// Path to the on-disk agent registry. Used to distinguish
    /// [`crate::verify::QuarantineReason::RecipientMismatch`] (agent is
    /// registered but addressed to the wrong inbox) from
    /// [`crate::verify::QuarantineReason::RecipientNotFound`] (no agent with
    /// that identity is registered on this host). When the registry is
    /// unreadable the watcher falls back to treating all recipients as known,
    /// producing `RecipientMismatch` rather than risking a mislabeled
    /// quarantine.
    agent_registry_path: PathBuf,
    /// Live routing table: `identity_id` → actix recipient that accepts
    /// [`ProcessInbound`] messages. Populated by [`Watcher::register_route`];
    /// consulted in the `Deliver` arm of [`Watcher::process_file`].
    routing_table: Arc<RwLock<HashMap<IdentityId, actix::Recipient<crate::agent::ProcessInbound>>>>,
}

impl Watcher {
    /// Construct a watcher.
    ///
    /// Uses [`DEFAULT_CLOCK_SKEW`] for the verification pipeline. Phase 6 can
    /// expose configurable skew tolerance if needed; for the walking skeleton
    /// the default is appropriate.
    // NOTE: deferred — see specs/reeve-walking-skeleton.ladder.md (Phase 4
    // Task 14 — cur/ rotation). cur/, quarantine/, and archive/ directory
    // modes are validated at InboxLayout::provision but not re-checked when
    // Watcher::new acquires an AgentInbox handle. A post-provision chmod
    // could silently weaken the integrity boundary.
    pub fn new(
        registry: &Arc<IdentityRegistry>,
        replay: &Arc<ReplayLedger>,
        delivery: Arc<DeliveryLedger>,
        audit: Arc<AuditLog>,
        agent_registry_path: PathBuf,
    ) -> Self {
        let pipeline = Arc::new(VerificationPipeline::new(
            Arc::clone(registry),
            Arc::clone(replay),
            Arc::clone(&delivery),
            DEFAULT_CLOCK_SKEW,
        ));
        Self {
            pipeline,
            delivery,
            audit,
            agent_registry_path,
            routing_table: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Register a live agent recipient for routing. After registration,
    /// verified envelopes addressed to `agent_id` are dispatched to
    /// `recipient` instead of being held in `cur/` for deferred pickup.
    pub fn register_route(
        &self,
        agent_id: IdentityId,
        recipient: actix::Recipient<crate::agent::ProcessInbound>,
    ) {
        self.routing_table
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(agent_id, recipient);
    }

    /// Returns `true` when a route is registered for `id`.
    #[cfg(test)]
    pub(crate) fn has_route(&self, id: IdentityId) -> bool {
        self.routing_table
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&id)
    }

    /// Send `envelope` to its registered actix recipient, if one is present in
    /// the routing table. Logs a warning when no entry is found; the file
    /// remains in `cur/` and will be dispatched on the next
    /// `scan_cur_and_dispatch` call (e.g. crash-recovery).
    ///
    /// `payload` must be the UTF-8 decoded body. Callers are responsible for
    /// ensuring the body is valid UTF-8 before calling this; the guard in
    /// [`Watcher::process_file`] upholds this invariant on the normal path.
    fn dispatch_envelope(&self, envelope: &Envelope, payload: String) {
        let recipient = self
            .routing_table
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&envelope.recipient_id)
            .cloned();
        if let Some(r) = recipient {
            let message_id = envelope.message_id.to_string();
            r.do_send(crate::agent::ProcessInbound {
                payload,
                message_id,
                sender_id: envelope.sender_id,
            });
        } else {
            warn!(
                recipient_id = %envelope.recipient_id,
                message_id = %envelope.message_id,
                "no routing entry for recipient; message in cur/ awaits crash-recovery"
            );
        }
    }

    /// Watch `inbox/new/` for `agent_id`, processing each arriving file.
    ///
    /// Subscribes to `notify` create events on `new/` first, then performs an
    /// initial scan to recover files left by a prior crash. Subscribing before
    /// scanning ensures no file is missed in the window between scan and watch
    /// setup.
    ///
    /// Processing is sequential within the agent: one file at a time, in
    /// arrival order.
    ///
    /// Returns when the `notify` watcher's internal channel is closed (which
    /// happens when `_watcher` is dropped — the caller owns the shutdown signal
    /// by holding or dropping that handle). A fatal infrastructure error also
    /// causes an early return.
    ///
    /// Phase 6 replaces this with a tokio task and a `CancellationToken`;
    /// for the walking skeleton a blocking loop is sufficient.
    pub fn run(
        &self,
        agent_id: IdentityId,
        inbox: &AgentInbox,
        on_quarantine: impl Fn(String) + Send,
    ) -> Result<(), WatcherError> {
        match self.run_inner(agent_id, inbox, &on_quarantine) {
            Ok(()) => Ok(()),
            Err(e) => {
                error!(err = %e, "watcher loop error");
                Err(e)
            }
        }
    }

    fn run_inner(
        &self,
        agent_id: IdentityId,
        inbox: &AgentInbox,
        on_quarantine: &(impl Fn(String) + Send),
    ) -> Result<(), WatcherError> {
        // Crash-recovery scan: picks up files left from a prior run.
        debug!("scanning inbox/new/ for existing files");
        self.scan_new(agent_id, inbox, on_quarantine)?;

        // Primary delivery is handled by the 2-second polling interval in
        // WatcherActor (supervisor.rs). kqueue/FSEvents is not used here
        // because the notify kqueue backend on macOS silently dies when a
        // second concurrent caller (the poll interval) renames files through
        // `inbox/new/`, and FSEvents does not reliably fire for cross-directory
        // renames (`inbox/tmp/ → inbox/new/`) regardless. Polling at 2 s is
        // fast enough for interactive use and avoids all of these failure modes.
        //
        // Return immediately so the spawn_blocking thread exits cleanly;
        // WatcherActor owns the ongoing delivery loop.
        Ok(())
    }

    /// Process one file at `path` in `inbox/new/`.
    ///
    /// Validates the filename first. If the filename is structurally invalid
    /// (non-UTF-8, reserved, or too long), emits a
    /// `transport.filename-rejected` audit event and returns
    /// `Ok(ProcessOutcome::InvalidFilename { .. })`, leaving the file in
    /// `new/` for operator inspection.
    ///
    /// The outcome for valid filenames determines the file's destination:
    /// `Delivered` / `AlreadyDelivered` moves to `cur/`; `Quarantined` moves
    /// to `quarantine/<filename>.<reason>`; `AlreadyProcessed` is a no-op.
    ///
    /// On a system error (`Err`) the file stays in `new/` — the caller
    /// decides whether to retry, escalate, or abort. No audit event is
    /// emitted for system errors. For the Deliver arm, the verdict has not
    /// been ledger-recorded; for the Quarantine arm, the verdict is reached
    /// but the audit event is conditional on a successful rename per the
    /// spec's audit-after-durable-rename mandate.
    pub fn process_file(
        &self,
        path: &Path,
        agent_id: IdentityId,
        inbox: &AgentInbox,
        is_known_recipient: impl Fn(IdentityId) -> bool,
    ) -> Result<ProcessOutcome, WatcherError> {
        // path.file_name() returns None for paths ending in ".." or "/".
        // These shapes are unreachable from read_dir/notify CreateKind::File,
        // but they are callers-eye-valid paths that name no file — Reserved is
        // the correct rejection reason (same as an empty or "." name).
        let Some(raw_name) = path.file_name() else {
            return self.reject_filename(agent_id, FilenameError::Reserved);
        };
        let validated_name: &str = match validate_filename(raw_name) {
            Ok(name) => name,
            Err(reason) => return self.reject_filename(agent_id, reason),
        };
        let bytes = match read_nofollow(path) {
            Ok(b) => b,
            Err(WatcherError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                return Ok(ProcessOutcome::AlreadyProcessed);
            }
            Err(e) => return Err(e),
        };
        let now = OffsetDateTime::now_utc();
        let verdict = self
            .pipeline
            .verify(&bytes, agent_id, is_known_recipient, now)
            .map_err(WatcherError::Verification)?;
        match verdict {
            Verdict::Deliver {
                envelope,
                sender,
                key_record,
                already_delivered,
            } => {
                // Guard: body must be valid UTF-8 before the file moves to cur/.
                // Non-UTF-8 bodies are quarantined here so dispatch_envelope
                // and scan_cur_and_dispatch never receive bytes that cannot
                // be converted to a String payload.
                if std::str::from_utf8(&envelope.body).is_err() {
                    let reason = QuarantineReason::BodyNotUtf8;
                    let ids = EnvelopeIds::from_envelope(&envelope);
                    let dest = quarantine_path(inbox.quarantine(), validated_name, &reason);
                    if !rename_disambiguating_enoent(path, &dest)? {
                        return Ok(ProcessOutcome::AlreadyProcessed);
                    }
                    emit_quarantine_audit(&self.audit, &reason, Some(&ids), agent_id, now)
                        .map_err(WatcherError::Verification)?;
                    warn!(
                        message_id = %envelope.message_id,
                        "quarantined: envelope body is not valid UTF-8"
                    );
                    return Ok(ProcessOutcome::Quarantined { reason });
                }
                self.handle_deliver(
                    path,
                    validated_name,
                    inbox,
                    agent_id,
                    now,
                    &envelope,
                    &sender,
                    &key_record,
                    already_delivered,
                )
            }
            Verdict::Quarantine {
                reason,
                identifying,
            } => {
                let dest = quarantine_path(inbox.quarantine(), validated_name, &reason);
                if !rename_disambiguating_enoent(path, &dest)? {
                    return Ok(ProcessOutcome::AlreadyProcessed);
                }
                emit_quarantine_audit(&self.audit, &reason, identifying.as_ref(), agent_id, now)
                    .map_err(WatcherError::Verification)?;
                warn!(reason = %reason, "quarantined");
                Ok(ProcessOutcome::Quarantined { reason })
            }
        }
    }

    /// Open the agent registry and collect all registered `identity_id` values
    /// into a set.
    ///
    /// Returns `Some(set)` on success and `None` when the registry cannot be
    /// opened. `None` causes the caller to treat every recipient as known —
    /// the conservative fallback that produces `RecipientMismatch` rather than
    /// `RecipientNotFound`, avoiding a mislabeled quarantine reason when the
    /// registry is transiently unreadable.
    fn snapshot_known_recipients(&self) -> Option<HashSet<IdentityId>> {
        match crate::agent_registry::AgentRegistry::open(self.agent_registry_path.clone()) {
            Ok(r) => Some(r.list().map(|rec| rec.identity_id).collect()),
            Err(e) => {
                warn!(
                    path = %self.agent_registry_path.display(),
                    err = %e,
                    "agent registry unavailable; assuming recipient may be known"
                );
                None
            }
        }
    }

    /// Handle a [`Verdict::Deliver`] from the verification pipeline.
    ///
    /// Moves the file to `cur/`, records delivery in the ledger, appends the
    /// audit event, and dispatches [`crate::agent::ProcessInbound`] to the
    /// registered recipient. Returns [`ProcessOutcome::AlreadyDelivered`]
    /// without writing to the ledger when the message was already recorded.
    /// The file is moved to `cur/` before the already-delivered check, so
    /// already-delivered messages remain in `cur/` for crash-recovery replay.
    #[expect(
        clippy::too_many_arguments,
        reason = "Verdict::Deliver fields passed directly; a one-use wrapper struct would add indirection without benefit"
    )]
    #[expect(
        clippy::expect_used,
        reason = "body is UTF-8 by invariant enforced in process_file; panicking is correct \
                  because a ghost delivery (ledger + audit say delivered but agent gets nothing) \
                  is worse than a crash"
    )]
    fn handle_deliver(
        &self,
        path: &Path,
        validated_name: &str,
        inbox: &AgentInbox,
        agent_id: IdentityId,
        now: OffsetDateTime,
        envelope: &Envelope,
        sender: &reeve_types::Identity,
        key_record: &reeve_types::KeyRecord,
        already_delivered: bool,
    ) -> Result<ProcessOutcome, WatcherError> {
        let dest = inbox.cur().join(validated_name);
        if !rename_disambiguating_enoent(path, &dest)? {
            return Ok(ProcessOutcome::AlreadyProcessed);
        }
        if already_delivered {
            debug!(message_id = %envelope.message_id, "already delivered; skipping dispatch");
            return Ok(ProcessOutcome::AlreadyDelivered {
                message_id: envelope.message_id,
            });
        }
        let payload = String::from_utf8(envelope.body.clone())
            .expect("body is UTF-8 by invariant: process_file guard quarantines non-UTF-8 bodies");
        self.delivery
            .record(
                DeliveryKey {
                    recipient_id: agent_id,
                    message_id: envelope.message_id,
                },
                now,
            )
            .map_err(WatcherError::Ledger)?;
        self.audit
            .append(&AuditEvent::TransportDelivered {
                sender_id: sender.identity_id,
                sender_key_id: key_record.key_id,
                recipient_id: agent_id,
                message_id: envelope.message_id,
                at: now,
            })
            .map_err(WatcherError::Audit)?;
        info!(
            message_id = %envelope.message_id,
            sender_id = %sender.identity_id,
            "delivered"
        );
        self.dispatch_envelope(envelope, payload);
        Ok(ProcessOutcome::Delivered {
            message_id: envelope.message_id,
            sender_id: sender.identity_id,
            sender_key_id: key_record.key_id,
        })
    }

    /// Emit a `transport.filename-rejected` audit event and return
    /// `Ok(ProcessOutcome::InvalidFilename)`.
    ///
    /// Fires INSTEAD OF `transport.quarantine` (file is not moved to
    /// `quarantine/`); the file stays in `new/` for operator inspection.
    /// Operators should alert on accumulation of this event because the
    /// runtime cannot self-clean these files: each scan pass will re-emit
    /// the audit event for the same file. This is the audit boundary
    /// for filenames that fail validation BEFORE any pipeline I/O.
    fn reject_filename(
        &self,
        agent_id: IdentityId,
        reason: FilenameError,
    ) -> Result<ProcessOutcome, WatcherError> {
        self.audit
            .append(&AuditEvent::TransportFilenameRejected {
                agent_id,
                reason: reason.clone(),
                at: OffsetDateTime::now_utc(),
            })
            .map_err(WatcherError::Audit)?;
        Ok(ProcessOutcome::InvalidFilename { reason })
    }

    /// Rotate aged files from `inbox/cur/` to `inbox/archive/`.
    ///
    /// Iterates every entry in `cur/` and, for each regular non-dotfile,
    /// compares the file's mtime against `now`. If `now - mtime >= retention`
    /// the file is atomically moved to `archive/<filename>` via
    /// `rename_disambiguating_enoent`. Files younger than the threshold are
    /// left in place. If a file vanishes between enumeration and rename
    /// (concurrent mover), it is silently not counted in archived.
    ///
    /// Symlinks, non-regular-file entries, and dotfiles (names beginning with
    /// `.`) are skipped and counted in [`RotationOutcome::skipped`].
    ///
    /// `now` is injected so tests are not dependent on the wall clock. Phase 6
    /// (the runtime daemon) will supply `OffsetDateTime::now_utc()` and choose
    /// the operational `retention` value; this method is the pure mechanism.
    ///
    /// `retention` must be non-negative. A NEGATIVE value triggers a
    /// debug-build panic via `debug_assert!`; in release builds, a negative
    /// retention archives every file in `cur/` (including any with mtimes
    /// in the future) — silently. A zero retention is accepted and archives
    /// every file whose `mtime <= now`; files with future mtimes (e.g.,
    /// from clock skew) are RETAINED. Phase 6 callers should validate
    /// retention upstream of this method.
    ///
    /// ## No audit event
    ///
    /// Archival is housekeeping, not a security event. No `AuditEvent` variant
    /// is emitted here — operators observe ground truth via file presence in
    /// `archive/`. This is deliberate per the Phase 4 Task 14 spec.
    ///
    /// ## Collision behaviour
    ///
    /// If `archive/<filename>` already exists, the atomic rename will overwrite
    /// it. This matches the `cur/`-collision behaviour documented in the watcher
    /// module comment; no additional collision handling is added here.
    ///
    /// ## Dotfile accumulation hazard
    ///
    /// `rotate_cur` skips dotfiles (names beginning with `.`). The watcher
    /// does not currently reject dotfile names at the `validate_filename`
    /// boundary (only `.` and `..` are reserved). A legitimate sender that
    /// uses a dotfile name (e.g., `.gitkeep`) will deliver the file to
    /// `cur/` where it can never be archived by this method. Operator-
    /// initiated cleanup is required. A future filename-policy refinement
    /// could close this by treating dotfile names as `FilenameError::Reserved`
    /// at the inbox boundary.
    ///
    /// # Threat-model notes
    ///
    /// `mtime` is not a tamper-resistant signal. A same-UID actor with write
    /// access to `cur/` can call `utimensat` (or `touch`) to either suppress
    /// rotation (mtime forward) or force premature archival (mtime backward).
    /// This is consistent with the watcher's existing same-UID residual risk
    /// (see module-level threat model). Already-delivered messages still
    /// reside in `archive/`; the agent's runtime view of `cur/` may be
    /// truncated faster than expected. The runtime accepts this residual.
    ///
    /// Same-UID actors can also leverage the mtime-force vector together
    /// with archive overwrite: by setting mtime backwards on a `cur/` file
    /// whose name matches an existing `archive/<filename>`, an attacker
    /// triggers a rename that silently overwrites the prior archived bytes.
    /// This is the documented overwrite behavior (see "If `archive/...`
    /// already exists" below); the mtime-force vector amplifies it from
    /// "rare collision" to "deliberate evidence destruction" in the
    /// adversarial case. Same-UID residual remains the boundary.
    ///
    /// # Serialization
    ///
    /// Concurrent `rotate_cur` calls against the same `(inbox)` are safe
    /// (POSIX rename is atomic; the `Ok(false)` benign-race path is handled),
    /// but archived counters may be inflated under concurrency. Phase 6
    /// callers should serialize per-agent rotation passes (e.g., one timer
    /// per agent in the actor tree).
    pub fn rotate_cur(
        &self,
        inbox: &AgentInbox,
        retention: Duration,
        now: OffsetDateTime,
    ) -> Result<RotationOutcome, WatcherError> {
        debug_assert!(
            retention >= Duration::ZERO,
            "retention must be non-negative"
        );
        let mut outcome = RotationOutcome {
            archived: 0,
            retained: 0,
            skipped: 0,
        };

        let entries = fs::read_dir(inbox.cur()).map_err(|source| WatcherError::Io {
            path: inbox.cur().to_path_buf(),
            source,
        })?;

        for entry_result in entries {
            let entry = entry_result.map_err(|source| WatcherError::Io {
                path: inbox.cur().to_path_buf(),
                source,
            })?;
            let path = entry.path();

            // Dotfiles are skipped unconditionally.
            let file_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) if n.starts_with('.') => {
                    outcome.skipped += 1;
                    continue;
                }
                Some(n) => n.to_owned(),
                None => {
                    outcome.skipped += 1;
                    continue;
                }
            };

            // Use symlink_metadata so we never follow a symlink.
            let metadata = fs::symlink_metadata(&path).map_err(|source| WatcherError::Io {
                path: path.clone(),
                source,
            })?;

            if metadata.file_type().is_symlink() || !metadata.is_file() {
                outcome.skipped += 1;
                continue;
            }

            // Extract the mtime. On platforms where modified() can fail we
            // surface that as WatcherError::Io so the caller sees it rather
            // than silently retaining or archiving the file.
            let mtime_system = metadata.modified().map_err(|source| WatcherError::Io {
                path: path.clone(),
                source,
            })?;

            // Convert SystemTime → OffsetDateTime. The conversion cannot
            // fail on any platform that supports SystemTime (since UNIX_EPOCH
            // to now is always representable), but the API returns a Result.
            let mtime = OffsetDateTime::from(mtime_system);
            let age = now - mtime;

            if age >= retention {
                let dest = inbox.archive().join(&file_name);
                if rename_disambiguating_enoent(&path, &dest)? {
                    outcome.archived += 1;
                }
                // else: file already moved by a concurrent actor — not counted.
            } else {
                outcome.retained += 1;
            }
        }

        Ok(outcome)
    }

    /// Scan `inbox/cur/` and dispatch [`crate::agent::ProcessInbound`] for
    /// each envelope found. Called by [`crate::supervisor::WatcherActor`]
    /// when registering a new inbox, so envelopes deposited in `cur/` during
    /// a prior run are delivered on restart.
    ///
    /// `agent_id` is compared against `envelope.recipient_id` for each file.
    /// Files whose `recipient_id` does not match `agent_id` are skipped with a
    /// warning — defense-in-depth against stray files in `cur/`.
    ///
    /// Files in `cur/` have already passed the full verification pipeline.
    /// This function dispatches them without re-verifying — callers must ensure
    /// that `cur/` is only written by [`Watcher::handle_deliver`] after a
    /// successful [`crate::verify::VerificationPipeline::verify`] call (and
    /// the UTF-8 body guard that precedes it in [`Watcher::process_file`]).
    ///
    /// **At-least-once semantics.** This function intentionally re-dispatches
    /// every file in `cur/` without consulting the delivery ledger. Files in
    /// `cur/` may have been dispatched to the agent before a crash, but the
    /// agent may not have processed them. Adding a ledger guard here would
    /// silently skip messages that were dispatched-but-not-processed,
    /// violating crash-recovery guarantees.
    ///
    /// The correct cleanup mechanism is `rotate_cur`, which archives verified
    /// messages from `cur/` after the configured retention period.
    ///
    /// Routes through [`Watcher::dispatch_envelope`] — the single
    /// `ProcessInbound` dispatch gateway per CLAUDE.md and
    /// `docs/decisions/001-single-processInbound-dispatch.md`.
    pub(crate) fn scan_cur_and_dispatch(&self, inbox: &AgentInbox, agent_id: IdentityId) {
        let cur = inbox.cur();
        let Ok(entries) = fs::read_dir(cur) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                debug!(path = %path.display(), "skipping symlink in cur/");
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            if let Some((envelope, payload)) = Self::read_cur_payload(&path) {
                if envelope.recipient_id != agent_id {
                    warn!(
                        path = %path.display(),
                        envelope_recipient_id = %envelope.recipient_id,
                        inbox_agent_id = %agent_id,
                        "cur/ envelope recipient mismatch; skipping dispatch",
                    );
                    continue;
                }
                self.dispatch_envelope(&envelope, payload);
            }
        }
    }

    /// Read an envelope file from `cur/` and return the full [`Envelope`] plus
    /// the UTF-8–decoded body payload.
    ///
    /// Opens with `O_NOFOLLOW`. Returns `None` when the file cannot be read or
    /// parsed. Logs a warning and returns `None` when the body is not valid
    /// UTF-8 so operators can detect files stuck in `cur/`.
    fn read_cur_payload(path: &Path) -> Option<(Envelope, String)> {
        let buf = read_nofollow(path).ok()?;
        let mut envelope: Envelope = serde_json::from_slice(&buf).ok()?;
        let body = std::mem::take(&mut envelope.body);
        if let Ok(payload) = String::from_utf8(body) {
            Some((envelope, payload))
        } else {
            warn!(
                path = %path.display(),
                message_id = %envelope.message_id,
                "cur/ envelope body is not valid UTF-8; skipping crash-recovery dispatch"
            );
            None
        }
    }

    /// Periodic fallback scan of `inbox/new/` — called from the housekeeping
    /// ticker in [`crate::supervisor::WatcherActor`] to recover messages whose
    /// `FSEvents` notification was dropped by the OS.
    ///
    /// Uses a no-op quarantine callback because quarantine events are emitted
    /// inside `process_file`; this path does not have access to the
    /// per-agent quarantine channel established at inbox-start time. Messages
    /// that would be quarantined are still moved to `inbox/quarantine/` by
    /// the pipeline; the operator learns about them from the audit log and
    /// the TUI quarantine screen rather than from the per-agent callback.
    ///
    /// Errors are swallowed — this is a best-effort background sweep and must
    /// not bring down the watcher actor.
    pub(crate) fn scan_new_fallback(&self, inbox: &AgentInbox, agent_id: IdentityId) {
        if let Err(err) = self.scan_new(agent_id, inbox, &|_| {}) {
            tracing::debug!(%err, "scan_new_fallback: error during periodic rescan");
        }
    }

    /// Iterates `inbox/new/`, skipping symlinks and non-files. Calls
    /// [`Watcher::process_file`] on each regular file. Stops on the first
    /// system error; returns `Ok(())` after a complete pass.
    ///
    /// Calls [`Watcher::snapshot_known_recipients`] once before the loop so
    /// the registry is opened at most once per pass, not once per file.
    fn scan_new(
        &self,
        agent_id: IdentityId,
        inbox: &AgentInbox,
        on_quarantine: &(impl Fn(String) + Send),
    ) -> Result<(), WatcherError> {
        let known = self.snapshot_known_recipients();
        let entries = fs::read_dir(inbox.new_dir()).map_err(|source| WatcherError::Io {
            path: inbox.new_dir().to_path_buf(),
            source,
        })?;
        for entry_result in entries {
            let entry = entry_result.map_err(|source| WatcherError::Io {
                path: inbox.new_dir().to_path_buf(),
                source,
            })?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path).map_err(|source| WatcherError::Io {
                path: path.clone(),
                source,
            })?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                continue;
            }
            let outcome = self.process_file(&path, agent_id, inbox, |rid| {
                known.as_ref().is_none_or(|s| s.contains(&rid))
            })?;
            if let ProcessOutcome::Quarantined { reason } = outcome {
                on_quarantine(reason.to_string());
            }
        }
        Ok(())
    }
}

/// Read `path` with `O_NOFOLLOW`, enforcing [`MAX_ENVELOPE_BYTES`].
///
/// Symlinks surface as an `Io` error (`ELOOP` on Unix). Files larger than the
/// cap are read up to the cap; the pipeline then rejects them as
/// `ParseFailure`.
fn read_nofollow(path: &Path) -> Result<Vec<u8>, WatcherError> {
    use std::fs::OpenOptions;
    use std::io::Read;

    let mut options = OpenOptions::new();
    options.read(true);
    set_nofollow(&mut options);
    let mut file = options.open(path).map_err(|source| WatcherError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    // Read up to one byte past the cap so the pipeline can detect oversize
    // payloads and return ParseFailure rather than a truncated parse.
    let cap = MAX_ENVELOPE_BYTES + 1;
    let mut buf = Vec::with_capacity(cap.min(INITIAL_READ_BUF));
    file.by_ref()
        .take(cap.try_into().unwrap_or(u64::MAX))
        .read_to_end(&mut buf)
        .map_err(|source| WatcherError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    Ok(buf)
}

/// Rename `from` to `to`, detecting cross-filesystem moves.
///
/// `rename(2)` returns `EXDEV` when source and destination are on different
/// filesystems. This is surfaced as [`WatcherError::CrossFilesystemRename`]
/// rather than silently falling back to a copy.
#[cfg(unix)]
fn atomic_rename(from: &Path, to: &Path) -> Result<(), WatcherError> {
    fs::rename(from, to).map_err(|source| {
        if source.raw_os_error() == Some(libc::EXDEV) {
            WatcherError::CrossFilesystemRename {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
            }
        } else {
            WatcherError::Io {
                path: from.to_path_buf(),
                source,
            }
        }
    })
}

/// Rename `from` to `to`.
///
/// On non-Unix platforms `EXDEV` detection is not available; all rename
/// failures are surfaced as [`WatcherError::Io`]. The cross-filesystem guard
/// is documented as a Unix-only guarantee.
#[cfg(not(unix))]
fn atomic_rename(from: &Path, to: &Path) -> Result<(), WatcherError> {
    fs::rename(from, to).map_err(|source| WatcherError::Io {
        path: from.to_path_buf(),
        source,
    })
}

/// Rename `from` to `to`, disambiguating the two distinct `NotFound` cases.
///
/// Returns `Ok(true)` when the rename succeeded.
///
/// Returns `Ok(false)` when `NotFound` is returned AND `from` no longer
/// exists — the benign race where an earlier processor already moved the
/// file. The caller should return `Ok(AlreadyProcessed)`.
///
/// Returns `Err(WatcherError::Io)` when `NotFound` is returned but `from`
/// still exists — the destination directory (e.g. `cur/` or `quarantine/`)
/// has been removed, which is an infrastructure failure and must be
/// surfaced as an error rather than silently discarded.
///
/// `pub(crate)` visibility is for testability: the symlink-vs-missing
/// disambiguation branch is unreachable through `process_file` (which
/// rejects symlinks via `O_NOFOLLOW` before the rename) but is directly
/// testable at this level.
pub(crate) fn rename_disambiguating_enoent(from: &Path, to: &Path) -> Result<bool, WatcherError> {
    match atomic_rename(from, to) {
        Ok(()) => Ok(true),
        Err(WatcherError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            // symlink_metadata, not exists, so a dangling symlink at the
            // source still surfaces dest-dir-gone correctly.
            if from.symlink_metadata().is_ok() {
                Err(WatcherError::Io {
                    path: to.parent().unwrap_or(to).to_path_buf(),
                    source,
                })
            } else {
                Ok(false)
            }
        }
        Err(e) => Err(e),
    }
}

/// Destination path in `quarantine/` for a rejected file.
///
/// Convention: `quarantine/<validated_name>.<reason_token>`. Preserves the
/// original UUID stem (useful for correlation) while making the reason visible
/// without opening the file.
fn quarantine_path(
    quarantine_dir: &Path,
    validated_name: &str,
    reason: &QuarantineReason,
) -> PathBuf {
    let quarantine_name = format!("{validated_name}.{reason}");
    quarantine_dir.join(quarantine_name)
}

/// Validate a filename deposited in `inbox/new/`.
///
/// Note: the no-filename case (`path.file_name() == None`, i.e. paths ending
/// in `/` or `..`) is co-handled by [`Watcher::process_file`] before this
/// function is called: it returns `InvalidFilename { Reserved }` directly.
/// The `Reserved` arm here handles filenames that *are* present but equal
/// `""`, `"."`, or `".."`.
///
/// Each constraint has a security rationale:
/// - UTF-8 is required so that path construction is safe (path APIs
///   accept `&str` directly without encoding negotiation).
/// - Null-byte rejection prevents C-string truncation by filesystem APIs
///   that pass paths to the kernel as null-terminated strings; without
///   this, attacker-supplied names like `"abc\0evil"` would be silently
///   truncated to `"abc"` by some bindings.
/// - Empty, `.`, and `..` are rejected so the path cannot escape the
///   inbox or refer to the directory itself.
/// - Length is bounded so the rename destination fits `NAME_MAX` after
///   the reason-token suffix is appended.
fn validate_filename(name: &OsStr) -> Result<&str, FilenameError> {
    let s = name.to_str().ok_or(FilenameError::NotUtf8)?;
    if s.contains('\0') {
        return Err(FilenameError::ContainsNull);
    }
    if s.is_empty() || s == "." || s == ".." {
        return Err(FilenameError::Reserved);
    }
    let len = s.len();
    if len > MAX_INBOX_FILENAME_BYTES {
        return Err(FilenameError::TooLong { len });
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use actix::{Actor, Context, Handler};
    use reeve_types::IdentityId;
    use tempfile::tempdir;
    use time::OffsetDateTime;

    use crate::audit::AuditLog;
    use crate::identity_registry::{IdentityRegistry, StoredIdentity};
    use crate::inbox::InboxLayout;
    use crate::ledger::{DeliveryKey, DeliveryLedger, ReplayLedger};

    struct TestCtx {
        watcher: Watcher,
        keypair: reeve_types::Keypair,
        sender_id: IdentityId,
        key_record: reeve_types::KeyRecord,
        recipient_id: IdentityId,
        inbox: AgentInbox,
        audit_dir: tempfile::TempDir,
        /// Keeps the inbox data directory alive for the test's duration.
        _inbox_data_dir: tempfile::TempDir,
        /// Keeps the agent registry data directory alive for the test's duration.
        _registry_data_dir: tempfile::TempDir,
    }

    fn build_ctx() -> TestCtx {
        use reeve_types::{Identity, KeyRecord, Keypair};

        let reg_dir = crate::test_support::secure_dir();
        let replay_dir = crate::test_support::secure_dir();
        let delivery_dir = crate::test_support::secure_dir();
        let audit_data_dir = tempdir().unwrap();
        let inbox_data_dir = crate::test_support::secure_dir();
        let registry_data_dir = crate::test_support::secure_dir();

        let registry = Arc::new(IdentityRegistry::open(reg_dir.keep()).unwrap());
        let replay = Arc::new(ReplayLedger::open(replay_dir.keep()).unwrap());
        let delivery = Arc::new(DeliveryLedger::open(delivery_dir.keep()).unwrap());
        let audit = Arc::new(AuditLog::open(audit_data_dir.path().to_path_buf()).unwrap());

        let keypair = Keypair::generate();
        let identity = Identity::new_operator("test-sender".to_owned()).unwrap();
        let sender_id = identity.identity_id;
        let key_record = KeyRecord::new(sender_id, *keypair.public()).unwrap();
        let stored = StoredIdentity::new(identity, key_record.clone()).unwrap();
        registry.write(&stored).unwrap();

        let recipient_id = IdentityId::new().unwrap();

        let layout = InboxLayout::open(inbox_data_dir.path().to_path_buf()).unwrap();
        let inbox = layout.provision(recipient_id).unwrap();

        // The agent registry is empty — tests that exercise RecipientMismatch
        // register the recipient_id explicitly before calling process_file.
        let agent_registry_path = registry_data_dir.path().join("registry.toml");
        let watcher = Watcher::new(
            &registry,
            &replay,
            Arc::clone(&delivery),
            Arc::clone(&audit),
            agent_registry_path,
        );

        TestCtx {
            watcher,
            keypair,
            sender_id,
            key_record,
            recipient_id,
            inbox,
            audit_dir: audit_data_dir,
            _inbox_data_dir: inbox_data_dir,
            _registry_data_dir: registry_data_dir,
        }
    }

    fn place_in(dir: &Path, filename: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(filename);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn place_in_new(inbox: &AgentInbox, filename: &str, bytes: &[u8]) -> PathBuf {
        place_in(inbox.new_dir(), filename, bytes)
    }

    fn audit_lines(audit_dir: &tempfile::TempDir) -> Vec<serde_json::Value> {
        let log_path = audit_dir.path().join("audit").join("log.jsonl");
        let Ok(content) = fs::read_to_string(&log_path) else {
            return Vec::new();
        };
        content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Register an agent record in the watcher's agent registry.
    ///
    /// Used by tests that need a known recipient in the registry before calling
    /// `process_file` or `scan_cur_and_dispatch`.
    fn register_in_ctx_registry(ctx: &TestCtx, name: &str, id: IdentityId, inbox_dir: PathBuf) {
        use crate::agent_registry::{AgentRecord, AgentRegistry, AgentStatus, ValidatedAgentName};
        let mut registry = AgentRegistry::open(ctx.watcher.agent_registry_path.clone()).unwrap();
        registry
            .register(AgentRecord {
                name: ValidatedAgentName::new(name).unwrap(),
                identity_id: id,
                inbox_dir,
                persona_name: None,
                spawned_at: time::OffsetDateTime::now_utc(),
                status: AgentStatus::Running,
            })
            .unwrap();
    }

    // Shared test-helper actors for tests that need a ProcessInbound recipient.
    //
    // `NotifyCollector` collects messages into a shared Vec. `field` controls
    // which field of each message is collected. `notify`, when `Some`, fires a
    // oneshot after the first message arrives.

    enum CollectorField {
        Payload,
        MessageId,
    }

    struct NotifyCollector {
        dispatched: Arc<Mutex<Vec<String>>>,
        notify: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
        field: CollectorField,
    }

    impl Actor for NotifyCollector {
        type Context = Context<Self>;
    }

    impl Handler<crate::agent::ProcessInbound> for NotifyCollector {
        type Result = ();
        fn handle(&mut self, msg: crate::agent::ProcessInbound, _ctx: &mut Context<Self>) {
            let value = match self.field {
                CollectorField::Payload => msg.payload,
                CollectorField::MessageId => msg.message_id,
            };
            self.dispatched.lock().unwrap().push(value);
            if let Some(sender) = self.notify.lock().unwrap().take() {
                let _ = sender.send(());
            }
        }
    }

    /// Start a `NotifyCollector` actor and return a `Recipient<ProcessInbound>`.
    ///
    /// Must be called inside an active `actix::System` context (e.g. inside
    /// `block_on`). The `dispatched` arc is shared with the caller so values
    /// can be read after the system stops. Pass `Arc::new(Mutex::new(None))`
    /// for `notify` when no oneshot signal is needed.
    fn make_collector(
        field: CollectorField,
        dispatched: Arc<Mutex<Vec<String>>>,
        notify: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    ) -> actix::Recipient<crate::agent::ProcessInbound> {
        let addr = NotifyCollector {
            dispatched,
            notify,
            field,
        }
        .start();
        addr.recipient::<crate::agent::ProcessInbound>()
    }

    // W1: valid signed envelope → Delivered, file in cur/, delivery ledger
    // entry, audit transport.delivered. No route is registered, so
    // dispatch_envelope logs a warning and 0 ProcessInbound messages are sent.
    #[test]
    fn valid_envelope_delivers_to_cur() {
        let ctx = build_ctx();
        let (env, bytes) = crate::test_support::make_signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
        );
        let path = place_in_new(&ctx.inbox, "test1.json", &bytes);

        let dispatched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatched_clone = Arc::clone(&dispatched);

        // Run inside an actix System so we can observe whether any
        // ProcessInbound messages are dispatched. No route is registered in
        // the routing table, so dispatch_envelope must log a warning and skip.
        actix::System::new().block_on(async move {
            let _recipient = make_collector(
                CollectorField::MessageId,
                dispatched_clone,
                Arc::new(Mutex::new(None)),
            );

            let outcome = ctx
                .watcher
                .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false)
                .unwrap();

            assert!(
                matches!(outcome, ProcessOutcome::Delivered { message_id, .. } if message_id == env.message_id),
                "expected Delivered, got {outcome:?}",
            );
            assert!(!path.exists(), "file should have been removed from new/");
            assert!(
                ctx.inbox.cur().join("test1.json").exists(),
                "file should be in cur/",
            );

            let delivery_key = DeliveryKey {
                recipient_id: ctx.recipient_id,
                message_id: env.message_id,
            };
            assert!(
                ctx.watcher.delivery.contains(&delivery_key).unwrap(),
                "delivery ledger should contain the key",
            );

            let lines = audit_lines(&ctx.audit_dir);
            assert_eq!(lines.len(), 1, "expected one audit line");
            assert_eq!(lines[0]["kind"], "transport.delivered");
            assert_eq!(lines[0]["message_id"], env.message_id.to_string());
            assert_eq!(lines[0]["recipient_id"], ctx.recipient_id.to_string());

            actix::System::current().stop();
        });

        // No route was registered: dispatch_envelope must have skipped
        // sending. The collector must have received 0 messages.
        assert_eq!(
            dispatched.lock().unwrap().len(),
            0,
            "no dispatch expected when routing table has no entry for recipient",
        );
    }

    // W2: tampered signature → Quarantined with reason signature_invalid, file
    // in quarantine/ with .signature_invalid suffix, audit transport.quarantine.
    #[test]
    fn tampered_signature_quarantines_with_reason_suffix() {
        use reeve_types::EnvelopeSignature;

        let ctx = build_ctx();
        let (mut env, _) = crate::test_support::make_signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
        );
        let mut sig_bytes = *env.signature.as_bytes();
        sig_bytes[0] ^= 0x01;
        env.signature = EnvelopeSignature::from_bytes(sig_bytes);

        let bytes = serde_json::to_vec(&env).unwrap();
        let path = place_in_new(&ctx.inbox, "tampered.json", &bytes);

        let outcome = ctx
            .watcher
            .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false)
            .unwrap();

        assert!(
            matches!(
                outcome,
                ProcessOutcome::Quarantined {
                    reason: QuarantineReason::SignatureInvalid
                }
            ),
            "expected Quarantined(SignatureInvalid), got {outcome:?}",
        );
        assert!(!path.exists(), "file should have left new/");
        let quarantine_dest = ctx
            .inbox
            .quarantine()
            .join("tampered.json.signature_invalid");
        assert!(
            quarantine_dest.exists(),
            "quarantine file with reason suffix should exist",
        );

        let lines = audit_lines(&ctx.audit_dir);
        assert_eq!(lines.len(), 1, "expected one audit line");
        assert_eq!(lines[0]["kind"], "transport.quarantine");
        assert_eq!(lines[0]["reason"], "signature_invalid");
    }

    // W3: crash-recovery scan — file placed in new/ before watcher starts;
    // scan_new picks it up and processes it normally.
    #[test]
    fn crash_recovery_scan_processes_leftover_file() {
        let ctx = build_ctx();
        let (_, bytes) = crate::test_support::make_signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
        );
        place_in_new(&ctx.inbox, "leftover.json", &bytes);

        ctx.watcher
            .scan_new(ctx.recipient_id, &ctx.inbox, &|_| {})
            .unwrap();

        assert!(
            ctx.inbox.cur().join("leftover.json").exists(),
            "leftover file should be in cur/ after scan",
        );
    }

    // W4: idempotency — delivery ledger already contains (recipient_id,
    // message_id) and the replay window has expired (replay ledger does not
    // contain the key). The file moves to cur/ and the outcome is
    // AlreadyDelivered; the ledger is not written again; no
    // transport.delivered audit event is emitted.
    #[test]
    fn delivery_dedup_when_replay_expired_yields_already_delivered() {
        let ctx = build_ctx();
        let (env, bytes) = crate::test_support::make_signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
        );

        let delivery_key = DeliveryKey {
            recipient_id: ctx.recipient_id,
            message_id: env.message_id,
        };
        ctx.watcher
            .delivery
            .record(delivery_key.clone(), OffsetDateTime::now_utc())
            .unwrap();

        let path = place_in_new(&ctx.inbox, "redelivery.json", &bytes);
        let outcome = ctx
            .watcher
            .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false)
            .unwrap();

        assert!(
            matches!(outcome, ProcessOutcome::AlreadyDelivered { message_id } if message_id == env.message_id),
            "expected AlreadyDelivered, got {outcome:?}",
        );
        assert!(!path.exists(), "file should have left new/");
        assert!(
            ctx.inbox.cur().join("redelivery.json").exists(),
            "file should still move to cur/ even when already delivered",
        );

        let lines = audit_lines(&ctx.audit_dir);
        assert_eq!(
            lines.len(),
            0,
            "no audit event expected for already-delivered path",
        );
        assert!(
            ctx.watcher.delivery.contains(&delivery_key).unwrap(),
            "delivery ledger entry should remain",
        );
    }

    // W5: garbage bytes → Quarantined(ParseFailure), file moves to quarantine/.
    #[test]
    fn garbage_bytes_quarantine_as_parse_failure() {
        let ctx = build_ctx();
        let path = place_in_new(&ctx.inbox, "garbage.json", b"not json at all");

        let outcome = ctx
            .watcher
            .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false)
            .unwrap();

        assert!(
            matches!(
                outcome,
                ProcessOutcome::Quarantined {
                    reason: QuarantineReason::ParseFailure
                }
            ),
            "expected Quarantined(ParseFailure), got {outcome:?}",
        );
        assert!(!path.exists(), "file should have left new/");
        assert!(
            ctx.inbox
                .quarantine()
                .join("garbage.json.parse_failure")
                .exists(),
            "quarantine file with parse_failure suffix should exist",
        );
    }

    // W6: quarantine filename preserves original stem — multi-component name
    // (dots in the original) gets the reason appended as an additional suffix.
    #[test]
    fn quarantine_filename_preserves_original_stem() {
        let quarantine_dir = PathBuf::from("/fake/quarantine");
        let reason = QuarantineReason::Replay;
        let dest = quarantine_path(&quarantine_dir, "abc-123.foo.json", &reason);
        assert_eq!(
            dest,
            PathBuf::from("/fake/quarantine/abc-123.foo.json.replay"),
        );
    }

    // W7: WatcherError Display produces non-empty messages and source() chains
    // are correct.
    #[test]
    fn watcher_error_display_non_empty() {
        use std::error::Error as _;

        let io_err = WatcherError::Io {
            path: PathBuf::from("some/path"),
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        let io_msg = io_err.to_string();
        assert!(!io_msg.is_empty(), "Io: {io_msg}");
        assert!(io_msg.contains("some/path"), "Io path: {io_msg}");
        assert!(io_err.source().is_some(), "Io should have source");

        let cross = WatcherError::CrossFilesystemRename {
            from: PathBuf::from("/a/from"),
            to: PathBuf::from("/b/to"),
        };
        let cross_msg = cross.to_string();
        assert!(
            cross_msg.contains("filesystem"),
            "CrossFilesystemRename: {cross_msg}",
        );
        assert!(
            cross.source().is_none(),
            "CrossFilesystemRename has no source",
        );

        let ledger_err = WatcherError::Ledger(LedgerError::Serialize(
            serde_json::from_str::<serde_json::Value>("bad").unwrap_err(),
        ));
        let ledger_msg = ledger_err.to_string();
        assert!(!ledger_msg.is_empty(), "Ledger: {ledger_msg}");
        assert!(
            ledger_err.source().is_some(),
            "Ledger should have source chain",
        );

        let notify_err = WatcherError::Notify(notify::Error::generic("synthetic notify error"));
        let notify_msg = notify_err.to_string();
        assert!(
            notify_msg.contains("notify"),
            "Notify display: {notify_msg}",
        );
        assert!(notify_err.source().is_some(), "Notify should have source");

        let verification_err = WatcherError::Verification(VerificationError::MessageIdMint);
        let verification_msg = verification_err.to_string();
        assert!(
            !verification_msg.is_empty(),
            "Verification: {verification_msg}",
        );
        assert!(
            verification_err.source().is_some(),
            "Verification wraps VerificationError as source",
        );
    }

    // W8: envelope addressed to a KNOWN identity that is not this inbox's agent
    // → Quarantined(RecipientMismatch).
    //
    // To distinguish RecipientMismatch from RecipientNotFound the envelope's
    // recipient_id must be present in the agent registry. We register it before
    // calling process_file.
    #[test]
    fn recipient_mismatch_quarantines() {
        let ctx = build_ctx();
        let wrong_recipient = IdentityId::new().unwrap();

        // Register wrong_recipient in the agent registry so the pipeline
        // produces RecipientMismatch rather than RecipientNotFound.
        register_in_ctx_registry(
            &ctx,
            "other",
            wrong_recipient,
            PathBuf::from("/tmp/other/inbox"),
        );

        let (_, bytes) = crate::test_support::make_signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            wrong_recipient,
        );
        let path = place_in_new(&ctx.inbox, "mismatch.json", &bytes);

        let outcome = ctx
            .watcher
            .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| true)
            .unwrap();

        let ProcessOutcome::Quarantined {
            reason: QuarantineReason::RecipientMismatch { expected },
        } = outcome
        else {
            panic!("expected Quarantined(RecipientMismatch), got {outcome:?}");
        };
        assert_eq!(
            expected, ctx.recipient_id,
            "RecipientMismatch.expected must be the inbox owner's agent_id",
        );
        assert!(!path.exists(), "file should have left new/");
    }

    // W_new1: envelope where recipient_id is unknown (not in registry) →
    // Quarantined(RecipientNotFound).
    #[test]
    fn unknown_recipient_id_quarantines_with_recipient_not_found() {
        let ctx = build_ctx();
        // Envelope addressed to a fresh identity that is NOT in the agent
        // registry → RecipientNotFound.
        let unknown_recipient = IdentityId::new().unwrap();
        let (_, bytes) = crate::test_support::make_signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            unknown_recipient,
        );
        let path = place_in_new(&ctx.inbox, "unknown_recipient.json", &bytes);

        let outcome = ctx
            .watcher
            .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false)
            .unwrap();

        assert!(
            matches!(
                outcome,
                ProcessOutcome::Quarantined {
                    reason: QuarantineReason::RecipientNotFound { .. }
                }
            ),
            "expected Quarantined(RecipientNotFound), got {outcome:?}",
        );
        assert!(!path.exists(), "file should have left new/");
        assert!(
            ctx.inbox
                .quarantine()
                .join("unknown_recipient.json.recipient_not_found")
                .exists(),
            "quarantine file with recipient_not_found suffix should exist",
        );
    }

    // W_new2: registered route → Delivered, ProcessInbound dispatched.
    //
    // We register a route for recipient_id before process_file, then verify
    // the watcher dispatches ProcessInbound to the registered recipient after
    // successful delivery.
    #[test]
    fn registered_route_dispatches_process_inbound() {
        let ctx = build_ctx();

        // Register ctx.recipient_id in the agent registry so the envelope passes
        // the recipient check.
        register_in_ctx_registry(
            &ctx,
            "lead",
            ctx.recipient_id,
            ctx.inbox.root().to_path_buf(),
        );

        let dispatched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatched_clone = Arc::clone(&dispatched);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let notify = Arc::new(Mutex::new(Some(tx)));

        actix::System::new().block_on(async move {
            let recipient = make_collector(CollectorField::Payload, dispatched_clone, notify);

            ctx.watcher.register_route(ctx.recipient_id, recipient);

            let (env, bytes) = crate::test_support::make_signed_envelope(
                &ctx.keypair,
                ctx.sender_id,
                ctx.key_record.key_id,
                ctx.recipient_id,
            );
            let path = place_in_new(&ctx.inbox, "routed.json", &bytes);

            let outcome = ctx
                .watcher
                .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false)
                .unwrap();

            assert!(
                matches!(outcome, ProcessOutcome::Delivered { message_id, .. } if message_id == env.message_id),
                "expected Delivered, got {outcome:?}",
            );

            tokio::time::timeout(std::time::Duration::from_secs(5), rx)
                .await
                .expect("timed out waiting for ProcessInbound dispatch")
                .expect("oneshot sender dropped");
            actix::System::current().stop();

            // Verify count and payload while env is still in scope.
            let got = dispatched.lock().unwrap();
            assert_eq!(got.len(), 1, "expected exactly one ProcessInbound dispatched");
            let expected_payload = String::from_utf8(env.body.clone()).unwrap();
            assert_eq!(
                got[0],
                expected_payload,
                "dispatched payload must equal envelope body",
            );
        });
    }

    // W_new3: already_delivered → no second dispatch.
    //
    // When the delivery ledger already contains (recipient_id, message_id),
    // the watcher moves the file to cur/ but must NOT call do_send again —
    // the message was already processed before a prior crash.
    #[test]
    fn already_delivered_does_not_dispatch() {
        let ctx = build_ctx();

        // Register ctx.recipient_id in the agent registry.
        register_in_ctx_registry(
            &ctx,
            "lead",
            ctx.recipient_id,
            ctx.inbox.root().to_path_buf(),
        );

        let dispatched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatched_clone = Arc::clone(&dispatched);

        actix::System::new().block_on(async move {
            let recipient = make_collector(
                CollectorField::Payload,
                dispatched_clone,
                Arc::new(Mutex::new(None)),
            );

            ctx.watcher.register_route(ctx.recipient_id, recipient);

            let (env, bytes) = crate::test_support::make_signed_envelope(
                &ctx.keypair,
                ctx.sender_id,
                ctx.key_record.key_id,
                ctx.recipient_id,
            );

            // Pre-record delivery so the watcher takes the already_delivered path.
            ctx.watcher
                .delivery
                .record(
                    DeliveryKey {
                        recipient_id: ctx.recipient_id,
                        message_id: env.message_id,
                    },
                    time::OffsetDateTime::now_utc(),
                )
                .unwrap();

            let path = place_in_new(&ctx.inbox, "already.json", &bytes);

            let outcome = ctx
                .watcher
                .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false)
                .unwrap();

            assert!(
                matches!(outcome, ProcessOutcome::AlreadyDelivered { .. }),
                "expected AlreadyDelivered, got {outcome:?}",
            );

            actix::System::current().stop();
        });

        // No dispatch must have occurred.
        let got = dispatched.lock().unwrap();
        assert_eq!(
            got.len(),
            0,
            "no ProcessInbound should be dispatched for already-delivered message"
        );
    }

    // W_new4: scan_cur_and_dispatch with non-existent cur/ — no panic, no dispatch.
    //
    // On first boot an agent's cur/ directory may not yet exist. The function
    // must degrade gracefully: read_dir failure is silently ignored and no
    // ProcessInbound messages are dispatched.
    #[test]
    fn scan_cur_and_dispatch_silently_handles_missing_cur_dir() {
        let ctx = build_ctx();
        let dispatched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatched_clone = Arc::clone(&dispatched);

        // Remove cur/ so read_dir fails.
        fs::remove_dir_all(ctx.inbox.cur()).unwrap();

        actix::System::new().block_on(async move {
            let recipient = make_collector(
                CollectorField::Payload,
                dispatched_clone,
                Arc::new(Mutex::new(None)),
            );
            // Register the route so dispatch_envelope has somewhere to send.
            ctx.watcher.register_route(ctx.recipient_id, recipient);

            // Must not panic.
            ctx.watcher
                .scan_cur_and_dispatch(&ctx.inbox, ctx.recipient_id);

            actix::System::current().stop();
        });

        let got = dispatched.lock().unwrap();
        assert_eq!(got.len(), 0, "no dispatch expected when cur/ is missing");
    }

    // W_p1: scan_cur_and_dispatch re-dispatches a message already delivered
    // by process_file — at-least-once crash-recovery semantics.
    //
    // The delivery ledger records the message after process_file runs.
    // scan_cur_and_dispatch must re-dispatch the same file regardless, because
    // the agent may have crashed before processing it. Adding a ledger guard
    // here would silently drop messages that were dispatched-but-not-processed.
    #[test]
    fn scan_cur_and_dispatch_redispatches_already_delivered_message() {
        let ctx = build_ctx();

        // Register the recipient so process_file can deliver the envelope.
        register_in_ctx_registry(
            &ctx,
            "lead",
            ctx.recipient_id,
            ctx.inbox.root().to_path_buf(),
        );

        let dispatched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatched_clone = Arc::clone(&dispatched);

        actix::System::new().block_on(async move {
            let addr = Actor::start(NotifyCollector {
                dispatched: Arc::clone(&dispatched_clone),
                notify: Arc::new(Mutex::new(None)),
                field: CollectorField::MessageId,
            });
            let recipient = addr.recipient::<crate::agent::ProcessInbound>();

            ctx.watcher.register_route(ctx.recipient_id, recipient.clone());

            let (env, bytes) = crate::test_support::make_signed_envelope(
                &ctx.keypair,
                ctx.sender_id,
                ctx.key_record.key_id,
                ctx.recipient_id,
            );
            let path = place_in_new(&ctx.inbox, "at_least_once.json", &bytes);

            // Step 1: process_file delivers the envelope and dispatches the first
            // ProcessInbound. The file now lives in cur/ and the ledger is written.
            let outcome = ctx
                .watcher
                .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false)
                .unwrap();
            assert!(
                matches!(outcome, ProcessOutcome::Delivered { message_id, .. } if message_id == env.message_id),
                "expected Delivered on first process_file, got {outcome:?}",
            );

            // Step 2: scan_cur_and_dispatch re-dispatches the same cur/ file
            // without consulting the ledger — at-least-once semantics.
            ctx.watcher.scan_cur_and_dispatch(&ctx.inbox, ctx.recipient_id);

            // Wait up to 5 s for both dispatches to arrive at the collector.
            let deadline =
                tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                {
                    let got = dispatched_clone.lock().unwrap();
                    if got.len() >= 2 {
                        break;
                    }
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for second ProcessInbound dispatch"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }

            actix::System::current().stop();

            // Verify both dispatches carry the same message_id.
            let got = dispatched_clone.lock().unwrap();
            assert_eq!(
                got.len(),
                2,
                "scan_cur_and_dispatch must re-dispatch the already-delivered message (at-least-once)"
            );
            assert_eq!(
                got[0], got[1],
                "both dispatches must carry the same message_id",
            );
            assert_eq!(
                got[0],
                env.message_id.to_string(),
                "dispatched message_id must match the envelope",
            );
        });
    }

    // W_p2: scan_cur_and_dispatch recovers a message when a route is registered
    // after process_file has already moved the envelope to cur/.
    //
    // Crash-recovery scenario: process_file verifies the envelope and writes it
    // to cur/, but no route was registered yet so dispatch_envelope skips
    // dispatch. Once a route is registered, scan_cur_and_dispatch must deliver
    // the waiting envelope.
    #[test]
    fn scan_cur_recovers_message_when_route_registered_after_delivery() {
        let ctx = build_ctx();

        let (env, bytes) = crate::test_support::make_signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
        );
        let path = place_in_new(&ctx.inbox, "recover.json", &bytes);

        // Step 1: process_file with no route registered. The envelope passes
        // verification and lands in cur/, but dispatch_envelope logs a warning
        // and skips dispatch.
        let outcome = ctx
            .watcher
            .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false)
            .unwrap();
        assert!(
            matches!(outcome, ProcessOutcome::Delivered { message_id, .. } if message_id == env.message_id),
            "expected Delivered on first process_file, got {outcome:?}",
        );
        assert!(
            ctx.inbox.cur().join("recover.json").exists(),
            "envelope should be in cur/ after process_file with no route",
        );

        let dispatched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatched_clone = Arc::clone(&dispatched);

        actix::System::new().block_on(async move {
            let recipient = make_collector(
                CollectorField::MessageId,
                dispatched_clone,
                Arc::new(Mutex::new(None)),
            );

            // Step 2: register the route.
            ctx.watcher
                .register_route(ctx.recipient_id, recipient.clone());

            // Step 3: scan_cur_and_dispatch picks up the waiting envelope and
            // dispatches it now that a route is present.
            ctx.watcher
                .scan_cur_and_dispatch(&ctx.inbox, ctx.recipient_id);

            // Wait up to 5 s for the dispatch to arrive.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                {
                    let got = dispatched.lock().unwrap();
                    if !got.is_empty() {
                        break;
                    }
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for ProcessInbound from scan_cur_and_dispatch",
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }

            actix::System::current().stop();

            let got = dispatched.lock().unwrap();
            assert_eq!(
                got.len(),
                1,
                "scan_cur_and_dispatch must dispatch the envelope recovered from cur/",
            );
            assert_eq!(
                got[0],
                env.message_id.to_string(),
                "dispatched message_id must match the envelope",
            );
        });
    }

    // W_new5: scan_cur_and_dispatch skips symlinks in cur/.
    //
    // Symlinks must be skipped via symlink_metadata, consistent with scan_new.
    // O_NOFOLLOW protects the open, but the explicit skip is defense-in-depth.
    #[cfg(unix)]
    #[test]
    fn scan_cur_and_dispatch_skips_symlinks() {
        use std::os::unix::fs::symlink;

        let ctx = build_ctx();
        let dispatched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatched_clone = Arc::clone(&dispatched);

        // Place a symlink in cur/ pointing to a file outside the inbox.
        let target = ctx.inbox.tmp().join("target.json");
        fs::write(&target, b"not an envelope").unwrap();
        let link = ctx.inbox.cur().join("symlink.json");
        symlink(&target, &link).unwrap();

        actix::System::new().block_on(async move {
            let recipient = make_collector(
                CollectorField::Payload,
                dispatched_clone,
                Arc::new(Mutex::new(None)),
            );
            // Register the route so dispatch_envelope has somewhere to send
            // (it is not expected to be called, but it must not panic).
            ctx.watcher.register_route(ctx.recipient_id, recipient);

            ctx.watcher
                .scan_cur_and_dispatch(&ctx.inbox, ctx.recipient_id);

            actix::System::current().stop();
        });

        assert!(link.exists(), "symlink should remain untouched");
        let got = dispatched.lock().unwrap();
        assert_eq!(got.len(), 0, "symlink must not be dispatched");
    }

    // W9: scan_new skips symlinks.
    #[cfg(unix)]
    #[test]
    fn scan_new_skips_symlinks() {
        use std::os::unix::fs::symlink;

        let ctx = build_ctx();
        let target = ctx.inbox.tmp().join("real_target.json");
        fs::write(&target, b"not an envelope").unwrap();
        let link = ctx.inbox.new_dir().join("symlink.json");
        symlink(&target, &link).unwrap();

        ctx.watcher
            .scan_new(ctx.recipient_id, &ctx.inbox, &|_| {})
            .unwrap();
        assert!(link.exists(), "symlink should remain untouched by scan");
    }

    // W10: non-UTF-8 filename → InvalidFilename(NotUtf8), file stays in new/,
    // audit event transport.filename-rejected emitted with reason "not_utf8".
    //
    // Non-UTF-8 filenames cannot be used to construct safe quarantine paths.
    // On macOS APFS rejects non-UTF-8 filenames at the OS level, so the path
    // is constructed without writing to disk.
    #[cfg(unix)]
    #[test]
    fn non_utf8_filename_yields_invalid_filename() {
        use std::os::unix::ffi::OsStrExt;

        let ctx = build_ctx();
        let bad_name = OsStr::from_bytes(b"\x80invalid\x81.json");
        let path = ctx.inbox.new_dir().join(bad_name);

        let outcome = ctx
            .watcher
            .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false)
            .unwrap();

        assert!(
            matches!(
                outcome,
                ProcessOutcome::InvalidFilename {
                    reason: FilenameError::NotUtf8
                }
            ),
            "expected InvalidFilename(NotUtf8), got {outcome:?}",
        );
        let lines = audit_lines(&ctx.audit_dir);
        assert_eq!(
            lines.len(),
            1,
            "expected one audit event for invalid-filename path"
        );
        assert_eq!(lines[0]["kind"], "transport.filename-rejected");
        assert_eq!(lines[0]["reason"], "not_utf8");
    }

    // W11: filename longer than MAX_INBOX_FILENAME_BYTES → InvalidFilename(TooLong),
    // file stays in new/, audit event transport.filename-rejected emitted.
    #[test]
    fn overlong_filename_yields_invalid_filename() {
        let ctx = build_ctx();
        // 246 bytes of 'a' plus ".json" = 251 bytes total — over MAX_INBOX_FILENAME_BYTES.
        let long_name = format!("{}.json", "a".repeat(246));
        assert!(
            long_name.len() > MAX_INBOX_FILENAME_BYTES,
            "test setup: name must exceed cap",
        );
        let path = ctx.inbox.new_dir().join(&long_name);
        fs::write(&path, b"irrelevant content").unwrap();

        let outcome = ctx
            .watcher
            .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false)
            .unwrap();

        assert!(
            matches!(
                outcome,
                ProcessOutcome::InvalidFilename {
                    reason: FilenameError::TooLong { .. }
                }
            ),
            "expected InvalidFilename(TooLong), got {outcome:?}",
        );
        assert!(path.exists(), "overlong filename file must stay in new/");
        let lines = audit_lines(&ctx.audit_dir);
        assert_eq!(
            lines.len(),
            1,
            "expected one audit event for overlong filename"
        );
        assert_eq!(lines[0]["kind"], "transport.filename-rejected");
        assert_eq!(
            lines[0]["reason"],
            format!("too_long({})", long_name.len()),
            "audit token must encode the exact byte length",
        );
    }

    // W12: file removed between detection and process_file call → AlreadyProcessed.
    #[test]
    fn file_removed_before_processing_yields_already_processed() {
        let ctx = build_ctx();
        let ghost_path = ctx.inbox.new_dir().join("ghost.json");

        let outcome = ctx
            .watcher
            .process_file(&ghost_path, ctx.recipient_id, &ctx.inbox, |_| false)
            .unwrap();

        assert!(
            matches!(outcome, ProcessOutcome::AlreadyProcessed),
            "expected AlreadyProcessed for missing file, got {outcome:?}",
        );
        assert!(!ghost_path.exists(), "ghost path should not exist in new/");
        let lines = audit_lines(&ctx.audit_dir);
        assert_eq!(lines.len(), 0, "no audit event for already-processed race");
    }

    // W13: oversize file (MAX_ENVELOPE_BYTES + 1 zeros) → Quarantined(ParseFailure),
    // file in quarantine/, audit transport.quarantine emitted.
    #[test]
    fn oversize_file_quarantines_as_parse_failure() {
        let ctx = build_ctx();
        let oversize = vec![0u8; MAX_ENVELOPE_BYTES + 1];
        let path = place_in_new(&ctx.inbox, "oversize.json", &oversize);

        let outcome = ctx
            .watcher
            .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false)
            .unwrap();

        assert!(
            matches!(
                outcome,
                ProcessOutcome::Quarantined {
                    reason: QuarantineReason::ParseFailure
                }
            ),
            "expected Quarantined(ParseFailure) for oversize file, got {outcome:?}",
        );
        assert!(!path.exists(), "oversize file should have left new/");
        assert!(
            ctx.inbox
                .quarantine()
                .join("oversize.json.parse_failure")
                .exists(),
            "quarantine file with parse_failure suffix should exist",
        );

        let lines = audit_lines(&ctx.audit_dir);
        assert_eq!(lines.len(), 1, "expected one audit line for oversize");
        assert_eq!(lines[0]["kind"], "transport.quarantine");
        assert_eq!(lines[0]["reason"], "parse_failure");
    }

    // W14: destination directory missing causes Err(WatcherError::Io), not
    // Ok(AlreadyProcessed). Deleting inbox.cur() after watcher construction
    // simulates infrastructure failure rather than benign race.
    #[cfg(unix)]
    #[test]
    fn dest_dir_missing_returns_io_error() {
        let ctx = build_ctx();
        let (_, bytes) = crate::test_support::make_signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
        );
        let path = place_in_new(&ctx.inbox, "infra_fail.json", &bytes);

        // Remove the destination directory so the rename fails with NotFound
        // while the source still exists.
        fs::remove_dir_all(ctx.inbox.cur()).unwrap();

        let result = ctx
            .watcher
            .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false);

        assert!(
            matches!(result, Err(WatcherError::Io { .. })),
            "expected Err(WatcherError::Io) when dest dir is missing, got {result:?}",
        );
        // Path in the error should point into cur/ (the missing dest dir).
        if let Err(WatcherError::Io { ref path, .. }) = result {
            assert!(
                path.to_string_lossy().contains("cur"),
                "error path should reference cur/, got: {}",
                path.display(),
            );
        }
    }

    // W17: filename containing a null byte → InvalidFilename(ContainsNull),
    // file stays in new/, audit event transport.filename-rejected emitted
    // with reason "contains_null".
    #[cfg(unix)]
    #[test]
    fn null_byte_filename_yields_invalid_filename() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let ctx = build_ctx();
        // Construct an OsStr containing a null byte — not valid as a C string
        // but constructible on Unix.
        let null_name = OsStr::from_bytes(b"abc\x00def.json");
        let path = ctx.inbox.new_dir().join(null_name);

        let outcome = ctx
            .watcher
            .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false)
            .unwrap();

        assert!(
            matches!(
                outcome,
                ProcessOutcome::InvalidFilename {
                    reason: FilenameError::ContainsNull
                }
            ),
            "expected InvalidFilename(ContainsNull), got {outcome:?}",
        );
        let lines = audit_lines(&ctx.audit_dir);
        assert_eq!(
            lines.len(),
            1,
            "expected one audit event for null-byte filename"
        );
        assert_eq!(lines[0]["kind"], "transport.filename-rejected");
        assert_eq!(lines[0]["reason"], "contains_null");
    }

    // W18: quarantine/ directory missing causes Err(WatcherError::Io), not
    // Ok(AlreadyProcessed). An invalid file (tampered signature) is placed in
    // new/, then quarantine/ is deleted to simulate infrastructure failure.
    #[cfg(unix)]
    #[test]
    fn quarantine_dir_missing_returns_io_error() {
        use reeve_types::EnvelopeSignature;

        let ctx = build_ctx();
        let (mut env, _) = crate::test_support::make_signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
        );
        // Tamper signature so the envelope is quarantined.
        let mut sig_bytes = *env.signature.as_bytes();
        sig_bytes[0] ^= 0x01;
        env.signature = EnvelopeSignature::from_bytes(sig_bytes);
        let bytes = serde_json::to_vec(&env).unwrap();
        let path = place_in_new(&ctx.inbox, "quarantine_miss.json", &bytes);

        // Remove the quarantine directory so the rename fails with NotFound
        // while the source still exists.
        fs::remove_dir_all(ctx.inbox.quarantine()).unwrap();

        let result = ctx
            .watcher
            .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| false);

        assert!(
            matches!(result, Err(WatcherError::Io { .. })),
            "expected Err(WatcherError::Io) when quarantine dir is missing, got {result:?}",
        );
        // Path in the error should point into quarantine/ (the missing dest dir).
        if let Err(WatcherError::Io { ref path, .. }) = result {
            assert!(
                path.to_string_lossy().contains("quarantine"),
                "error path should reference quarantine/, got: {}",
                path.display(),
            );
        }
    }

    // W19: path with no file_name component (e.g. "/") → InvalidFilename(Reserved),
    // audit event transport.filename-rejected emitted with reason "reserved".
    // This exercises the no-filename branch in process_file that was previously
    // returning AlreadyProcessed.
    #[test]
    fn no_filename_path_yields_reserved() {
        let ctx = build_ctx();
        // PathBuf::from("/") has no file_name() component.
        let root_path = PathBuf::from("/");
        assert!(
            root_path.file_name().is_none(),
            "test setup: path must have no file_name",
        );

        let outcome = ctx
            .watcher
            .process_file(&root_path, ctx.recipient_id, &ctx.inbox, |_| false)
            .unwrap();

        assert!(
            matches!(
                outcome,
                ProcessOutcome::InvalidFilename {
                    reason: FilenameError::Reserved
                }
            ),
            "expected InvalidFilename(Reserved) for no-filename path, got {outcome:?}",
        );
        let lines = audit_lines(&ctx.audit_dir);
        assert_eq!(
            lines.len(),
            1,
            "expected one audit event for no-filename path"
        );
        assert_eq!(lines[0]["kind"], "transport.filename-rejected");
        assert_eq!(lines[0]["reason"], "reserved");
        // The rejection fires before any I/O on the path; new/ is unchanged.
        let new_entries: Vec<_> = fs::read_dir(ctx.inbox.new_dir()).unwrap().collect();
        assert!(
            new_entries.is_empty(),
            "new/ should be empty — rejection fires before any I/O",
        );
    }

    // W20: dangling symlink at source + missing dest dir → rename_disambiguating_enoent
    // returns Err(WatcherError::Io), not Ok(false). The key distinction: `exists()`
    // returns false for a dangling symlink (follows the link, finds no target), but
    // `symlink_metadata()` returns Ok (the symlink object itself exists). Using
    // `symlink_metadata` correctly classifies this as a dest-dir-gone infrastructure
    // failure rather than a benign AlreadyProcessed race.
    //
    // Paired with a missing-source test confirming Ok(false) is returned when
    // the source genuinely does not exist.
    #[cfg(unix)]
    #[test]
    fn dangling_symlink_dest_dir_missing_returns_io_error() {
        use std::os::unix::fs::symlink;

        let src_dir = tempdir().unwrap();
        let dst_dir = tempdir().unwrap();

        // Create a dangling symlink at `from`: the target doesn't exist so
        // `from.exists()` == false, but `from.symlink_metadata()` == Ok.
        let from = src_dir.path().join("dangling.json");
        let nonexistent_target = src_dir.path().join("does_not_exist.json");
        symlink(&nonexistent_target, &from).unwrap();
        assert!(
            from.symlink_metadata().is_ok(),
            "dangling symlink should exist (symlink_metadata succeeds)",
        );
        assert!(
            !from.exists(),
            "dangling symlink target should not exist (exists follows symlinks)",
        );

        // Remove the destination directory so rename(2) fails with ENOENT.
        // A naive check using `from.exists()` would see false and return Ok(false)
        // (treating it as a benign race), masking the real infrastructure failure.
        let dest_dir_path = dst_dir.keep();
        fs::remove_dir_all(&dest_dir_path).unwrap();
        let to = dest_dir_path.join("dangling.json");

        let result = rename_disambiguating_enoent(&from, &to);
        assert!(
            matches!(result, Err(WatcherError::Io { .. })),
            "dangling symlink + missing dest dir should return Err(Io), got {result:?}",
        );
        if let Err(WatcherError::Io { ref path, .. }) = result {
            assert_eq!(
                path,
                &dest_dir_path,
                "error path should be the dest dir (parent of to), got: {}",
                path.display(),
            );
        }
    }

    // Paired test for W20: when the source genuinely does not exist (no symlink,
    // just a missing file), rename_disambiguating_enoent returns Ok(false).
    #[cfg(unix)]
    #[test]
    fn missing_source_file_returns_ok_false() {
        let src_dir = tempdir().unwrap();
        let dst_dir = tempdir().unwrap();

        let from = src_dir.path().join("ghost.json"); // Does not exist.
        let to = dst_dir.path().join("ghost.json");

        let result = rename_disambiguating_enoent(&from, &to);
        assert!(
            matches!(result, Ok(false)),
            "missing source file should return Ok(false), got {result:?}",
        );
    }

    // Helper: place a regular file in `cur/` with a real system mtime (now).
    // Returns the path of the created file.
    fn place_in_cur(inbox: &AgentInbox, filename: &str) -> PathBuf {
        place_in(inbox.cur(), filename, b"placeholder")
    }

    // R1: rotate_cur moves files older than retention to archive/, leaves
    // younger files in cur/. "old" gets an ancient mtime via set_ancient_mtime;
    // "mid" and "fresh" keep the real system mtime (≈ now).
    #[test]
    fn r1_rotates_old_files() {
        let ctx = build_ctx();
        let real_now = OffsetDateTime::now_utc();
        let retention = Duration::hours(1);

        let old = place_in_cur(&ctx.inbox, "old.json");
        crate::test_support::set_ancient_mtime(&old);

        let mid = place_in_cur(&ctx.inbox, "mid.json");
        let fresh = place_in_cur(&ctx.inbox, "fresh.json");

        let outcome = ctx
            .watcher
            .rotate_cur(&ctx.inbox, retention, real_now)
            .unwrap();

        assert_eq!(outcome.archived, 1, "one file should be archived");
        assert_eq!(outcome.retained, 2, "two files should be retained");
        assert_eq!(outcome.skipped, 0, "no files should be skipped");

        assert!(!old.exists(), "old file should have been moved out of cur/");
        assert!(
            ctx.inbox.archive().join("old.json").exists(),
            "old file should be in archive/",
        );
        assert!(mid.exists(), "mid file should remain in cur/");
        assert!(fresh.exists(), "fresh file should remain in cur/");
    }

    // R2: rotate_cur on an empty cur/ returns all-zero outcome.
    #[test]
    fn r2_empty_cur_returns_zero() {
        let ctx = build_ctx();
        let now = OffsetDateTime::now_utc();

        let outcome = ctx
            .watcher
            .rotate_cur(&ctx.inbox, Duration::hours(1), now)
            .unwrap();

        assert_eq!(outcome.archived, 0);
        assert_eq!(outcome.retained, 0);
        assert_eq!(outcome.skipped, 0);
    }

    // R3: symlinks in cur/ are skipped (not archived), regular files are processed.
    #[cfg(unix)]
    #[test]
    fn r3_skips_symlinks() {
        use std::os::unix::fs::symlink;

        let ctx = build_ctx();

        // A regular file with an ancient mtime — old enough to archive at any sane `now`.
        let old = place_in_cur(&ctx.inbox, "regular.json");
        crate::test_support::set_ancient_mtime(&old);

        // A symlink in cur/ — should be skipped regardless of age.
        let target = ctx.inbox.tmp().join("link_target.json");
        fs::write(&target, b"target content").unwrap();
        let link = ctx.inbox.cur().join("link.json");
        symlink(&target, &link).unwrap();

        let now = OffsetDateTime::now_utc();
        let outcome = ctx
            .watcher
            .rotate_cur(&ctx.inbox, Duration::hours(1), now)
            .unwrap();

        assert_eq!(outcome.archived, 1, "regular file should be archived");
        assert_eq!(outcome.skipped, 1, "symlink should be skipped");
        assert!(link.exists(), "symlink should remain untouched");
        assert!(!old.exists(), "regular file should have been moved");
        assert!(ctx.inbox.archive().join("regular.json").exists());
    }

    // R4: no files old enough to archive → archived=0, retained=N.
    // All files have mtime ≈ real_now; pass now = real_now so age ≈ 0 < 1h.
    #[test]
    fn r4_no_eligible_files_archives_zero() {
        let ctx = build_ctx();
        let now = OffsetDateTime::now_utc();

        place_in_cur(&ctx.inbox, "a.json");
        place_in_cur(&ctx.inbox, "b.json");

        let outcome = ctx
            .watcher
            .rotate_cur(&ctx.inbox, Duration::hours(1), now)
            .unwrap();

        assert_eq!(outcome.archived, 0);
        assert_eq!(outcome.retained, 2);
        assert_eq!(outcome.skipped, 0);
    }

    // R5: missing archive/ directory causes Err(WatcherError::Io).
    // Uses ancient mtime (via touch) to ensure file is old enough to trigger
    // the rename attempt, which then fails because archive/ is gone.
    #[test]
    fn r5_missing_archive_dir_returns_io_error() {
        let ctx = build_ctx();

        // Place an old file in cur/ — ancient mtime guarantees age > any retention.
        let old = place_in_cur(&ctx.inbox, "old.json");
        crate::test_support::set_ancient_mtime(&old);

        // Remove archive/ to simulate infrastructure failure.
        fs::remove_dir_all(ctx.inbox.archive()).unwrap();

        let now = OffsetDateTime::now_utc();
        let result = ctx.watcher.rotate_cur(&ctx.inbox, Duration::hours(1), now);

        assert!(
            matches!(result, Err(WatcherError::Io { .. })),
            "expected Err(WatcherError::Io) when archive/ is missing, got {result:?}",
        );
    }

    // R6: file whose age equals retention exactly is archived (pins >= semantics).
    //
    // Injects both mtime (via touch -d) and now as parameters so that
    // now - mtime == retention precisely. Distinguishes >= from > because a
    // file one second younger would be retained.
    #[cfg(unix)]
    #[test]
    fn r6_age_equal_to_retention_is_archived() {
        let ctx = build_ctx();
        let retention = Duration::hours(1);

        // Fixed mtime with sub-second components zeroed to avoid touch drift.
        let mtime = OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .unwrap()
            .replace_nanosecond(0)
            .unwrap();
        // now - mtime == retention exactly: the file sits on the >= boundary.
        let now = mtime + retention;

        let path = place_in_cur(&ctx.inbox, "boundary.json");
        crate::test_support::set_mtime_at(&path, mtime);

        let outcome = ctx.watcher.rotate_cur(&ctx.inbox, retention, now).unwrap();

        assert_eq!(
            outcome.archived, 1,
            "file at exact boundary must be archived (>=)"
        );
        assert_eq!(outcome.retained, 0);
        assert_eq!(outcome.skipped, 0);
        assert!(!path.exists(), "should be moved out of cur/");
        assert!(ctx.inbox.archive().join("boundary.json").exists());
    }

    // R7: archive/ already contains a file with the same name; rotate_cur
    // overwrites it with the cur/ version.
    #[cfg(unix)]
    #[test]
    fn r7_pre_populated_archive_overwritten() {
        let ctx = build_ctx();

        // Pre-populate archive/ with stale content.
        place_in(ctx.inbox.archive(), "old.json", b"original");

        // Place replacement in cur/ with ancient mtime.
        let cur_path = place_in_cur(&ctx.inbox, "old.json");
        // Override placeholder content so we can detect overwrite.
        fs::write(&cur_path, b"replacement").unwrap();
        crate::test_support::set_ancient_mtime(&cur_path);

        let now = OffsetDateTime::now_utc();
        let outcome = ctx
            .watcher
            .rotate_cur(&ctx.inbox, Duration::hours(1), now)
            .unwrap();

        assert_eq!(outcome.archived, 1);
        assert!(!cur_path.exists(), "cur/ file must be gone");

        let archive_content = fs::read(ctx.inbox.archive().join("old.json")).unwrap();
        assert_eq!(
            archive_content, b"replacement",
            "archive/ file must contain the cur/ version",
        );
    }

    // R8: dotfiles in cur/ are skipped; regular files with the same mtime are archived.
    #[cfg(unix)]
    #[test]
    fn r8_dotfiles_are_skipped() {
        let ctx = build_ctx();

        let hidden = place_in_cur(&ctx.inbox, ".hidden.json");
        crate::test_support::set_ancient_mtime(&hidden);

        let regular = place_in_cur(&ctx.inbox, "regular.json");
        crate::test_support::set_ancient_mtime(&regular);

        let now = OffsetDateTime::now_utc();
        let outcome = ctx
            .watcher
            .rotate_cur(&ctx.inbox, Duration::hours(1), now)
            .unwrap();

        assert_eq!(outcome.archived, 1, "one regular file archived");
        assert_eq!(outcome.skipped, 1, "dotfile counted as skipped");
        assert_eq!(outcome.retained, 0);

        assert!(hidden.exists(), ".hidden.json must remain in cur/");
        assert!(
            ctx.inbox.archive().join("regular.json").exists(),
            "regular.json must be in archive/",
        );
        assert!(
            !ctx.inbox.archive().join(".hidden.json").exists(),
            ".hidden.json must not appear in archive/",
        );
    }

    // W_m1: crash-recovery dispatches envelope.message_id, not the filename stem.
    //
    // Place a real signed envelope in cur/ under a filename that does NOT match
    // the envelope's message_id. scan_cur_and_dispatch must use envelope.message_id
    // as the dispatched message_id, not the file stem.
    #[test]
    fn scan_cur_uses_envelope_message_id_not_filename_stem() {
        let ctx = build_ctx();

        // Register the recipient in the agent registry so delivery succeeds.
        register_in_ctx_registry(
            &ctx,
            "lead",
            ctx.recipient_id,
            ctx.inbox.root().to_path_buf(),
        );

        let (env, bytes) = crate::test_support::make_signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
        );
        // Write the envelope to cur/ under a filename that does NOT match message_id.
        let cur_path = ctx.inbox.cur().join("not-the-uuid.json");
        fs::write(&cur_path, &bytes).unwrap();

        let dispatched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatched_clone = Arc::clone(&dispatched);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let notify = Arc::new(Mutex::new(Some(tx)));

        actix::System::new().block_on(async move {
            let recipient = make_collector(CollectorField::MessageId, dispatched_clone, notify);
            ctx.watcher.register_route(ctx.recipient_id, recipient);

            ctx.watcher
                .scan_cur_and_dispatch(&ctx.inbox, ctx.recipient_id);

            tokio::time::timeout(std::time::Duration::from_secs(5), rx)
                .await
                .expect("timed out waiting for ProcessInbound dispatch")
                .expect("oneshot sender dropped");
            actix::System::current().stop();
        });

        let got = dispatched.lock().unwrap();
        assert_eq!(got.len(), 1, "exactly one message dispatched");
        assert_eq!(
            got[0],
            env.message_id.to_string(),
            "dispatched message_id must equal envelope.message_id, not filename stem",
        );
    }

    // W_m2: registry open failure → RecipientMismatch, not RecipientNotFound.
    //
    // Build a Watcher with a non-existent registry path. When
    // scan_new calls snapshot_known_recipients and the registry cannot be opened,
    // it returns None (conservative fallback: treat all recipients as known), so
    // an envelope addressed to a different recipient produces RecipientMismatch
    // rather than RecipientNotFound.
    #[test]
    fn registry_open_failure_produces_recipient_mismatch_not_not_found() {
        use crate::identity_registry::{IdentityRegistry, StoredIdentity};
        use crate::ledger::{DeliveryLedger, ReplayLedger};

        let reg_dir = crate::test_support::secure_dir();
        let replay_dir = crate::test_support::secure_dir();
        let delivery_dir = crate::test_support::secure_dir();
        let audit_data_dir = tempdir().unwrap();
        let inbox_data_dir = crate::test_support::secure_dir();

        let registry = Arc::new(IdentityRegistry::open(reg_dir.keep()).unwrap());
        let replay = Arc::new(ReplayLedger::open(replay_dir.keep()).unwrap());
        let delivery = Arc::new(DeliveryLedger::open(delivery_dir.keep()).unwrap());
        let audit = Arc::new(AuditLog::open(audit_data_dir.path().to_path_buf()).unwrap());

        let keypair = reeve_types::Keypair::generate();
        let identity =
            reeve_types::Identity::new_operator("registry-fail-sender".to_owned()).unwrap();
        let sender_id = identity.identity_id;
        let key_record = reeve_types::KeyRecord::new(sender_id, *keypair.public()).unwrap();
        let stored = StoredIdentity::new(identity, key_record.clone()).unwrap();
        registry.write(&stored).unwrap();

        let recipient_id = IdentityId::new().unwrap();
        let layout = InboxLayout::open(inbox_data_dir.path().to_path_buf()).unwrap();
        let inbox = layout.provision(recipient_id).unwrap();

        // Point agent_registry_path at a non-existent file — open will fail.
        let watcher = Watcher::new(
            &registry,
            &replay,
            Arc::clone(&delivery),
            Arc::clone(&audit),
            PathBuf::from("/nonexistent/no-such-registry.toml"),
        );

        // Envelope addressed to a different identity than this inbox's agent.
        let wrong_recipient = IdentityId::new().unwrap();
        let (_, bytes) = crate::test_support::make_signed_envelope(
            &keypair,
            sender_id,
            key_record.key_id,
            wrong_recipient,
        );
        let path = inbox.new_dir().join("mismatch_fallback.json");
        fs::write(&path, &bytes).unwrap();

        // scan_new calls snapshot_known_recipients once; with a missing
        // registry it returns None and treats all recipients as known.
        let quarantined: Mutex<Vec<String>> = Mutex::new(Vec::new());
        watcher
            .scan_new(recipient_id, &inbox, &|reason| {
                quarantined.lock().unwrap().push(reason);
            })
            .unwrap();

        let reasons = quarantined.into_inner().unwrap();
        assert_eq!(
            reasons.len(),
            1,
            "expected one quarantine on registry failure"
        );
        assert!(
            reasons[0].contains("recipient_mismatch"),
            "expected RecipientMismatch on registry open failure, got {:?}",
            reasons[0],
        );
    }

    // W_p1: non-UTF-8 envelope body → Quarantined(BodyNotUtf8), file in quarantine/.
    //
    // Build a valid signed envelope whose body bytes are not valid UTF-8.
    // process_file must quarantine it before delivery, not move it to cur/.
    #[test]
    fn non_utf8_body_quarantines_before_delivery() {
        use crate::verify::QuarantineReason;
        use reeve_transport::sign::sign_envelope;
        use reeve_types::{
            Envelope, EnvelopeSignature, MessageId, Nonce, PayloadHash, SchemaVersion,
        };

        let ctx = build_ctx();

        // Register recipient_id so the envelope passes the recipient check.
        register_in_ctx_registry(
            &ctx,
            "lead",
            ctx.recipient_id,
            ctx.inbox.root().to_path_buf(),
        );

        // Construct an envelope with a non-UTF-8 body.
        let placeholder = EnvelopeSignature::from_bytes([0u8; reeve_types::SIGNATURE_LEN]);
        let mut env = Envelope::new(
            SchemaVersion::V1,
            MessageId::new().unwrap(),
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
            time::OffsetDateTime::now_utc(),
            Nonce::from_bytes([0xAAu8; reeve_types::NONCE_LEN]),
            PayloadHash::from_bytes([0xBBu8; reeve_types::PAYLOAD_HASH_LEN]),
            // Invalid UTF-8: lone continuation byte.
            vec![0x80u8, 0x81u8, 0x82u8],
            placeholder,
        );
        let sig = sign_envelope(&env, ctx.keypair.private()).unwrap();
        env.signature = sig;
        let bytes = serde_json::to_vec(&env).unwrap();

        let path = place_in_new(&ctx.inbox, "bad_body.json", &bytes);

        let outcome = ctx
            .watcher
            .process_file(&path, ctx.recipient_id, &ctx.inbox, |_| true)
            .unwrap();

        assert!(
            matches!(
                outcome,
                ProcessOutcome::Quarantined {
                    reason: QuarantineReason::BodyNotUtf8
                }
            ),
            "expected Quarantined(BodyNotUtf8), got {outcome:?}",
        );
        assert!(!path.exists(), "file should have left new/");
        assert!(
            !ctx.inbox.cur().join("bad_body.json").exists(),
            "file must not be in cur/",
        );
        assert!(
            ctx.inbox
                .quarantine()
                .join("bad_body.json.body_not_utf8")
                .exists(),
            "file must be in quarantine/ with body_not_utf8 suffix",
        );
        let lines = audit_lines(&ctx.audit_dir);
        assert_eq!(lines.len(), 1, "expected one audit line");
        assert_eq!(lines[0]["kind"], "transport.quarantine");
        assert_eq!(lines[0]["reason"], "body_not_utf8");
        assert!(
            !ctx.watcher
                .delivery
                .contains(&DeliveryKey {
                    recipient_id: ctx.recipient_id,
                    message_id: env.message_id,
                })
                .unwrap(),
            "delivery ledger must remain clean after BodyNotUtf8 quarantine",
        );
    }

    // W_p2: scan_cur_and_dispatch skips a cur/ file whose envelope body is not
    // valid UTF-8. read_cur_payload returns None for such files and logs a
    // warning; no ProcessInbound is dispatched.
    //
    // The envelope is written directly to cur/ (bypassing process_file) to
    // simulate the residual-risk scenario where a non-UTF-8 body arrives in
    // cur/ without passing the guard in process_file.
    #[test]
    fn scan_cur_and_dispatch_skips_non_utf8_body_in_cur() {
        use reeve_types::{
            Envelope, EnvelopeSignature, MessageId, Nonce, PayloadHash, SchemaVersion,
        };

        let ctx = build_ctx();

        // Build a valid envelope struct whose body bytes are not valid UTF-8.
        // The envelope does not need a real signature: read_cur_payload only
        // parses the JSON and checks the body encoding; it does not verify.
        let placeholder_sig = EnvelopeSignature::from_bytes([0u8; reeve_types::SIGNATURE_LEN]);
        let env = Envelope::new(
            SchemaVersion::V1,
            MessageId::new().unwrap(),
            ctx.sender_id,
            ctx.key_record.key_id,
            ctx.recipient_id,
            time::OffsetDateTime::now_utc(),
            Nonce::from_bytes([0xAAu8; reeve_types::NONCE_LEN]),
            PayloadHash::from_bytes([0xBBu8; reeve_types::PAYLOAD_HASH_LEN]),
            vec![0x80u8, 0x81u8, 0x82u8],
            placeholder_sig,
        );
        let bytes = serde_json::to_vec(&env).unwrap();
        // Write the serialized envelope directly to cur/, bypassing process_file.
        let cur_path = ctx.inbox.cur().join("non_utf8_body.json");
        fs::write(&cur_path, &bytes).unwrap();

        let dispatched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatched_clone = Arc::clone(&dispatched);

        actix::System::new().block_on(async move {
            let recipient = make_collector(
                CollectorField::Payload,
                dispatched_clone,
                Arc::new(Mutex::new(None)),
            );
            ctx.watcher.register_route(ctx.recipient_id, recipient);

            ctx.watcher
                .scan_cur_and_dispatch(&ctx.inbox, ctx.recipient_id);

            actix::System::current().stop();
        });

        let got = dispatched.lock().unwrap();
        assert_eq!(
            got.len(),
            0,
            "non-UTF-8 body in cur/ must not be dispatched; got {got:?}",
        );
    }

    // W_p3: scan_cur_and_dispatch skips a cur/ file whose envelope.recipient_id
    // does not match the agent_id argument — defense-in-depth against stray
    // files in cur/.
    //
    // The envelope is written directly to cur/ (bypassing process_file) to
    // simulate a stray file landing there. No ProcessInbound must be dispatched.
    #[test]
    fn scan_cur_and_dispatch_skips_stray_recipient_id_mismatch() {
        let ctx = build_ctx();

        // A fresh identity that is NOT ctx.recipient_id — the stray recipient.
        let stray_recipient = IdentityId::new().unwrap();
        let (_, bytes) = crate::test_support::make_signed_envelope(
            &ctx.keypair,
            ctx.sender_id,
            ctx.key_record.key_id,
            stray_recipient,
        );
        // Write the envelope directly to cur/, bypassing process_file.
        let cur_path = ctx.inbox.cur().join("stray.json");
        fs::write(&cur_path, &bytes).unwrap();

        let dispatched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let dispatched_clone = Arc::clone(&dispatched);

        actix::System::new().block_on(async move {
            let recipient = make_collector(
                CollectorField::Payload,
                dispatched_clone,
                Arc::new(Mutex::new(None)),
            );
            // Register the route for the correct agent_id; the stray envelope
            // targets a different id and must be skipped.
            ctx.watcher.register_route(ctx.recipient_id, recipient);

            ctx.watcher
                .scan_cur_and_dispatch(&ctx.inbox, ctx.recipient_id);

            actix::System::current().stop();
        });

        let got = dispatched.lock().unwrap();
        assert_eq!(
            got.len(),
            0,
            "stray envelope with mismatched recipient_id must not be dispatched",
        );
    }

    // FilenameError Display covers all variants.
    #[test]
    fn filename_error_display_all_variants() {
        assert_eq!(
            FilenameError::NotUtf8.to_string(),
            "filename is not valid UTF-8",
        );
        assert_eq!(
            FilenameError::Reserved.to_string(),
            "filename is empty, '.', or '..'",
        );
        assert!(
            FilenameError::ContainsNull
                .to_string()
                .contains("null byte"),
            "ContainsNull display should mention null byte: {}",
            FilenameError::ContainsNull,
        );
        let too_long = FilenameError::TooLong { len: 300 };
        let msg = too_long.to_string();
        assert!(
            msg.contains("300"),
            "TooLong display should contain the byte length: {msg}",
        );
        assert!(
            msg.contains(&MAX_INBOX_FILENAME_BYTES.to_string()),
            "TooLong display should contain the limit: {msg}",
        );
    }
}
