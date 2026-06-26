## Phase 1: Capability profile schema, snapshot, tool-actor enforcement

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-06-04 | 2026-06-04 |

Tags: runtime, security, persona

Foundation phase. Defines the capability profile schema, ships the on-disk
representation, wires the snapshot at spawn, and integrates the first
authority check site (the tool actor's `Handler<InvokeTool>`). Without
this phase nothing else in the ladder has a check to plug into.

**Profile schema.** New `CapabilityProfile` type in `reeve-runtime`
(module name at the implementer's discretion, but it should sit next to
`PersonaConfig`). Fields per `reeve-domain-model.md` § Capability Profile:
`name`, `version` (u32), `enabled_categories` (closed enum: `read_files`,
`write_files`, `execute_shell`, `git_read`, `git_write`, `spawn_agents`,
`message_peers`, `network_egress`, `write_memory`, `write_configuration`),
`thresholds` (closed enum with the same names from the domain model;
this phase parses all four but only `spawn_agents` /
`message_peers`-category logic is exercised — the actual threshold
enforcement lands in phases 3 and 4). TOML serde with
`deny_unknown_fields`.

**Persona-side file.** Profiles live at
`<data_dir>/identities/personas/<persona>/profile.toml`, alongside the
existing `config.toml`. The existing `capability_profile: Option<String>`
field in `PersonaConfig` is **removed** as part of this phase — carrying
a stale name reference would be a configuration footgun once profiles
have their own file.

**Persona migration.** Write `profile.toml` for the two shipped
personas (`lead`, `worker`). Lead's profile enables every category
(it is the operator's principal proxy and must be able to do
anything). Worker's profile enables `read_files`, `git_read`,
`message_peers`, `spawn_agents` — a deliberate restriction so the
demo for this phase shows refusals on the worker without ambiguity.
Thresholds default to operator-friendly values; document the chosen
defaults inline in the persona's profile.toml comments.

**Snapshot at spawn.** The `SpawnCoordinator` is extended to read the
persona's `profile.toml`, validate it (schema version, closed-enum
membership), and write a verbatim snapshot to
`<data_dir>/agents/<name>/profile.toml`. Snapshot is immutable for the
agent's lifetime. **Missing persona profile → `SpawnError::ProfileMissing`,
spawn refused.** No permissive fallback.

