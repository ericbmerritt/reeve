# ADR 002 — Tool-actor trust boundary

## Context

Reeve agents invoke tools (filesystem reads, shell commands, model calls) as
part of their reasoning loop. The question is where the authority check for
tool invocations should live and how tool actors communicate with the agent.

Two candidates were considered:

**Option A — Mediating actor.** A dedicated `ToolGateway` actor sits between
the agent and every tool actor. All `InvokeTool` messages pass through it; the
gateway enforces the capability profile before forwarding to the tool.

**Option B — Tool actor's own handler.** The tool actor's message handler
performs the authority check directly on receipt of `InvokeTool`. No
intermediary.

The question of communication channel also arose: should tool actors use the
envelope protocol (signed Maildir) or actix messages (in-process)?

## Decision

**Option B.** Authority checks live in the tool actor's message handler, not
in a separate mediating actor. The topology is `Agent → ToolActor` with no
intermediary.

`InvokeTool.sender_id` is the authority check token. Its type and position in
the message are stable: future ladders add enforcement logic to the handler
without changing the message shape.

**Tool actors use actix messages internally**, not the envelope protocol. The
envelope protocol governs cross-agent boundaries — messages that cross process
or trust-tier boundaries and therefore need a durable, signed, replay-protected
channel. In-process tool calls satisfy none of those requirements: they are
synchronous, local, and already within the agent's supervision tree. Adding
envelope overhead to intra-process calls would impose signing, serialization,
and Maildir I/O costs for zero security gain.

## Consequences

- Tool actors are simpler: a single `Handler<InvokeTool>` impl, no routing
  layer.
- Adding a new tool means implementing `Handler<InvokeTool>` and registering
  with the supervisor — one location, not three.
- `InvokeTool.sender_id` is the sole authority token. Enforcement logic added
  in later ladders (capability profiles, blacklists) plugs into the handler at
  that one point.
- The architectural boundary is clear: envelope protocol for cross-agent
  messages, actix messages for intra-agent tool calls. This boundary is stable
  and does not change when enforcement is added.
- The absence of a gateway actor means there is no single choke-point for
  cross-cutting concerns (rate limiting, audit sampling). Those are addressed
  per-tool or per-agent rather than centrally.
