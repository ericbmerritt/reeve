# ADR 005 — `actix::Supervisor` restart is gated on mailbox connectivity, not `stop()` vs `terminate()`

## Context

ADR 003 states "The supervisor restarts a panicking actor without touching
neighbours" and treats `Agent`'s `ctx.terminate()` (used in
`transition_to_stopped`) as a permanent, non-restartable stop, distinct from
`ctx.stop()`. A code comment made this explicit: "Use terminate() rather than
stop() so that actix::Supervisor does not invoke restarting()."

That assumption is false for `actix` 0.13.5. Reading
`actix::Supervisor::poll` (`supervisor.rs`) and `ContextFut::restart`
(`context_impl.rs`) shows the restart decision is gated on one thing only:
`self.mailbox.connected()` — whether any `Addr`/`Recipient` handle to the
actor is still alive anywhere in the process. `ContextParts::restart()` also
clears the `STARTED` flag on every restart, so the next poll calls
`Actor::started()` again, not just `Supervised::restarting()`.

Two independent tables held a `Recipient` to every agent for its whole
lifetime with no removal on stop: `Watcher::routing_table` (keyed by
`IdentityId`, used for `ProcessInbound` dispatch) and `estate`'s
`control_routes` (keyed by name, used for `Retire` dispatch). Because an
agent's own `transition_to_stopped` never freed either entry, the mailbox
stayed connected forever after a `Retire` or threshold-trip stop. The result:
`started()` → `set_idle()` → sees `exiting == true` → `transition_to_stopped()`
→ registry flush (`File::sync_all`, `F_FULLFSYNC` on macOS) → `ctx.terminate()`
→ Supervisor restarts anyway → repeat, synchronously, with no yield point —
an infinite busy loop that saturates disk I/O for the whole machine. This
surfaced as two estate tests hanging 50+ minutes under a full-disk-fsync
storm.

## Decision

`stop()` vs `terminate()` is not a restart-prevention mechanism in this actix
version and must not be treated as one. The only way to stop `Supervisor`
from restarting an actor is to ensure no `Recipient`/`Addr` handle to it
survives anywhere.

`Agent::transition_to_stopped` is the single place that unregisters both
routes on every terminal transition (`Watcher::unregister_route`,
`ControlRoutes::unregister`), and is made idempotent (a `stopped` flag) so a
restart that manages to land before the routes are pruned does not repeat the
registry flush.

## Consequences

- ADR 003's crash-isolation claim ("supervisor restarts a panicking actor")
  is still correct for actual panics — a fresh `Addr` is never re-registered
  for a panicked actor by callers, so it stays restartable only as long as
  something still holds a route to it, which is the desired behavior for
  genuine crash recovery. What was wrong was treating a _clean, intentional_
  stop as automatically non-restartable; it isn't, unless the routes are
  explicitly dropped.
- Any future long-lived table that holds a `Recipient`/`Addr` to an agent
  (not just the two known today) must prune its entry on that agent's
  terminal transition, or it silently re-introduces this failure mode for
  whatever event stream feeds that table.
- `agent.rs`'s and `watcher.rs`'s doc comments on `ControlRoutes`,
  `Watcher::unregister_route`, and `transition_to_stopped` carry this
  explanation directly, since it is not recoverable from reading either file
  in isolation.