**Tool-actor check.** The tool actor's `Handler<InvokeTool>` runs the
authority check first, before any work. The check reads the snapshotted
profile at `agents/<name>/profile.toml` once per agent (cached for the
agent's lifetime; the snapshot is immutable), looks up the category
the tool declares, and refuses with `layer=profile,
category=<category>` if not enabled. Each tool actor declares its
category via the tool registration interface (`spawn_agent` →
`spawn_agents`, `send_message` → `message_peers`).

**Refusal type.** New `Refusal` type in `reeve-runtime` (module name at
implementer's discretion). Carries `layer` ("profile" | "blacklist" |
"threshold"), the layer-specific field (category | pattern |
threshold name + current + limit), and a `rationale` string. The tool
actor builds the `Refusal`, JSON-serializes it into a `ToolResult {
is_error: true, content: <json> }`, and sends it back. The model sees
the refusal as a normal tool error.

**Audit log.** Every authority decision (Allow and Refuse) emits an
`authority.decision` audit entry per `reeve-domain-model.md` §
Runtime § Audit Log. Fields: `kind: "authority.decision"`,
`timestamp`, `agent_id`, `persona_name`, `profile_version`, `action`
(stub — full action-descriptor convention lands in phase 2; for now
serialize as `<ToolName>(<input json>)` or similar terse form),
`disposition` ("allow" | "refuse"), `layer`, `rationale` (refuse only).
`blacklist_version` field present but always `null` until phase 2.

**Tests.** Unit tests on the profile parser (closed-enum validation,
schema-version rejection, missing-field rejection). Integration test
that spawns an agent with a restrictive worker profile and verifies a
`spawn_agent` call returns `is_error: true` with a parseable `Refusal`
body. Integration test that the lead (with full profile) succeeds.
Integration test that a persona without `profile.toml` causes
`SpawnError::ProfileMissing`.

#### Delivers

- `CapabilityProfile` type + TOML parser in `reeve-runtime`
- `Refusal` type + serialization in `reeve-runtime`
- `profile.toml` written for the shipped `lead` and `worker` personas
- `capability_profile: Option<String>` removed from `PersonaConfig`
- `SpawnCoordinator` snapshots `profile.toml` to
- Tool actor's `Handler<InvokeTool>` runs the profile check first;
- `authority.decision` audit entries for Allow and Refuse from the

#### Done When

- Given a worker with `spawn_agents` disabled, when its model issues
- Given the lead with full profile, when it issues either `spawn_agent`
- Given a persona without `profile.toml`, when the operator attempts
- `cargo test -p reeve-runtime` passes including the new profile parser
- `just validate` passes

#### Depends On

- (none)

## Phase 2: Blacklist + tool-actor blacklist enforcement

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-06-04 | 2026-06-10 |

Tags: runtime, security

Adds the operator's deterministic floor: a global blacklist file that
unconditionally refuses matching actions, regardless of profile.

**File and registry.** Global `<data_dir>/blacklist.toml`. New
`BlacklistRegistry` in `reeve-runtime`. The registry holds the parsed
entries, a SHA-256 content hash of the canonical-serialized contents
(the `blacklist_version` value emitted into every authority decision),
and a watcher subscription that triggers reload on file change.

**File shape.** `schema_version = 1` (u32); `[[entry]]` table arrays
each with `pattern` (string) and `rationale` (string, non-empty). TOML
serde with `deny_unknown_fields`. Empty file is valid (no entries).

**Reload semantics.** Successful reload swaps the in-memory state
atomically and writes a SHA-256 of the canonical contents as the new
`blacklist_version`. Failed reload (TOML parse error, schema version
mismatch, pattern syntax error, missing rationale) leaves the last-good
state in place, emits an audit event of kind
`blacklist.reload_failed` with the error message, and surfaces a
banner in the panopticon footer until the next successful reload. The
runtime starts with an empty blacklist if the file does not exist at
startup (this is not a failure mode — operator may not have opinions
yet).

**Action descriptor convention.** Each tool actor exposes a method
`canonical_action(input: &Value) -> String` that produces the
`Tool(specifier)` form used for blacklist matching. For the two tools
shipped today: `SpawnAgent(persona=<persona_name>)` and
`SendMessage(to=<recipient_role_name>)`. Match semantics for these
tools: exact equality on the parenthesized key-value list (no
globbing). The pattern syntax is extensible — future tool kinds
declare their semantics when they register; phase 2 ships the
exact-equality matcher only.

**Tool-actor wiring.** The tool actor's check runs blacklist after
profile per `reeve-authority.md` § Order of evaluation. On a blacklist
match the actor builds the `Refusal` with `layer="blacklist"`,
`pattern=<matched pattern>`, `rationale=<entry rationale>`. Audit entry
records `blacklist_version` with the SHA-256.

**Watcher integration.** The existing TUI watcher already runs
recursively on `<data_dir>/agents/`. Extend it (or add a sibling
watcher) to also observe `<data_dir>/blacklist.toml`. The runtime
daemon needs its own watcher subscription — the TUI's watcher only
informs the TUI process. Wire whichever path the daemon uses for its
existing file-system observation.

**Audit entry shape.** All `authority.decision` entries from phase 2
onward carry the `blacklist_version` field populated (or `null` when
the blacklist is empty at startup, but never absent).

**Tests.** Parser tests (schema version, missing rationale, malformed
patterns). Registry tests: reload-on-change, fail-closed on malformed
file (last-good remains), version-hash stability across identical
content, version-hash change on entry edit. Integration test: edit
blacklist to add `SendMessage(to=worker)`, see refusal next time lead
tries to send to worker.

#### Delivers

- `BlacklistRegistry` in `reeve-runtime` with TOML parser
- `<data_dir>/blacklist.toml` schema + reload-on-edit + fail-closed
- `canonical_action()` method on each shipped tool actor
- Tool-actor `Handler<InvokeTool>` runs blacklist check after profile
- `blacklist_version` (SHA-256 content hash) populated in every
- `blacklist.reload_failed` audit event on parse failure

#### Done When

- Given a blacklist entry `SendMessage(to=worker)`, when the lead's
- Given the operator edits `blacklist.toml` while the daemon is
- Given a `blacklist.toml` with a TOML parse error, when the runtime
- `blacklist_version` in every `authority.decision` entry equals the
- `just validate` passes

#### Depends On

- capability-profile-schema-snapshot-tool-actor-enforcement

## Phase 3: Cost thresholds at the adapter call boundary

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-06-10 | 2026-06-17 |

Tags: runtime, adapter

Adds the first two threshold checks — `cost_per_agent` and
`cost_per_session` — at the model-adapter call boundary. This is the
first authority enforcement site outside the tool actor.

**Per-agent check.** Before each adapter call, the agent reads the
snapshotted `thresholds.cost_per_agent` (USD) from its profile snapshot
and the running cost from its existing cost meter (shipped in ladder
1). If `running >= cost_per_agent`, refuse the model call: the agent
returns to idle with a system entry in its conversation journal that
includes the `Refusal` body, and an `authority.decision` audit entry is
emitted with `layer="threshold"`, `threshold="cost_per_agent"`,
`current=<running>`, `limit=<cost_per_agent>`.

**Session-cost aggregator.** A new `SessionCostMeter` in `reeve-runtime`
walks the supervisor tree from a root agent and sums every descendant's
cost meter. The aggregator is a brainstem-tier facility per
`reeve-actor-interior.md` — it has no authority of its own and exists
only to feed the threshold check. Update cadence: after every successful
model call on any agent in the tree, the responsible agent triggers an
aggregator recompute. Cost is cumulative across the supervisor tree's
lifetime (a single "session" in the spec's sense is the tree from root
spawn until the root exits).

**Session-cost check.** Before each adapter call, the agent also reads
the snapshotted `thresholds.cost_per_session` and the current
aggregated session cost. If `aggregated >= cost_per_session`, refuse
the model call across **all** agents in the tree (each agent that
attempts a model call after the trip is refused). Audit entry per
above with `threshold="cost_per_session"`.

**Refusal shape.** Reuse the `Refusal` type from phase 1. The
`ToolResult { is_error: true }` shape doesn't apply here — model-call
refusals are not tool refusals. Instead the agent appends a system
entry to its conversation journal with the `Refusal` body serialized
and returns to idle. The TUI Decisions tab (phase 5) renders these
the same as tool refusals.

**Where the check actually fires.** The check sits in the agent's
adapter-call wrapper, not inside `reeve-adapter`. The adapter crate
stays free of authority concerns (it doesn't know about profiles or
cost meters). The runtime crate's agent-internal `call_adapter()`
helper does the threshold check before delegating to the adapter
crate.

**Tests.** Set `cost_per_agent = 0.01` on lead's profile, run the
lead with a model that returns text content (use the existing test
fixtures from ladder 1's adapter tests), confirm the second model
call after the meter crosses the threshold returns the refusal.
Multi-agent test: lead spawns worker, both have profiles with
`cost_per_session = 0.05`, after the aggregated session cost crosses
threshold the next model call on either agent refuses.

#### Delivers

- `cost_per_agent` threshold check at adapter-call boundary
- `SessionCostMeter` in `reeve-runtime` walking the supervisor tree
- `cost_per_session` threshold check at adapter-call boundary
- `authority.decision` audit entries with `layer="threshold"` for
- System entries in conversation journals when a model call is refused

#### Done When

- Given an agent with `cost_per_agent = 0.01` USD, when its cost meter
- Given a multi-agent tree with `cost_per_session = 0.05` on the root,
- `SessionCostMeter` correctly sums descendants — verified by a unit
- `just validate` passes

#### Depends On

- capability-profile-schema-snapshot-tool-actor-enforcement

## Phase 4: Concurrency threshold + max_task_duration + Exiting state

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-06-17 | 2026-06-20 |

Tags: runtime, lifecycle

Adds the remaining two threshold checks. `max_concurrent_subordinates`
is straightforward; `max_task_duration` introduces a new `Exiting`
agent state with a non-trivial lifecycle.

**Concurrency check.** `SpawnCoordinator::Handler<SpawnAgent>` is
extended to count the calling agent's live subordinates (from the
`AgentRegistry`) before honoring the request. If `live_subordinates >=
thresholds.max_concurrent_subordinates`, refuse with
`layer="threshold"`, `threshold="max_concurrent_subordinates"`,
`current=<count>`, `limit=<configured>`. The refusal flows back to the
calling agent as a `ToolResult { is_error: true }` with the `Refusal`
body — `spawn_agent` is a tool, so the standard tool-refusal shape
applies.

**Task clock.** Each agent gets a per-agent task clock. The clock
starts when the agent receives a new task (currently: the first
inbound user-tier message after going idle; the precise trigger is
documented in the implementation comments). The clock stops on task
completion (return to idle without pending work). The clock is held
in `AgentActor` state, not on disk — it does not survive restart, and
that is acceptable because `max_task_duration` is a session-runtime
bound, not a persistent ceiling.

**Exiting state.** New agent state `Exiting`, added alongside the
existing `Working`, `Idle`, `Stopped`, etc. in
`reeve-runtime`'s agent state machine. Transitions:

- **Entry**: any trip of `max_task_duration` while in `Working` or
  `Idle`. The check runs on a ticker (1-second granularity is fine;
  the implementer chooses) and on every state-affecting event.
- **In Exiting**: the agent refuses new tool invocations and new
  adapter calls (both with `layer="threshold"`,
  `threshold="max_task_duration"`). In-flight work — a pending
  adapter call, a pending `InvokeTool` awaiting a `ToolResult` — runs
  to completion. Inbound messages from peers are not delivered to
  the agent; they accumulate in `inbox/new/` for the next live agent
  of the same role (matches the existing reattach behavior).
- **Exit**: once all in-flight work completes, the agent transitions
  to `Stopped` with `stopped_reason = "max_task_duration_exceeded"`.
  The agent registry's record updates accordingly.
- **Resumption**: the operator may re-spawn the same role. This is a
  new agent (new identity ID) sharing the role name; conversation
  journal continuation matches ladder 2's reattach behavior.

**TUI sigil.** Panopticon's agent table renders `Exiting` with a
distinct sigil. Recommendation: `…` (horizontal ellipsis,
`\u{2026}`). The implementer can choose a different glyph if `…` is
unreadable at 80×24; sigil choice is not load-bearing.

**Tests.** Unit tests on the state machine: legal transitions
`Working → Exiting → Stopped`, `Idle → Exiting → Stopped`; illegal
transitions (e.g., `Stopped → Exiting`) rejected. Integration test:
set `max_task_duration = 5s` on a persona, spawn an agent under that
persona, send it a task that takes longer, verify it enters
`Exiting`, the in-flight model call completes, the agent transitions
to `Stopped` with the named reason. Integration test for
`max_concurrent_subordinates`: lead with `max_concurrent_subordinates
= 2`, spawn three workers in rapid succession, verify the third
`spawn_agent` call returns the structured refusal.

#### Delivers

- `max_concurrent_subordinates` enforcement in `SpawnCoordinator`
- Per-agent task clock in `AgentActor`
- `max_task_duration` enforcement on a periodic check
- New `Exiting` agent state with full lifecycle (Entry / In Exiting /
- Panopticon sigil for `Exiting` state
- Audit-log entries for both threshold trips

#### Done When

- Given a lead with `max_concurrent_subordinates = 2`, when its model
- Given an agent with `max_task_duration = 5s`, when it has been in
- The panopticon's agent table renders `Exiting` with a distinct
- Unit tests cover legal/illegal state transitions for `Exiting`
- `just validate` passes

#### Depends On

- capability-profile-schema-snapshot-tool-actor-enforcement

## Phase 5: TUI surfacing — pending-decisions panel + Decisions tab

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-06-26 | 2026-06-26 |

Tags: tui

Populates the two operator-facing surfaces. The panopticon's
pending-decisions panel — empty since ladder 2 — now shows recent
refusals. The per-agent inspect Decisions tab — a stub since ladder 2 —
now lists every authority decision for the focused agent.

**Authority-decision view-model.** New `AuthorityDecision` type in
`reeve-tui` (or a sub-module) representing a single decision as the
TUI sees it: `timestamp`, `agent_name`, `action` string,
`disposition`, `layer`, `rationale`. The reader (see
`crates/reeve-tui/src/reader.rs` for the pattern) parses these from
the audit log's `authority.decision` entries. Tail-read the audit log
the same way `read_conversation_tail()` works (a bounded byte tail
keeps per-tick IO O(1) per file regardless of log size). Filter the
tail to `authority.decision` entries only.

**Panopticon panel.** The pending-decisions panel renders the most
recent N refusals (N = 5 is a reasonable default; tune in
implementation). The panel's title bar shows `▲ N` where N is the
**total** refusal count from the audit log's current tail, not just
the visible-row count. When the count is zero, the panel renders the
existing "── none ──" empty-state header per ladder 2's
`build_pending_header()`.

**Decisions tab.** The per-agent inspect's Decisions tab (currently a
"not yet available" stub from ladder 2's `Phase 7`) now renders the
full authority-decision history for the focused agent in reverse
chronological order. Columns: time (HH:MM), action, disposition,
layer, rationale (truncated if long). Allow and Refuse decisions both
render; the column styling differentiates them (Allow dim, Refuse
bright/colored). The renderer reads from `state.conversation` or a
parallel `state.authority_decisions` Vec — implementer chooses the
state shape, but it should follow the existing per-agent-inspect view
model pattern.

**Read path.** The reader parses the audit log's tail for entries
with `kind == "authority.decision"`. Tail size: 64 KiB is enough for
~512 decisions and matches the existing `CONVERSATION_TAIL_BYTES`
order of magnitude. The audit log path is the runtime's audit log
location (already established).

**Tests.** Smoke test that the panopticon renders the pending-decisions
panel correctly with N=0, N=1, N=5 refusals (modify the
80×24 NO_COLOR smoke example to seed authority decisions). Unit test
on the audit-log reader: parse a synthetic log file with a mix of
`authority.decision` and other entries, verify only authority
decisions are extracted. Unit test on the Decisions tab: render with
a 50-entry fixture and verify columns/styling.

#### Delivers

- `AuthorityDecision` view-model in `reeve-tui`
- Audit-log tail reader for `authority.decision` entries
- Panopticon pending-decisions panel populated with recent refusals
- Per-agent inspect Decisions tab rendering the full decision history

#### Done When

- Given a refused tool call has been written to the audit log, when
- Given the operator presses Enter on an agent row and switches to
- The `▲ N` title-bar indicator reflects the total refusal count in
- 80×24 NO_COLOR smoke passes with at least one refusal seeded into
- `just validate` passes

#### Depends On

- capability-profile-schema-snapshot-tool-actor-enforcement

## Phase 6: System-prompt source annotation + already-running-agent rehydration

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ⬜ not-started  |            |            |

Tags: runtime, security, lifecycle

Final phase. Two bundled cleanups: (1) the system-prompt source
annotation closes the trust boundary that ladder 2 deferred, and (2)
the daemon-restart rehydration path closes the migration gap for
agents that were running when ladder 3 first shipped.

**`spawn_agent` provenance.** When `spawn_agent` is invoked, the
`SpawnCoordinator` records the `sender_id` from the `InvokeTool`
message as the spawned agent's `system_prompt_source`. This field is
written to `agents/<name>/agent.toml` alongside the existing spawn
metadata. The field is a typed `IdentityId`. When the operator (not a
peer agent) is the source — i.e., the spawn was triggered by an
operator-tier inbound message, not by a peer's tool call — the field
is set to the operator's identity ID. Cold spawns (no `spawn_agent`
inbound, e.g., the daemon spawning the lead at startup) record the
operator's identity ID as the source.

**Length cap.** A transport-level cap on the `system_prompt` field of
a `spawn_agent` invocation. Default 8 KiB. Configurable in the team
config (the team config is the right place per
`reeve-domain-model.md` — team-level configuration is the spawn
boundary). Over-cap requests are refused at the `SpawnCoordinator`
dispatch boundary with a clear error returned as the tool's
`ToolResult { is_error: true }`. The cap applies to the
**caller-supplied** part of the system prompt — the persona's base
prompt is exempt from the cap because it's operator-authored.

**Trust posture.** The spawned agent's curator (or, in the
walking-skeleton's degenerate curator, whatever assembles the
system-prompt context) reads `system_prompt_source` from
`agent.toml`. When the source is a peer agent (not the operator), the
caller-supplied system_prompt portion is tagged in the agent's
working context as untrusted-typed input rather than trusted
instruction. The implementation surface in ladder 3 is small: a
boolean flag in the agent's context-assembly code that affects how
the prompt segment is annotated in the model call. Full
untrusted-input handling (classifier-driven dispositions, etc.)
arrives in ladder 4.

