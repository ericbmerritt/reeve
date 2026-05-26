# ADR 001 — Single `ProcessInbound` dispatch gateway

## Context

`Watcher` delivers messages to agent actors via `ProcessInbound`. Two code
paths issued that message independently:

- `dispatch_verified_envelope` — called after a file is moved from `new/` to
  `cur/` and signature-verified. Checked `DeliveryLedger` before dispatching.
- `scan_cur_and_dispatch` — called at startup and on inotify events to
  re-dispatch files already in `cur/`. Did **not** check `DeliveryLedger`.

Because `scan_cur_and_dispatch` bypassed the ledger, messages in `cur/` could
be dispatched multiple times: once by `dispatch_verified_envelope` and again by
any subsequent `scan_cur_and_dispatch` run triggered by a filesystem event on
the same directory.

`SeenIds` was added to `Agent` as a band-aid: an in-memory FIFO set of seen
`message_id`s that prevented the agent from processing a duplicate even if the
watcher sent one. This introduced a second source of truth for delivery dedup —
the durable `DeliveryLedger` and the ephemeral `SeenIds` — with different
failure modes and different restart semantics.

## Decision

All `ProcessInbound` messages are issued through a single private function in
`Watcher` (`deliver`). No code outside that function calls
`do_send(ProcessInbound { ... })` directly. The function owns the full delivery
sequence: check `DeliveryLedger`, record delivery, append the audit event,
dispatch the message. New code that needs to dispatch a message to an agent must
go through `deliver`; it does not get its own ledger check.

`SeenIds` is removed from `Agent` entirely. The watcher's single dispatch
gateway is the sole dedup gate.

## Consequences

- Delivery dedup has one implementation and one failure mode. Restarts are
  handled correctly: `DeliveryLedger` is on-disk, so a message already
  delivered before a crash is not re-dispatched after restart.
- Adding a new dispatch path requires threading `Arc<DeliveryLedger>` and
  `Arc<AuditLog>` to the call site — this is intentional friction that makes
  the constraint visible to the implementer.
- `Agent` carries less in-memory state. The agent does not need to remember
  what it has seen; the watcher guarantees at-most-once dispatch.
- Test code that needs to send a `ProcessInbound` directly (bypassing the
  watcher) remains valid — the constraint is on watcher-internal code, not on
  test harnesses that construct messages for injection.
