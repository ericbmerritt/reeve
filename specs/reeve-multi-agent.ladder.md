# reeve-multi-agent — Ladder

## Phase 1: Tool execution loop

| Status      | Started    | Completed  |
| ----------- | ---------- | ---------- |
| ✅ complete | 2026-05-08 | 2026-05-08 |

Tags: runtime, adapter

The tool execution loop is the load-bearing new mechanism. In ladder 1, the lead
agent fires a single adapter call and records a text response. This phase
replaces that with a loop that handles `FinishReason::ToolUse` responses: the
adapter returns tool calls, the agent dispatches them to tool actors, collects
results, pushes them onto history, and calls the adapter again — repeating until
`FinishReason::EndTurn`.

**Adapter layer.** `MessageContent` in `reeve-adapter` gains two new variants:
`ToolUse { id, name, input }` to represent a tool call in the conversation
history (assistant turn), and `ToolResult { tool_use_id, content, is_error }` to
represent the result pushed back as a user turn. The Anthropic adapter must
serialize and deserialize these correctly. Both variants are required for the
provider to maintain tool-call attribution across multi-turn tool loops.

**Runtime message types.** `InvokeTool` and `ToolResult` are new message types
in `reeve-runtime`. `InvokeTool` carries: `tool_use_id` (the provider-assigned
ID echoed from the adapter response), `name`, `input` (JSON value), `sender_id`
(`IdentityId` of the invoking agent), and `reply_to`
(`actix::Recipient<ToolResult>`). `ToolResult` carries: `tool_use_id`, `content`
(string), `is_error` (bool). The `sender_id` field is the authority check slot —
present and structurally correct in this phase, always returning Allow.

**Tool actor trait.** A `ToolActor` trait (or convention) declares `descriptor()
-> reeve_adapter::Tool` for the adapter to know the tool's name, description,
and input schema. Each concrete tool actor implements `Handler<InvokeTool>` and
checks authority (always Allow now) before executing. Tool actors are supervised;
a crashed tool actor is restarted by the supervisor without quiescing the agent.

**Loop in LeadAgent.** The adapter call in `LeadAgent` is replaced by the tool
loop. The loop pushes parallel tool calls onto an `InvokeTool` future set, waits
for all results, assembles them into a tool-result message, appends to history,
and calls the adapter again. Loop depth is bounded: more than
`MAX_TOOL_ITERATIONS` (16) consecutive ToolUse responses without an EndTurn
causes the loop to abort, append a system entry, and return to idle.

**Echo tool.** A no-op `EchoTool` actor is wired for this phase. Its descriptor
declares a single string argument; its handler returns the argument unchanged as
the result. It is used to verify the complete loop end-to-end (adapter returns
ToolUse → EchoTool executes → result returned to adapter → EndTurn) without
needing real tool implementations. `EchoTool` is wired into the production
daemon as well so the operator can exercise the loop manually from the TUI;
both wirings are removed when `spawn_agent` and `send_message` land in phase 3.

#### Delivers

- `MessageContent::ToolUse` and `MessageContent::ToolResult` variants in
  `reeve-adapter`, with Anthropic adapter serialization
- `InvokeTool` and `ToolResult` message types in `reeve-runtime`
- Tool actor convention: `descriptor()` + `Handler<InvokeTool>` with authority
  check slot (always Allow)
- Tool execution loop in `Agent` with `MAX_TOOL_ITERATIONS` bound and a
  per-batch `TOOL_TIMEOUT` watchdog
- `EchoTool` actor wired into the production daemon for both end-to-end
  testing and operator-driven manual verification

#### Done When

- Given a mock adapter that returns `FinishReason::ToolUse` with an echo tool
  call, when the lead receives a message, then the tool loop fires, `EchoTool`
  is invoked, the result is pushed to history, and the adapter is called a second
  time — verified by inspecting the conversation journal entries
- Given the mock adapter returns `FinishReason::EndTurn` on the second call,
  then status returns to `idle` and the journal has: inbound, tool\_use,
  tool\_result, outbound, model\_call entries
- Given a mock adapter that always returns `FinishReason::ToolUse`, when the
  loop hits `MAX_TOOL_ITERATIONS`, then the agent appends a system entry with the
  abort reason and returns to idle without calling the adapter a 17th time