**Already-running-agent rehydration.** When the daemon restarts onto
the ladder-3 binary, the `SpawnCoordinator`'s agent-rehydration path
(the code that reattaches each registered agent from
`agents/<name>/`) synthesizes the per-agent profile snapshot from the
persona's current `profile.toml` if `agents/<name>/profile.toml` does
not exist. The synthesized snapshot is written to disk verbatim, just
as if the agent had been freshly spawned. If the persona's
`profile.toml` is **also** missing, the rehydration fails: the agent
is left in the registry as `Stopped` with `stopped_reason =
"profile_missing"`. The operator writes the persona profile, then
restarts the daemon or the specific agent to bring it back.

**Why synthesize from current persona at upgrade time.** There is no
persona state at original-spawn time to recover (the data didn't
exist before ladder 3). Using the current persona's profile respects
the operator's intent at the moment of the upgrade. This is the one
documented exception to the "snapshot at original spawn time" rule.

**Tests.** Unit test for the 8 KiB cap (encode/decode round-trip,
over-cap refusal). Integration test for provenance: peer-spawned
agent has `system_prompt_source` set to the spawning peer's identity;
operator-spawned agent has it set to the operator's identity.
Integration test for rehydration: set up a tempdir with an `agent.toml`
but no `agents/<name>/profile.toml` and a valid persona `profile.toml`,
start the daemon, verify the snapshot is synthesized and the agent
rehydrates. Integration test for the persona-missing case: tempdir
with no persona profile, verify `stopped_reason = "profile_missing"`.

