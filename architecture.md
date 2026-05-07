# Reeve — Architecture

## The core idea: filesystem as the bus

The runtime, TUI, and external tooling all communicate through the same
Maildir primitives — no sockets, no RPC. A sender writes a signed JSON
envelope into `agents/<name>/inbox/tmp/`, then renames it atomically into
`inbox/new/`. The runtime's filesystem watcher fires, verifies the signature,
and delivers the message to the agent actor. The TUI watches the same directory
tree for conversation updates.

This gives atomic writes without coordination, durable at-least-once delivery
across runtime crashes, and a transport surface any local process can speak to
without linking against Reeve.

## Actor model

The runtime is a supervised actix actor tree. Each agent is an actor with:

- **inbox** — signed Maildir directory (tmp / new / cur / quarantine)
- **conversation journal** — append-only file the TUI reads directly
- **status file** — plain file updated by the runtime (idle / working / error)
- **cost meter** — token count and USD estimate, updated per model call

A panicking actor is restarted by its supervisor without quiescing the runtime.
Actors are location-transparent: senders address agents by name; the runtime
resolves name to actor ref. The same supervision pattern supports the walking
skeleton's single lead agent and the eventual multi-agent team.

## Identity and transport

Every message is an ed25519-signed JSON envelope. The operator key is generated
at enrollment and stored in the OS keychain (macOS Keychain or Linux Secret
Service). The runtime verifies signatures against a registry of known public
keys before delivery. Trust tier — operator, agent, external, untrusted — is
determined by verified sender identity, not by anything the message asserts
about itself. Untrusted envelopes are quarantined; the agent never sees them.

Signed envelopes are the only communication channel. There is no socket or RPC.

## The walking skeleton (ladder 1)

Ladder 1 is the smallest end-to-end slice: one lead agent, one operator, one
machine, Claude Opus 4.7 as the backing model. It establishes every structural
seam — keychain, maildir transport, actix supervision, filesystem TUI — that
later ladders extend.

Later ladders add subordinate agent spawning, the panopticon, per-agent memory,
forge personas that improve Reeve's own configuration, and the authority model
(capability profiles, blacklist, classifier).

## Key files

| Path                                | What it is                         |
| ----------------------------------- | ---------------------------------- |
| `specs/reeve-overview.md`           | Product context, principles        |
| `specs/reeve-roadmap.md`            | Build sequence, key decisions      |
| `specs/reeve-domain-model.md`       | Formal definitions, invariants     |
| `specs/reeve-actor-interior.md`     | Brainstem, curator, cognition      |
| `specs/reeve-transport-security.md` | Envelope format, trust tiers       |
| `specs/reeve-tui-design.md`         | TUI principles, access modes       |
| `specs/reeve-walking-skeleton.md`   | Ladder 1 narrative and scope       |
| `crates/reeve-cli/src/main.rs`      | CLI entry point, subcommand wiring |
| `crates/reeve-runtime/`             | Daemon, supervision tree, maildir  |
| `crates/reeve-tui/`                 | Filesystem-watching TUI            |
| `crates/reeve-adapter/`             | Claude adapter, message types      |