- `MessageContent::ToolUse` and `ToolResult` round-trip through the Anthropic
  adapter's serialization (unit test with golden JSON)
- `InvokeTool.sender_id` is present and correctly typed in all tool actor
  invocations (asserted in the EchoTool test handler)

#### Depends On

- walking-skeleton (all phases)

## Phase 2: Agent identity and runtime registry

| Status      | Started    | Completed  |
| ----------- | ---------- | ---------- |
| ✅ complete | 2026-05-09 | 2026-05-09 |

Tags: runtime, security, identity

Subordinate agents need identities so their signed envelopes pass verification,
and the runtime needs a registry to route verified messages to the correct actor.
This phase adds both without yet adding the tools that create agents. Durability
is the design constraint: the identity, conversation context, and registry must
survive daemon crashes and clean restarts so agents can be resumed and their
history inspected.

**Durable agent identity.** At spawn time the runtime generates an ed25519
keypair for the agent. The public key is registered in the identity registry
under identity type `Agent` with `status: active`. The private key is written to
`agents/<name>/identity.key` with mode 0600 — not in the OS keychain (agents are
not operator-tier principals), but persisted so the identity survives restarts.
On daemon startup, agents listed in the on-disk registry whose key file exists
are loaded with their stored keypair and their identity ID is unchanged. Identity
IDs are stable across restarts for as long as the key file exists.

**Conversation history on start.** When an agent actor starts (including after a
daemon restart), it reads its `conversation.jsonl` and reconstructs the
in-memory `history: Vec<Message>` from it. Inbound entries become user-role
messages; outbound entries become assistant-role messages; system and model\_call
entries are skipped. The agent resumes with the full prior context, ready to
continue a task.

**AgentRegistry.** The runtime maintains an `AgentRegistry` mapping agent name →
`AgentRecord { identity_id, inbox, addr, keypair, persona_name, spawned_at,
status }`. The registry is written to disk as
`~/.local/share/reeve/agents/registry.toml` on every mutation. The on-disk
format is a TOML array of records — everything except the runtime-only `addr`
and `keypair` fields. The registry is **cumulative**: records are never removed
on shutdown or crash; a stopped agent's record remains with `status: stopped`.
On daemon start the registry is loaded from disk; agents with existing key files
and `status: running` are restarted automatically (the lead always; subordinates
on explicit restart or future auto-restart policy).

**Stopped agents in the TUI.** Because the registry is cumulative, the
panopticon can show historical agents with a stopped sigil (`✓` exited cleanly,
`✗` crashed). The operator can navigate to a stopped agent's inspect screen and
read its full conversation history from disk, even when the agent is not running.

**Multi-agent watcher routing.** The watcher's hardcoded `Addr<LeadAgent>`
delivery is replaced with a routing table backed by the `AgentRegistry`. After
verification, the watcher looks up `recipient_id` in the registry to find the
correct `Recipient<ProcessInbound>`. Running agents receive the message;
messages addressed to stopped agents are held in `inbox/new/` and delivered when
the agent is restarted (the existing crash-recovery scan handles this). Envelopes
addressed to an identity ID not in the registry at all are quarantined with
reason `recipient_not_found`.

**Lead agent registration.** The lead agent's transient-per-boot identity from
ladder 1 is replaced with a durable identity under the same scheme. On first
run, the lead's keypair is generated and persisted. On subsequent runs, it is
loaded from disk. The lead appears in the registry under name `"lead"`.

#### Delivers

- `AgentRecord` type and `AgentRegistry` in `reeve-runtime`
- Per-agent private key persisted to `agents/<name>/identity.key` (mode 0600)
- Agent private key loaded from disk on daemon start if present
- Conversation history loaded from `conversation.jsonl` into in-memory history
  on agent start
- `AgentRegistry` written to `~/.local/share/reeve/agents/registry.toml` on
  mutation; cumulative across restarts with `status` field
- Multi-agent watcher routing via `AgentRegistry` lookup by `recipient_id`
- Lead agent migrated to durable identity scheme

#### Done When