#### Delivers

- `system_prompt_source: IdentityId` field in `agents/<name>/agent.toml`
- 8 KiB length cap on caller-supplied `system_prompt` at the
- Untrusted-typed input tagging in the spawned agent's context
- Daemon-restart rehydration path synthesizes missing
- `stopped_reason = "profile_missing"` for agents whose persona has

#### Done When

- Given a peer agent invokes `spawn_agent`, when the new agent's
- Given the operator's lead inbound triggers a cold spawn of the
- Given a `spawn_agent` invocation with `system_prompt` larger than
- Given the daemon restarts and an agent's `profile.toml` is missing
- Given the daemon restarts and both an agent's `profile.toml` and
- `just validate` passes

#### Depends On

- capability-profile-schema-snapshot-tool-actor-enforcement

## Notes

### Non-goals for this ladder

Deferred to later ladders:

- **Classifier integration** — ladder 4 (`reeve-gatekeeper`). The
  composition slot in the check order is documented in
  `reeve-authority.md` but no code lands in ladder 3.
- **Profile sharing across personas** — `reeve-domain-model.md` says
  profiles "may be shared across personas"; ladder 3 ships per-persona
  profiles only. Sharing-by-reference is a future capability.
- **Interactive approve / override on pending decisions** — deferred
  to a future ladder; ladder ownership is open. Ladder 3 ships
  read-only display of the pending-decisions panel.
