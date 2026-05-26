# ADR 003 — Actor-system internals

## Context

The runtime needs a concurrency model for agent actors. Key requirements:

- An agent panic must not bring down other agents or the daemon.
- The operator can spawn, query, and stop agents at runtime.
- The model call loop (potentially seconds-long) must not block delivery of
  inbound messages to other agents.
- Restarting a crashed agent must be possible without re-reading all on-disk
  state from scratch.

The two main candidates were `tokio::task` (lightweight green threads, manual
supervision) and `actix` (actor framework with built-in supervision trees).

A separate decision: what is the durable channel between the runtime and the
TUI? Options included Unix sockets, shared memory, and the existing Maildir
filesystem.

## Decision

**actix for the actor system.** Each agent is an `actix::Actor` managed by a
supervisor. The supervisor restarts a panicking actor without touching
neighbours. `actix` provides location transparency (address-based message
dispatch), typed message handlers, and a lifecycle (`started`, `stopped`,
`stopping`) that maps cleanly onto agent states.

**Maildir spool as the runtime/TUI channel.** The TUI talks to the runtime
exclusively through the filesystem:

- The TUI reads `agents/<name>/log/conversation.jsonl` directly for
  conversation history.
- The TUI reads `agents/<name>/status` and `agents/<name>/cost` for live state.
- The TUI writes new operator messages as signed envelopes into
  `agents/<name>/inbox/tmp/` and renames them into `inbox/new/`.
- The runtime's `notify` watcher fires on any change; the TUI's own watcher
  fires on conversation/status/cost changes.

There is no socket, pipe, or shared-memory channel between the runtime daemon
and the TUI process.

`MAX_TOOL_ITERATIONS = 16` bounds the tool loop per reasoning cycle to prevent
runaway loops.

## Consequences

- **Crash isolation is structural.** A panicking `AgentActor` is restarted by
  its supervisor; the `SpawnCoordinator` and peer agents are unaffected.
- **The filesystem is the observable surface.** Any process with filesystem
  access — the TUI, shell scripts, external tools — can read agent state or
  send messages without linking against reeve. The `reeve send` subcommand
  exploits this.
- **TUI/daemon coupling is zero.** The TUI can attach to a running daemon, run
  without a daemon (read-only replay from journals), or be replaced entirely,
  because the protocol is files.
- **The Maildir channel is at-least-once.** Atomic rename into `inbox/new/`
  survives daemon crashes; the replay ledger prevents duplicate delivery on
  restart.
- **actix couples the runtime to a specific async executor.** Migrating away
  from actix would require replacing the supervision tree and all message
  handlers. The coupling is accepted because actix's supervision semantics are
  the central requirement.
- **`MAX_TOOL_ITERATIONS`** is a hard bound, not a soft hint. An agent whose
  model is stuck in a loop terminates the current cycle rather than running
  indefinitely.