- Given a running agent that has exchanged messages, when the daemon is killed
  and restarted, then the agent resumes with its full prior conversation history
  in memory (verified by sending another message and confirming the adapter
  receives the complete prior context)
- Given the daemon restarts, the agent's `identity_id` is unchanged (same key
  file loaded; same public key in registry)
- Given two registered agents with distinct `recipient_id` values, when a signed
  envelope arrives in each agent's inbox, then the watcher delivers to the
  correct actor (verified by inspecting each agent's conversation journal)
- Given an envelope whose `recipient_id` does not match any registry entry, then
  the watcher moves it to `quarantine/` with reason `recipient_not_found`
- Given a daemon crash while a subordinate is running, when the operator
  inspects the registry and the TUI, then the subordinate appears as stopped and
  its full conversation history is readable from the inspect screen
- Messages in a stopped agent's `inbox/new/` are delivered when that agent is
  restarted (crash-recovery scan picks them up)
- `AgentRegistry` on disk contains all ever-spawned agents with their status;
  the lead appears under name `"lead"` after first daemon start

#### Depends On

- tool-execution-loop

## Phase 3: SpawnCoordinator and spawn\_agent tool

| Status      | Started    | Completed  |
| ----------- | ---------- | ---------- |
| ✅ complete | 2026-05-09 | 2026-05-10 |

Tags: runtime, agent

The `SpawnCoordinator` actor and the `spawn_agent` tool wire together the
tool loop (phase 1) and the agent identity machinery (phase 2) into the first
autonomous agent spawn.

**SpawnCoordinator actor.** A supervised actix actor that handles
`SpawnRequest { persona_name, display_name, system_prompt, reply_to }` messages.
On receipt it: provisions the agent's directory tree, generates a durable
identity (keypair generated and persisted per the phase 2 scheme), registers it
in the `AgentRegistry`, starts the new agent under the supervisor tree, and
replies with `SpawnResponse { agent_name, agent_id }`. The coordinator is
started by the daemon at launch and is available to any tool actor that holds its
address. A spawn that fails (persona not found, provisioning error) replies with
an error result; no partial state is left behind.

**spawn\_agent tool.** A tool actor that wraps `SpawnCoordinator`. Its
descriptor declares: `persona` (string, required) — the persona to load;
`task` (string, required) — initial instruction forwarded to the agent as a
system-prompt annotation; `context` (string, optional) — additional context
appended to the system prompt. The handler sends a `SpawnRequest` to the
coordinator, awaits the reply, and returns the new agent's name as the tool
result content. The `sender_id` in `InvokeTool` is recorded in the spawn
snapshot for audit purposes.

**EchoTool removal.** The `EchoTool` from phase 1 is removed; the loop is now
exercised by `spawn_agent` in integration tests.

#### Delivers

- `SpawnCoordinator` actor with `SpawnRequest` / `SpawnResponse` message types
- `spawn_agent` tool actor wired into the lead agent's tool set
- `SpawnCoordinator` address injected into the lead agent at startup
- EchoTool removed
- Integration test: lead receives a message → model returns `spawn_agent` call →
  coordinator provisions agent → result returned → agent appears in registry

#### Done When

- Given the lead receives a message and the adapter returns a `spawn_agent` tool
  call, when the tool loop executes, then a new agent directory tree exists under
  `agents/`, the agent appears in `AgentRegistry`, and the lead's journal
  contains inbound, tool\_use, tool\_result (with agent name), outbound entries
- Given `persona_name` does not match any installed persona, then the tool
  result carries `is_error: true` and the coordinator leaves no partial state
- Spawned agent's `status` file reads `idle` within 5 seconds of spawn
- Spawned agent's identity appears in the identity registry with type `Agent`
  and status `active`
- `spawn_agent` tool descriptor validates via Anthropic's tool schema rules
  (round-trip test: serialize descriptor, parse back, fields match)

#### Depends On

- agent-identity-and-registry

## Phase 4: MessageDispatcher and send\_message tool

| Status      | Started    | Completed  |
| ----------- | ---------- | ---------- |
| ✅ complete | 2026-05-10 | 2026-05-22 |

Tags: runtime, transport

Agents need to send signed messages to each other. The `MessageDispatcher` actor
handles delivery: it looks up the recipient in the `AgentRegistry`, signs the
envelope with the sender's in-memory key, and atomic-renames it into the
recipient's `inbox/new/`. The `send_message` tool exposes this to the model.

**MessageDispatcher actor.** Handles `SendMessage { from_id, to_name, body,
reply_to }` messages. Looks up `to_name` in the `AgentRegistry`; if not found,
replies with an error. Looks up the sender's `AgentRecord` to retrieve its
in-memory keypair. Signs an envelope with the sender's key, addressed to the
recipient's `identity_id`. Writes to `recipients/inbox/tmp/`, then
atomic-renames into `inbox/new/`. Replies with `SendResult { message_id }` on
success. The dispatcher is supervised; a crash does not lose the sender's key
(key lives in `AgentRegistry`, not the dispatcher).

**send\_message tool.** Descriptor declares: `to` (string, required) — recipient
agent name; `body` (string, required) — message body. Handler sends `SendMessage`
to the dispatcher and returns the `message_id` as the tool result. The
`sender_id` from `InvokeTool` is passed through as `from_id`.

**Cross-agent delivery.** With both phases 3 and 4 in place, the full
agent-to-agent flow works: lead spawns a subordinate via `spawn_agent`, sends it
a task via `send_message`, the watcher routes the envelope to the subordinate's
actor, the subordinate processes it and calls the model, the subordinate replies
to the lead via `send_message` in its own tool loop.

#### Delivers

- `MessageDispatcher` actor with `SendMessage` / `SendResult` message types
- `send_message` tool actor wired into all agents' tool sets (lead and
  subordinates receive the same tool set at spawn)