- **Full transport-level length-cap framework** — ladder 3 ships
  only the `system_prompt`-specific 8 KiB cap; the broader
  transport-security framework arrives in later transport-security
  work.
- **Profile versioning lifecycle** — bumping a profile version
  retroactively (and the persona/skill versioning machinery that
  owns it) arrives in ladder 6 (`reeve-skills-versioning`). Ladder 3
  treats the `version` field as a stable integer, snapshotted at
  spawn, and does not reason about bumps.
- **Audit log viewer TUI** — ladder 3 surfaces authority decisions
  via the panopticon panel and the per-agent inspect Decisions tab.
  A standalone log viewer screen is a separate concern.
- **Curator architecture** — the spec mentions "the curator reads
  `system_prompt_source` and tags accordingly." The full curator
  architecture from `reeve-actor-interior.md` is not implemented in
  ladder 3; the tagging is a minimal hook on top of the
  walking-skeleton's degenerate curator.

### Architectural commitments

Fixed for this ladder:

- **Authority is enforced at four sites, not one.** Tool actor's
  `Handler<InvokeTool>` (profile + blacklist), model-adapter call
  boundary (cost thresholds), `SpawnCoordinator` (concurrency
  threshold), per-agent task clock (duration threshold). They share a
  decision-record shape and a single audit-log surface but each fires
  at its own site. There is no central authority service.
