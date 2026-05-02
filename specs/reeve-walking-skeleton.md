# Reeve — Walking Skeleton

## Context

The first ladder in Reeve's build sequence (see `reeve-roadmap.md` for the full
sequence). The walking skeleton is the smallest end-to-end slice that proves the
architecture works: signed maildir transport, a runtime daemon supervising one
agent, a Claude adapter backing that agent, and a filesystem-only TUI the
operator can attach to and detach from.

This spec is the front door for ladder 1 — narrative, scope, and reading order.
It does not re-state design content; the seven sibling specs are canonical for
that.

## Narrative

You install Reeve on a clean laptop. No runtime; no agents; no operator
identity. You run `reeve`. The TUI prompts you for a display name and generates
an ed25519 keypair — the public half is written to
`~/.local/share/reeve/identities/<id>.toml`, the private half is stored in the
OS keychain.

The daemon starts in the background. It loads the default persona and team TOML,
resolves the lead persona's preferred model to the Claude adapter, and spawns
the lead. The TUI attaches to the lead's filesystem state and renders the chat
screen.

You type a message. The TUI signs an envelope with the operator key from the
keychain and atomic-renames it into `agents/lead/inbox/new/`. The runtime's
filesystem watcher fires; the envelope is verified; the delivery ledger is
updated; the file moves to `cur/`. The lead actor receives it, calls Claude, and
appends the response to its conversation thread. The TUI's watcher sees the file
change and renders the response within ~250ms.

You close the laptop. The TUI process exits with the terminal. The runtime
daemon keeps running, holding the lead in memory and the conversation on disk.

Next morning you open the laptop and run `reeve attach`. The TUI re-attaches,
reads the conversation from disk, renders the same thread. The lead is still
alive; the cost meter still reads what it read last night.

That is what ladder 1 ships.

## Scope

**In scope:** Cargo workspace, ed25519 identity and OS keychain integration,
signed canonical-JSON envelopes, maildir transport with verification and
ledgers, Claude adapter, actix-supervised runtime daemon, lead agent with
durable conversation thread, filesystem-only TUI, and a first-run experience
that orchestrates all of it.

**Out of scope** (deferred to later ladders per the ladder's `Notes` section):
capability profile / blacklist / classifier enforcement, cost ceiling
enforcement, adapter failover, memory subsystem, skills, configuration revision
flow, audit ring buffer, per-agent git worktrees, subordinate spawning,
panopticon, per-agent inspect.

## Reading Order

Before picking up any phase, read in order:

1. `reeve-overview.md` — product context and principles
2. `reeve-roadmap.md` — build sequence and load-bearing decisions
3. The sibling spec(s) most relevant to the phase:
   - **Identity, envelope, transport (phases 2–4):**
     `reeve-transport-security.md` + `reeve-domain-model.md`
   - **Adapter, runtime, agent (phases 5–7):** `reeve-domain-model.md`
   - **TUI (phase 8):** `reeve-tui-design.md` + `reeve-tui-screens.md`
4. `reeve-walking-skeleton.ladder.md` — the phase-by-phase plan

The ladder encodes the load-bearing constraints from the specs, but the specs
are the source of truth for design questions the ladder does not answer.