- `MessageDispatcher` address injected into all agent actors at startup
- Integration test: agent A sends to agent B via `send_message`; B's journal
  contains the inbound entry from A's identity

#### Done When

- Given agent A sends a `send_message` to agent B, when the tool executes, then
  B's `inbox/new/` receives a signed envelope, the watcher delivers it to B, and
  B's journal records the inbound entry with A's `sender_id`
- Given `to` names an agent not in the registry, then the tool result carries
  `is_error: true` and no file is written
- The signed envelope passes the watcher's full verification pipeline
  (signature, replay, recipient-match) — verified by replaying the test with the
  standard watcher
- A `message_id` returned by the tool appears in B's delivery ledger after
  delivery

#### Depends On

- spawn-agent-tool

## Phase 5: reeve send CLI

| Status    | Started | Completed |
| --------- | ------- | --------- |
| not started |         |           |

Tags: cli

The `reeve send` subcommand lets the operator deliver a signed message to any
running agent from a shell, without attaching the TUI. It uses the same
envelope and delivery path as agent-to-agent messaging — the operator signs with
their key, the watcher verifies and routes.

**Subcommand.** `reeve send --to <agent-name> --body <text>`. Reads the
`AgentRegistry` from disk to resolve `agent-name` to an `identity_id`. Loads
the operator key from the OS keychain. Signs an envelope addressed to the
agent's `identity_id`. Writes to `inbox/tmp/`, atomic-renames to `inbox/new/`.
Prints `sent: <message_id>` on success.

**Shell dispatch use case.** A shell script can loop over a list of tasks and
call `reeve send` for each, dispatching work to a named agent without operator
involvement beyond the initial script. This exercises the "dispatch from a shell
script" demo scenario from the ladder 2 roadmap entry.

#### Delivers

- `reeve send --to <name> --body <text>` subcommand
- `AgentRegistry` reader in `reeve-cli` (reads the on-disk TOML, no socket)
- Delivery path: operator key → signed envelope → `inbox/new/`

#### Done When

- `reeve send --to lead --body "hello"` delivers a verified envelope to the lead
  agent; the lead's journal records the inbound entry; `just validate` passes
- Given the target agent name is not in the registry, the command exits with a
  clear error and writes no file
- The sent message passes the watcher's full verification pipeline

#### Depends On

- message-dispatcher-tool

## Phase 6: Panopticon as home screen

| Status    | Started | Completed |
| --------- | ------- | --------- |
| not started |         |           |

Tags: tui

