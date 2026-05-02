# Reeve — Roadmap

## Purpose

Build sequence for Reeve. The sibling specs describe design; this document
describes the order in which design becomes running code. Each ladder is a
vertical slice that ends with something the operator can try.

This is a thin sequencing artifact, not a re-statement of the specs.

## Slicing Principles

**Vertical, not horizontal.** Each ladder advances the "Day With Reeve" story by
one observable step. We do not build the whole runtime, then the whole TUI, then
the whole security layer. Each ladder cuts through the stack and ships something
runnable.

**Security from day one.** The transport security model — signed envelopes, key
registry, trust tier resolution, quarantine — is part of ladder 1. Trust tier is
the transport's contract; shipping an unsigned stub means shipping the directory
layout without the actual model. Retrofitting security onto an already-shipped
transport is a smell we are not paying for.

**Specs implemented across ladders, not within them.** Most sibling specs
(domain model, transport security, TUI design, TUI screens) are touched by
multiple ladders. A ladder takes the thinnest cut through whatever subsystems
are needed for its demo.

## Sequence

| #   | Ladder                    | What's new                                                                                                                                                                                   | Demo at end                                                                                                      | Touches specs                                             |
| --- | ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------- |
| 1   | `reeve-walking-skeleton`  | Workspace, supervisor + actor runtime, Claude adapter, agent FS layout, signed maildir transport with key registry + verification + trust tiers + quarantine, TUI lead chat, detach/reattach | Talk to lead from TUI; close, reopen, conversation persists; tampered message goes to quarantine                 | domain-model, transport-security, tui-design, tui-screens |
| 2   | `reeve-multi-agent`       | Lead spawns subordinates, each with own keypair and inbox, peer messaging falls out, `reeve send` CLI, panopticon screen, per-agent inspect, quarantine review screen                        | Ask lead to delegate; agents work in parallel; dispatch from a shell script; observe quarantine on bad signature | domain-model, transport-security, tui-design, tui-screens |
| 3   | `reeve-authority`         | Capability profile per persona, blacklist, classifier-passthrough scaffold, audit-log surfacing of decisions                                                                                 | Persona without `git_commit` refuses; blacklisted force-push hard-refused                                        | domain-model, tui-design                                  |
| 4   | `reeve-gatekeeper`        | Content classifier at promotion boundaries, pass/flag/block disposition, gatekeeper events in audit log                                                                                      | Prompt-injection in a file read gets flagged in the panopticon                                                   | gatekeeper-model, tui-design, tui-screens                 |
| 5   | `reeve-memory`            | Project, persona, operator memory scopes; cold-start core; queryable store; memory review screen; reference observability                                                                    | Add a note to a persona; next-spawned agent picks it up; see which entries are referenced                        | domain-model, tui-design, tui-screens                     |
| 6   | `reeve-skills-versioning` | Skill bundles, persona / skill / memory versioning, version attribution on every event, config revision review screen                                                                        | Edit persona → version bumps → next spawn uses new version; running agents stay on old                           | domain-model, tui-design, tui-screens                     |
| 7   | `reeve-shipped-teams`     | Default working team and forge team, seed memories, first-run experience                                                                                                                     | Fresh install boots with a working team; forge team self-improves from observability data                        | shipped-teams                                             |

## Status

| Ladder                    | Status      |
| ------------------------- | ----------- |
| `reeve-walking-skeleton`  | in planning |
| `reeve-multi-agent`       | not started |
| `reeve-authority`         | not started |
| `reeve-gatekeeper`        | not started |
| `reeve-memory`            | not started |
| `reeve-skills-versioning` | not started |
| `reeve-shipped-teams`     | not started |

## Key Decisions

- **Signing is in ladder 1.** Shipping the maildir without verification means
  shipping the directory layout but not the trust contract. Every integration
  would form around the unsigned shape and have to be retrofitted later. Sign
  from the first message.
- **Peer messaging is not its own ladder.** Once subordinates exist in ladder 2,
  peer dispatch is the same maildir mechanism with a different sender keypair.
  The transport does not change; only the senders do.
- **`cur/` rotation from day one.** Maildir's `cur/` is an in-flight buffer, not
  a durable store. The append-only conversation log is the durable history.
  `cur/` is rotated post-integration to keep directory size bounded and
  filesystem churn bounded with it.
- **Maildir as transport boundary is non-negotiable.** The "no client library;
  any local process can write a signed message" property is load-bearing for
  Reeve's openness. Replacing maildir with SQLite or a custom queue file
  destroys that property in exchange for filesystem-churn savings that do not
  manifest at workstation scale.
- **Filesystem is the TUI ↔ runtime protocol.** The TUI is a privileged file
  reader plus a watcher plus a signed-envelope writer. There is no socket, no
  RPC, no REST. The runtime maintains canonical state on disk (status,
  conversation log, cost meter, audit log); the TUI reads it directly, watches
  it via inotify/kqueue, and submits messages by signing and atomic-renaming
  envelopes into `inbox/new/` — exactly the path any external sender takes.
  Liveness via a `runtime/heartbeat` file the runtime touches periodically. This
  extends the maildir-as-transport principle through the operator surface;
  adding a socket protocol would be the inconsistent choice.
- **Canonical envelope serialization is canonical JSON (RFC 8785-style).**
  Debuggable on disk, ed25519-dalek pairs cleanly, schema evolution is
  straightforward. CBOR is the runner-up if compactness ever matters; for
  workstation use it does not.

## Non-Goals For This Roadmap

- Not a release plan. No dates, no versions.
- Not a re-statement of the specs. If this document and a sibling spec disagree
  on design, the sibling spec wins. This document is canonical for sequence
  only.
- Covers the first shippable trajectory. Multi-machine, cloud-managed, model
  routing, and other non-goals from the overview remain non-goals.
