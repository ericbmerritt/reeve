# Architecture

## Filesystem as the bus

The runtime, TUI, and external tooling communicate through Maildir primitives.
A sender writes a signed JSON envelope into `agents/<name>/inbox/tmp/`, then
renames it atomically into `inbox/new/`. The runtime's filesystem watcher
fires, verifies the signature, and delivers the message to the agent actor. The
TUI watches the same directory tree for conversation updates.

Atomic renames give at-least-once delivery without coordination. The replay
ledger prevents duplicate processing across restarts. Any local process that
can write files can participate in the protocol.

## Actor topology

The runtime is a supervised actix actor tree.

```text
Supervisor
├── AgentActor ("lead")
│   ├── WatcherActor  — verifies + delivers inbound envelopes
│   └── ToolActors    — one per registered tool
├── AgentActor ("worker-abc12345")
│   └── ...
└── SpawnCoordinator  — handles spawn_agent tool calls
```

Each agent holds:

- **Inbox** — signed Maildir directory (`tmp/`, `new/`, `cur/`, `quarantine/`)
- **Conversation journal** — append-only JSONL the TUI reads directly
- **Status file** — plain file updated by the runtime (`idle`, `working`,
  `crashed`)
- **Cost meter** — token count and USD estimate, updated per model call

Tools communicate with the agent via actix messages internally. The envelope
protocol governs cross-agent boundaries; there is no envelope exchange for
in-process tool calls. See
[ADR 002](./decisions/002-tool-actor-trust-boundary.md).

## Identity model

Every principal — operator and agent alike — holds an Ed25519 keypair and a
`UUIDv7` identity ID. The operator's key lives in the OS keychain; agent keys
live in `agents/<name>/identity.key` (mode 0600). All envelopes carry
`sender_id`, `sender_key_id`, `recipient_id`, and an Ed25519 signature.

The watcher verifies signatures before delivery. Messages from unregistered
senders, with invalid signatures, replayed nonces, or excessive clock skew go
to `quarantine/` rather than `cur/`. The TUI's quarantine review screen lets
the operator inspect and discard these.

## Session memory and startup

`reeve` and `reeve attach` (no argument) consult
`~/.local/state/reeve/session.toml` at startup. If the last-chatted agent is
still running, the TUI opens on that agent's chat screen; otherwise it opens
the panopticon. The session file is written on TUI exit from a chat screen.

## Decisions

- [ADR 001](./decisions/001-single-processInbound-dispatch.md) — Single
  `ProcessInbound` dispatch gateway
- [ADR 002](./decisions/002-tool-actor-trust-boundary.md) — Tool-actor trust
  boundary
- [ADR 003](./decisions/003-actor-system-internals.md) — Actor-system
  internals