The panopticon becomes the primary entry point. On startup, `reeve` and
`reeve attach` (no argument) land on the panopticon rather than dropping
directly into the lead chat. This removes the implicit privilege of the lead
agent — all agents are peers in the list, and the operator chooses who to talk
to. The panopticon shows all registered agents (running and stopped), a recent
event stream, and queue counts. The pending decisions panel is present but empty
(capability enforcement is ladder 3). Navigation follows `reeve-tui-screens.md §
Panopticon home`.

**Session memory.** `~/.local/state/reeve/session.toml` records the name of the
last agent the operator conversed with (`last_agent = "lead"`). On startup, if
the recorded agent is present in the registry and running, the TUI opens
directly to that agent's chat screen. From there, `Tab` opens the panopticon.
If no session record exists, or the last agent is stopped or absent, the TUI
opens on the panopticon instead. The session file is written on TUI exit
whenever the operator was in a chat screen.

**Startup flow change.** `cmd_reeve()` in `reeve-cli` is updated: after daemon
start, it reads the session file and either attaches to the last-session agent
or opens the panopticon. `reeve attach` with no argument follows the same logic.
`reeve attach <name>` continues to open that agent's chat directly.

**Data sources.** All reads go through the filesystem. The agent table is built
from the on-disk `AgentRegistry` plus per-agent `status` and `cost` files.
Stopped agents appear with a stopped sigil (`✓` clean exit, `✗` crash). The
recent events stream is assembled from the most recent lines of each agent's
`conversation.jsonl`, merged by timestamp. Queue counts read file counts in each
agent's `quarantine/`. The TUI watcher covers the `AgentRegistry` file and all
registered agents' state directories.

**Quiet state.** Per the wireframe: agent table with columns AGENT, PERSONA,
STATUS, MODEL, ELAPSED, COST, ACTIVITY. The `▶` cursor marks the focused row.
`j/k` navigate rows; `Enter` opens the focused agent's chat; `Tab` cycles
between regions.

**Pending decisions panel.** Rendered but empty in this ladder. The `▲ N`
title bar indicator is suppressed when the count is zero. Approve/block actions
arrive in ladder 3.

**Quarantine count.** The `Q N quarantine` counter reflects files across all
agents' `quarantine/` directories. `Q` opens the quarantine review screen
(phase 8).

#### Delivers

- Panopticon screen in `reeve-tui` with agent table (running + stopped),
  event stream, queue counts
- TUI watcher covering `AgentRegistry` and all agents' state dirs
- `reeve` and `reeve attach` (no arg) open panopticon or last-session agent
- Session memory at `~/.local/state/reeve/session.toml`; written on TUI exit
- `Enter` on an agent row opens that agent's chat
- `Tab` from any chat opens panopticon; `Tab` from panopticon cycles regions
- `Q` opens quarantine review
- Pending decisions panel rendered but empty

#### Done When

- `reeve` with no prior session opens the panopticon; with a prior session
  whose agent is running, opens that agent's chat directly
- `reeve attach` (no arg) follows the same session-memory logic
- Given two running agents and one stopped agent, all three appear in the
  panopticon table with correct sigils; the stopped agent shows its last-known
  status
- Given an agent's `status` file changes to `"working"`, the panopticon
  re-renders within ~250ms
- Given a file in any agent's `quarantine/`, the `Q N` counter is correct
- Quitting the TUI from a chat screen writes `last_agent` to the session file;
  the next `reeve` invocation opens that agent's chat
- `NO_COLOR=1` and 80×24 terminal: panopticon is fully readable (smoke test)

#### Depends On

- reeve-send-cli

## Phase 7: Per-agent inspect and reeve attach

| Status    | Started | Completed |
| --------- | ------- | --------- |
| not started |         |           |

Tags: tui, cli

The per-agent inspect screen shows a subordinate's conversation thread. The
Thread tab (default) is the primary view; the other tabs (Tools, Model,
Decisions, Memory) are stub placeholders that show `not yet available` in this
ladder. `reeve attach <name>` opens the inspect screen directly for a named
agent.