- **Capability profiles are per-persona at this ladder.** Each
  persona owns its `profile.toml` next to `config.toml`. No
  sharing-by-reference.
- **Snapshot at spawn is immutable for the agent's lifetime.** The
  tool actor caches the snapshot in memory after first read. The only
  exception is the daemon-restart rehydration path (phase 6), which
  is documented as the one place a snapshot is taken from current
  persona state rather than original-spawn-time persona state.
- **Blacklist is global, fail-closed, content-hash-versioned.**
  `<data_dir>/blacklist.toml` is the operator's deterministic floor.
  Malformed reload keeps the last-good state in effect. The
  `blacklist_version` recorded in audit entries is a SHA-256 content
  hash of the canonical-serialized contents.
- **Refusal is a tool error, not an exception.** The model sees
  refusals as normal tool errors (`ToolResult { is_error: true }`)
  and adapts in conversation. The agent's tool loop continues; the
  refusal does not exit the agent.
- **Pattern syntax models Claude Code's permission rules.** Tool
  actors expose `canonical_action()` producing `Tool(specifier)`
  strings; the blacklist matches per-tool semantics declared at tool
  registration. This is a deliberate convergence on operator-familiar
  pattern syntax.
- **No permissive fallbacks.** Missing persona profile → spawn
  refused. Missing agent profile snapshot at rehydration without a
  persona profile → agent stopped with named reason. Malformed
  blacklist file → last-good preserved. The ladder never substitutes
  a more-permissive default for missing or broken authority state.
