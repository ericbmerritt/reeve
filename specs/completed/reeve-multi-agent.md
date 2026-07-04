# Reeve — Multi-Agent

## Context

The second ladder in Reeve's build sequence (see `reeve-roadmap.md`). The
walking skeleton proved the architecture — signed maildir transport, one
supervised agent, a model adapter, a filesystem-only TUI. The multi-agent
ladder makes the runtime do what it was designed for: the lead agent delegates
work to subordinates, agents exchange signed messages, and the operator watches
the estate from a panopticon screen.

This spec is the front door for ladder 2 — narrative, scope, and reading order.
It does not re-state design content; the sibling specs are canonical.

## Narrative

You have a running Reeve install with a lead agent. You type a task that
requires delegation. The lead calls the model; the model responds with a
`spawn_agent` tool call. The runtime's SpawnCoordinator provisions a subordinate
inbox, generates an ephemeral ed25519 identity for the new agent, registers it
in the identity registry, and starts it under the supervisor tree. The lead's
tool loop receives the result — the subordinate's name — and continues the
conversation, possibly calling `send_message` to deliver the initial task.

You switch to the panopticon with Tab. You see all running agents in a table:
name, persona, status, model, elapsed, cost. The new subordinate is there. You
press Enter on it; the per-agent inspect screen shows its conversation thread.
Pressing `h` returns you to the panopticon.

From a second terminal you run `reeve send --to <agent> --body "..."`. The
envelope is signed with your operator key and delivered directly to the agent's
inbox. The watcher picks it up, verifies it, and routes it to the correct actor.
The panopticon's event stream updates.

You notice a quarantined message — a message whose signature the runtime
rejected. You press `Q` from the panopticon to open the quarantine screen.
You read the envelope metadata and body, and press `d` to discard it.

That is what ladder 2 ships.

## The Tool Subsystem

The central new mechanism is the **tool execution loop** — the step between the
walking skeleton's degenerate curator and the full architecture described in
`reeve-actor-interior.md`. In ladder 1, `LeadAgent` fires a single adapter call
and records the response. In ladder 2 it runs a loop:

```
call adapter with tool descriptors
  → FinishReason::ToolUse: dispatch tool calls to tool actors, await results,
    push results onto history, call adapter again
  → FinishReason::EndTurn: record final response, go idle
```

Tools are actors that receive `InvokeTool` messages and reply with
`ToolResult` messages. The authority check — whether the calling agent is
permitted to use this tool — lives in the tool actor's message handler, not
in a separate mediating actor. In this ladder the check is always `Allow`; the
capability profile enforcement arrives in `reeve-authority` (ladder 3) and fills
in the same slot without changing the topology.

The two tools in this ladder are `spawn_agent` and `send_message`. Both dispatch
to purpose-built actors: `SpawnCoordinator` and `MessageDispatcher`.

## Agent Identity and Durability

Subordinate agents carry durable identities: an ed25519 keypair generated at
first spawn, private key written to `agents/<name>/identity.key` (mode 0600),
public key registered in the identity registry under type `Agent` with
`status: active`. The identity is stable across daemon restarts — the same key
file is loaded on each start, so the identity ID never changes for a given agent.

On daemon start, each agent loads its prior conversation history from
`conversation.jsonl` and reconstructs the in-memory context. A restarted agent
resumes mid-task rather than starting cold. Messages that arrived in `inbox/new/`
while the agent was stopped are delivered by the crash-recovery scan on restart.

The `AgentRegistry` on disk is cumulative: stopped agents remain visible with a
stopped status so the operator can inspect their full history from the TUI even
when they are not running.

## Scope

**In scope:** Tool execution loop, tool actor interface with authority check
slot, `spawn_agent` and `send_message` tools, `SpawnCoordinator` and
`MessageDispatcher` actors, ephemeral agent identity and registry, multi-agent
watcher routing, `reeve send` CLI, panopticon screen, per-agent inspect screen,
`reeve attach <name>` subcommand, quarantine review screen.

**Out of scope** (deferred to later ladders):

- Capability profile and blacklist enforcement — the authority check slot exists
  but always returns Allow. Enforcement arrives in `reeve-authority` (ladder 3).
- Bus tape — the causal record described in `reeve-actor-interior.md` is not
  implemented in this ladder. Journal entries are the durable record.
- The full curator architecture — working context, compression, satellites,
  small-model fallback. The degenerate loop from ladder 1 is extended with the
  tool loop only.
- Persistent agent identity — agent keypairs are regenerated on each daemon
  start.
- Panopticon pending decisions panel — requires classifier output from ladder 3.
- Memory review and configuration revision screens — ladders 5 and 6.
- Per-agent git worktrees — open question in domain-model § Open Questions.

## Reading Order

Before picking up any phase, read in order:

1. `reeve-overview.md` — product context and principles
2. `reeve-roadmap.md` — build sequence and key decisions
3. `reeve-domain-model.md` — entity model, invariants, boundaries
4. `reeve-actor-interior.md` — the target interior architecture this ladder
   moves toward; the tool loop and dispatcher sections are most relevant
5. `reeve-transport-security.md` — agent identity model, signing
6. `reeve-tui-screens.md` — panopticon, per-agent inspect, quarantine wireframes
7. `reeve-multi-agent.ladder.md` — the phase-by-phase plan