**Inspect screen.** Per `reeve-tui-screens.md § Per-agent inspect`: title bar
shows agent name, persona, model, status, elapsed, cost. The Thread tab renders
the conversation journal entries with speaker attribution. `h` or `Esc` returns
to the panopticon. Tab cycles to the next stub tab. In this ladder, authority
decision entries and memory-reference entries in the journal are rendered as
plain system entries.

**`reeve attach <name>`.** Extends the existing `reeve attach` subcommand to
accept an optional agent name. With no argument, follows the session-memory
logic introduced in phase 6 (panopticon or last-session agent). With a name,
opens that agent's chat screen directly. Looks up the agent in the on-disk
`AgentRegistry`; if absent, prints a clear error.

#### Delivers

- Per-agent inspect screen (Thread tab active; Tools/Model/Decisions/Memory tabs
  stub)
- `Enter` from panopticon opens inspect for the focused agent
- `h` / `Esc` from inspect returns to panopticon
- `reeve attach <name>` subcommand routing

#### Done When

- Given the operator presses Enter on an agent row in the panopticon, then the
  inspect screen opens showing that agent's conversation thread
- Given `reeve attach reviewer-a3f2`, then the inspect screen opens for that
  agent (or prints `agent not found` if absent)
- The thread renders inbound and outbound entries with correct speaker labels
  per the updated `EntryKind::speaker_label` convention
- `h` from inspect returns to the panopticon with the same agent row focused
- Tab cycling through stub tabs does not panic or corrupt render state

#### Depends On

- panopticon-screen

## Phase 8: Quarantine review screen

| Status    | Started | Completed |
| --------- | ------- | --------- |
| not started |         |           |

Tags: tui

The quarantine review screen lets the operator triage rejected envelopes. Per
`reeve-tui-screens.md § Quarantine`: a list of quarantined files across all
agents with envelope metadata and raw body; `d` to discard; `o` to convert to an
operator-tier message.

**Data source.** The screen reads all files in all agents' `quarantine/`
directories. Each filename encodes the quarantine reason (the `.reason_token`
suffix convention from the watcher). The screen parses the envelope to display
metadata; if parsing fails the file is shown with its filename and a
`parse_failure` label.

**Discard (`d`).** Deletes the quarantine file. The replay ledger entry is
preserved so the same message\_id cannot be replayed. A confirmation prompt
appears before delete.

**Convert (`o`).** Opens a compose surface pre-filled with the quarantined body.
The operator edits and submits; the result is a new operator-signed message
delivered to the same recipient. The original quarantine file is not moved or
deleted — it remains as the audit record. The new message's envelope has a fresh
`message_id` and nonce.

#### Delivers

- Quarantine review screen with file list, envelope metadata pane, body pane
- `d` to discard with confirmation
- `o` to convert: compose surface, operator-signed delivery, original preserved
- `Q` from panopticon opens the review screen; `Tab` returns

#### Done When

- Given files in a `quarantine/` directory, when the operator opens the
  quarantine screen, then all quarantined files are listed with reason and
  arrived timestamp
- `d` on a selected entry prompts for confirmation; on confirm the file is
  deleted from `quarantine/`; the quarantine count in the panopticon queue row
  decrements
- `o` on a selected entry opens a compose surface; submitting delivers a new
  operator-signed envelope to the original recipient; the quarantine file remains
- The replay ledger entry for a discarded message prevents re-delivery if the
  same `message_id` is submitted again (verified by integration test)
- `Tab` from the quarantine screen returns to the panopticon

#### Depends On

- per-agent-inspect

## Phase 9: Documentation

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ⬜ not-started  |            |            |

Tags: docs

The implementation is complete. This phase wires up the project's durable
documentation layer so the knowledge that currently lives in specs survives
their retirement.

**mdBook.** Add `mdbook` to the Nix dev shell and create `docs/book.toml`.
The book has two sections: Architecture (narrative overview, key design
decisions) and Operations (install, configure, run, send a message). A
`just docs` target builds and opens it. The GitHub Actions CI job adds a
`mdbook build` step to verify the book compiles on every push.

**Architecture Decision Records.** `docs/decisions/` holds numbered ADR
files (`001-*.md`, `002-*.md`, …). Each ADR is a short document with three
sections: Context (why the decision arose), Decision (what was chosen and
what was rejected), Consequences (what follows from the choice). ADRs are
never edited after the fact; superseded ADRs are marked with a `Superseded
by:` line at the top. The first two ADRs capture decisions already made:
the tool-actor trust boundary (tools as local executors, Agent as the audit
point, envelope protocol reserved for agent-to-agent crossings) and the
actor-system internals model (actix supervision tree, Maildir spool,
filesystem-only runtime/TUI channel).

**Crate-level rustdoc.** Each crate root (`lib.rs` or `main.rs`) gains a
`//!` module doc: one paragraph on what the crate does, one on its public
surface, and a link to the book for narrative context. `cargo doc --no-deps`
must produce zero warnings.

**`architecture.md` cleanup.** The root `architecture.md` currently
references spec files in its key-files table; those pointers are replaced
with pointers to `cargo doc` output and `docs/decisions/`. The line "Signed
envelopes are the only communication channel. There is no socket or RPC" is
corrected to reflect the tool-actor model (tools communicate via actix
messages internally; the envelope protocol governs cross-agent boundaries).

#### Delivers

- `docs/book.toml` and at least two book chapters (Architecture, Operations)
- `docs/decisions/002-tool-actor-trust-boundary.md`
- `docs/decisions/003-actor-system-internals.md`
- `just docs` target building and opening the book
- CI step verifying `mdbook build`
- `//!` crate-level docs on all reeve-* crate roots (zero `cargo doc`
  warnings)
- `architecture.md` updated and accurate

#### Done When

- `just docs` builds without error and opens the book
- `cargo doc --no-deps` produces zero warnings across all crates
- Both ADRs exist and are prose-complete (context, decision, consequences)
- `architecture.md` contains no dead links to retired spec files

#### Depends On

- quarantine-review-screen

## Notes

### Non-goals for this ladder

Deferred to later ladders:

- Capability profile and blacklist enforcement — `InvokeTool.sender_id` is
  present; the authority check always returns Allow. Enforcement arrives in
  `reeve-authority` (ladder 3).
- System-prompt source annotation and length cap — `spawn_agent`'s
  `system_prompt` is caller-supplied (the spawning agent) but currently
  treated as trusted by the spawned agent. Source annotation (mark as
  untrusted at the boundary) and a transport-level length cap arrive in
  `reeve-transport-security.md` (ladder 3+). [t5 yelena.SP-1 → 2026-05-09]
- Classifier integration — no content classification on tool inputs or outputs.
  Arrives in `reeve-gatekeeper` (ladder 4).
- Long-term agent lifecycle management — agents accumulate on disk indefinitely
  in this ladder. Pruning, archiving, and explicit decommissioning of stopped
  agents arrive when the agent lifecycle model matures.
- Bus tape — the causal record from `reeve-actor-interior.md` is not
  implemented. The journal is the durable record.
- Full curator architecture — working context, compression, eviction, satellites,
  small-model fallback. The tool loop is the one addition to the degenerate
  curator from ladder 1.
- Panopticon pending decisions panel — requires classifier output (ladder 4).
- Memory review and configuration revision screens — ladders 5 and 6.

### Architectural commitments

Fixed for this ladder:

- Tool authority check lives in the tool actor's message handler, not in a
  separate mediating actor. Topology: `Agent → ToolActor`; no intermediary.
- `InvokeTool.sender_id` is the authority check token. Its type and position in
  the message are stable across ladders.
- The panopticon is the home screen. `reeve` and `reeve attach` (no arg) land
  there unless session memory points to a running agent.
- Session memory lives at `~/.local/state/reeve/session.toml`. It is written
  on TUI exit and read on startup. It carries no security-sensitive data.
- `AgentRegistry` on disk is the sole TUI ↔ runtime channel for agent
  enumeration. No socket, no RPC — consistent with the filesystem-only boundary.
- Agent private keys are written to `agents/<name>/identity.key` (mode 0600).
  Not in the OS keychain — agents are not operator-tier principals — but durable
  across restarts. The key file is the source of truth for agent identity.
- Tool loop bound: `MAX_TOOL_ITERATIONS = 16`. A loop that hits this bound is a
  runaway; the agent appends a system entry and goes idle.
