# Reeve — Authority

## Context

The third ladder in Reeve's build sequence (see `reeve-roadmap.md`). The
multi-agent ladder shipped the tool execution loop, agent spawning, peer
messaging, and the panopticon — but the authority check at the tool boundary
was a stub. Every `InvokeTool` returned `Allow`. That was deliberate: the
topology (Agent → ToolActor with the check in the tool actor's handler) was
the architectural commitment, and the enforcement was the deferral.

Ladder 3 fills in the enforcement. Every tool invocation now passes through a
real check that consults the agent's snapshotted capability profile, a global
blacklist, and a small set of quantitative thresholds before the tool actor
does any work. Refusals are surfaced to the calling agent as structured tool
errors, recorded in the audit log, and exposed in the panopticon's
pending-decisions panel — the panel ladder 2 rendered empty.

This spec is the front door for ladder 3 — narrative, scope, and reading
order. It does not re-state the canonical design, which lives in
`reeve-domain-model.md` § Capability Profile, § Blacklist, and § Classifier
Policy. Where this spec resolves implementation details left open by the
domain model, it says so explicitly.

## Narrative

You have a running Reeve install with the lead agent and several subordinates.
You spawn a reviewer-persona agent. The reviewer's persona declares its
capability profile: file reads (`read_files`) enabled, file writes
(`write_files`) enabled within the working tree, git read (`git_read`)
enabled, git write (`git_write`) **disabled**, shell (`execute_shell`)
**disabled**, network egress (`network_egress`) disabled. The
SpawnCoordinator snapshots that profile into
`agents/reviewer-91d4/profile.toml` alongside the existing `agent.toml`.

You ask the lead to dispatch a code review task. The lead delegates to the
reviewer. The reviewer's model decides to run `git diff` — `git_read` is
enabled, the check returns `Allow`, the tool runs. Then the model decides
to `git commit` — `git_write` is disabled. The tool actor's handler runs
the authority check, returns `Refuse(profile_denied: git_write category
not enabled)`, and replies with a `ToolResult { is_error: true, body:
{layer: "profile", category: "git_write", rationale: "..."} }`. The
reviewer's model sees the structured refusal, understands the constraint,
and proposes the commit as a recommendation in its response instead.

You watch this happen from the panopticon. The pending-decisions panel
now shows `▲ 1 — reviewer-91d4: git_write refused`. You press Enter on the
reviewer row, switch to the Decisions tab in the inspect view, and see the
full chain: every authority decision the reviewer has made, the disposition,
the reason. The audit log on disk carries the durable record.

From another terminal you edit `<data_dir>/blacklist.toml` to add
`Bash(git push --force*)` with a rationale. The filesystem watcher picks
up the change, reloads the global blacklist, and the next time any agent's
tool actor evaluates a `git push --force` invocation it sees the new entry
and refuses unconditionally — even agents whose profile enables `git_write`.

The lead's running task has been generating model calls for some time. The
lead's profile has `cost_per_session = 5.00` USD. The session-cost
aggregator (a new per-supervisor-tree meter introduced by this ladder)
crosses the threshold. The next model call on **any** agent in the tree
returns `Refuse(threshold_tripped: cost_per_session, current=$5.04,
limit=$5.00)`. You see the alert in the panopticon. You either raise the
ceiling and restart the tree, or accept that the session is done.

That is what ladder 3 ships.

## The Authority Check

Authority is enforced at **four call sites, not one**. They share a
decision-record shape and a single audit-log surface, but each fires
independently at its own site:

| Call site                                     | What it checks                             |
| --------------------------------------------- | ------------------------------------------ |
| Tool actor's `Handler<InvokeTool>`            | Profile (category enabled), then blacklist |
| Model-adapter call boundary                   | `cost_per_agent`, `cost_per_session`       |
| `SpawnCoordinator::Handler<SpawnAgent>`       | `max_concurrent_subordinates`              |
| Per-agent task clock (driven by `AgentActor`) | `max_task_duration`                        |

There is no central "authority service." Each call site does its own
check and emits its own decision record into the shared audit log. The
implementer should expect to touch all four sites.

Where the tool-level check lives is locked in by ladder 2's
architectural commitment: the tool actor's handler runs it first, before
any work, and there is no mediating gateway actor. `InvokeTool.sender_id`
is the identity token; its position in the message is stable across
ladders.

### Order of evaluation

Within the tool actor's handler:

1. **Capability profile** — is the tool's declared category enabled in
   the sender's snapshotted profile? If not, refuse with
   `layer=profile, category=<category>`.
2. **Blacklist** — does the sender's intended action match a blacklist
   entry? If yes, refuse with `layer=blacklist, pattern=<pattern>`.
3. **Classifier** — deferred to ladder 4 (gatekeeper). The slot exists
   in the design (per `reeve-domain-model.md` § Composition with other
   layers) but ladder 3 ships no classifier code and no `Classifier`
   trait. Ladder 4 introduces both.

A blacklist match is unconditional: even an agent with a fully-permissive
profile is refused. The blacklist is the operator's deterministic floor.

The other three sites (adapter, SpawnCoordinator, task clock) do not run
this list; they perform a single threshold check at their own boundary
and emit the same decision-record shape. See § Thresholds.

### Action descriptor

For blacklist matching, every tool actor exposes a canonical **action
descriptor** at invoke time: a string of the form `Tool(specifier)` where
`Tool` is the tool's registered name and `specifier` is a tool-specific
serialization of the request. Examples:

- `Bash(git push --force origin main)` — Bash actor concatenates argv
- `Read(/Users/.../secrets/.env)` — Read actor uses the resolved path
- `Write(/Users/.../README.md)` — Write actor uses the resolved path
- `WebFetch(domain:internal.example.com)` — WebFetch actor extracts the
  host

Blacklist patterns use the same `Tool(specifier)` shape with per-tool match
semantics, modeled on Claude Code's permission rules:

- `Bash(...)` — prefix match against argv concatenation
- `Read(...)`, `Write(...)`, `Edit(...)` — path glob (`*`, `**`,
  `~` for home, `.` for working tree root)
- `WebFetch(domain:...)` — host string match with `*` wildcards
- New tool kinds declare their match semantics when they register

Modeling on a known UX prevents reinventing pattern syntax and lets
operators reuse the mental model they already have from Claude Code's
settings.json.

The two tools shipped today — `spawn_agent` and `send_message` — produce
descriptors of the form `SpawnAgent(persona=<name>)` and
`SendMessage(to=<recipient_name>)`. Their match semantics: exact
equality on the parenthesized key-value list, no globbing. Future tool
kinds shipped in later ladders (`Bash`, `Read`, `Write`, `WebFetch`,
etc.) declare their match semantics when they register.

## Capability Profiles

The canonical definition is in `reeve-domain-model.md` § Capability Profile:
fields are `name`, `version`, enabled categories (closed enum), thresholds
(closed enum). Ladder 3's implementation:

### Persona-side

Each persona owns its profile in-tree:

```
<data_dir>/identities/personas/<persona>/
├── config.toml          (existing: name, system_prompt, ...)
└── profile.toml         (new: name, version, enabled_categories, thresholds)
```

The existing `capability_profile: Option<String>` field in `config.toml`
is **removed**. The persona's profile is now in its own
`profile.toml` next to `config.toml`; carrying a stale name reference in
`config.toml` would be a configuration footgun. The removal is part of
the same migration commit that writes `profile.toml` for the two
shipped personas.

Domain model § Capability Profile says profiles "may be shared across
personas" — ladder 3 does **not** ship sharing. Each persona owns its
profile inline. Sharing-by-reference is a future capability and is
explicitly deferred.

### Agent-side (snapshot)

At spawn, the SpawnCoordinator reads the persona's `profile.toml`,
validates it (schema version, closed-enum membership), and writes a
verbatim snapshot to:

```
<data_dir>/agents/<name>/profile.toml
```

The snapshot is immutable for the agent's lifetime. The runtime never
widens it; a re-spawn of the same agent role re-snapshots from the current
persona profile. This is the "profile cannot be widened during the
agent's lifetime" invariant from the domain model, made operational.

The tool actor's authority check loads `agents/<name>/profile.toml` once
when it first sees an `InvokeTool` for that agent and caches it in
memory for the agent's lifetime. The snapshot file is immutable by
design (see invariant above), so cache invalidation is unnecessary.
Loading the file on every invocation would be wasted I/O.

### Migration

Two migration cases — new spawns after ladder 3 ships, and agents that
were already running at the time of the upgrade.

**New spawns.** Existing personas (the walking-skeleton `lead`, the
multi-agent `worker`) do not yet have a `profile.toml`. When ladder 3
ships, the SpawnCoordinator hard-errors at spawn time if the persona is
missing `profile.toml`:

> SpawnError::ProfileMissing { persona: "lead", expected_path: ".../personas/lead/profile.toml" }

The operator sees the error, writes the profile, retries. There is no
permissive fallback. The reasoning: a "default-allow" fallback reintroduces
the failure mode ladder 3 exists to eliminate. A visible spawn failure is
a better signal than a silent broad authorization.

The ladder includes the migration commits: write `profile.toml` for the
two shipped personas as part of ladder 3's first phase.

**Already-running agents.** When the daemon restarts onto ladder 3,
agents that were running before the upgrade have an `agent.toml` but no
`agents/<name>/profile.toml` — the file didn't exist as a concept. On
restart, the SpawnCoordinator's agent-rehydration path synthesizes the
snapshot from the persona's current `profile.toml`, writes
`agents/<name>/profile.toml`, and resumes the agent. If the persona's
`profile.toml` is missing, the rehydration fails with `ProfileMissing`
and the agent is left in the registry as `stopped` with
`stopped_reason = "profile_missing"` — the operator writes the persona
profile, then restarts the daemon (or the specific agent) to bring it
back.

Synthesizing from the **current** persona profile at upgrade time
respects the operator's intent at the moment of the upgrade. This is
the one place a snapshot is taken from "live" persona state rather than
"persona state at original spawn time" — there is no persona state at
original spawn time to recover.

## Blacklist

The blacklist is global to the runtime. It lives at:

```
<data_dir>/blacklist.toml
```

The TUI watcher (already recursive on `<data_dir>/agents/`) is extended to
also cover this file. Edits reload the blacklist in-place; running agents
pick up new entries on their next tool invocation. There is no
snapshotting — the blacklist is the operator's deterministic floor and
should reflect their current intent, not their intent-at-spawn.

### Versioning and reload semantics

Each successful blacklist load produces an opaque content hash
(SHA-256 of the canonical-serialized contents). This hash is the
`blacklist_version` recorded in every authority-decision audit entry.
Two distinct file states never share a hash; the hash is what makes a
historical decision reconstructable.

If the file is malformed (TOML parse error, schema-version mismatch,
pattern syntax error), the reload **fails closed**:

- The in-memory blacklist remains at its last-good state.
- The reload error surfaces to the operator: an `audit.log` event of
  kind `blacklist.reload_failed`, and a banner in the panopticon footer
  until the next successful reload clears it.
- Running agents continue to be gated by the last-good blacklist. The
  operator's intent of "block X" is preserved across a typo.

This is the fail-closed posture the spec adopts as a general principle:
when authority state cannot be loaded cleanly, retain the more
restrictive prior state rather than substituting a permissive default.

### File shape

```toml
schema_version = 1

[[entry]]
pattern = "Bash(git push --force*)"
rationale = "force-push is irreversible on shared branches"

[[entry]]
pattern = "Bash(rm -rf /*)"
rationale = "absolute-path rm -rf is almost always wrong"

[[entry]]
pattern = "Write(**/.env)"
rationale = "credentials should not be machine-edited"
```

Each entry has only `pattern` and `rationale`. The rationale is mandatory
— it's the operator's note-to-self, surfaced in the refusal record and the
audit log so future readers (and future operators) understand the
deterministic ban.

### Matching

The tool actor builds its action descriptor (`Tool(specifier)`) at invoke
time and asks the blacklist registry whether any entry matches. Matching
is per-tool semantics — `Bash` patterns are prefix-matched against argv
concatenation, path patterns use glob semantics, etc. The first match
returns the entry's pattern and rationale to the authority decision
record; later entries are not considered.

## Thresholds

All four thresholds from the domain model are enforced in ladder 3.

| Threshold                     | Enforcement point     | Trip behavior                                                                                            |
| ----------------------------- | --------------------- | -------------------------------------------------------------------------------------------------------- |
| `cost_per_agent`              | Adapter call boundary | Refuse model call; surface in panopticon                                                                 |
| `cost_per_session`            | Adapter call boundary | Refuse model call across the entire tree; alert operator                                                 |
| `max_concurrent_subordinates` | SpawnCoordinator      | Refuse spawn until a subordinate exits                                                                   |
| `max_task_duration`           | Per-agent wall-clock  | Transition agent to `Exiting`; no new tool invocations or model calls accepted; in-flight work completes |

`cost_per_agent` reads the existing per-agent cost meter (shipped in
ladder 1). `cost_per_session` is new — a session-cost aggregator that
walks the supervisor tree from a root agent and sums every descendant's
cost meter. The aggregator is a brainstem-tier facility (per
`reeve-actor-interior.md`); it has no authority of its own and exists only
to feed the threshold check. It runs at the same cadence as the per-agent
cost meter (i.e., after every successful model call).

`max_task_duration` requires a per-agent task clock that starts when
the agent declares a task and stops on completion. Trip transitions the
agent into the `Exiting` state — a new state introduced by this ladder.

`Exiting` lifecycle:

- **Entry**: any trip of `max_task_duration` while the agent is in
  `Working` or `Idle`.
- **In Exiting**: the agent refuses new tool invocations and new model
  calls (both with `layer=threshold, threshold=max_task_duration`).
  In-flight work — a pending model call, a pending `InvokeTool`
  awaiting a `ToolResult` — runs to completion. The agent does not
  process new inbound messages from peers; they accumulate in
  `inbox/new/` for the next live agent of the same role (after
  operator action).
- **Exit**: once all in-flight work has completed, the agent
  transitions to `Stopped` with `stopped_reason = "max_task_duration_exceeded"`.
  The agent registry shows the final state; the TUI panopticon shows
  the stopped sigil with the cause in the inspect view.
- **Resumption**: the operator may re-spawn the same role from the
  TUI or CLI. This is a new agent (new identity ID) sharing the role
  name; its conversation journal is appended to the existing
  `conversation.jsonl` per the ladder-2 reattach behavior.

The TUI panopticon shows `Exiting` with a distinct sigil
(recommendation: `…` for "winding down") — the implementer chooses the
exact glyph at render time. Sigil choice is not load-bearing for the
spec.

## Refusal UX

A refused tool invocation returns:

```rust
ToolResult {
    is_error: true,
    body: serde_json::to_string(&Refusal {
        layer: "profile" | "blacklist" | "threshold",
        // exactly one of:
        category: Option<String>,         // when layer = "profile"
        pattern: Option<String>,          // when layer = "blacklist"
        threshold: Option<ThresholdName>, // when layer = "threshold"
        current: Option<String>,          // when layer = "threshold"
        limit: Option<String>,            // when layer = "threshold"
        rationale: String,                // free-form, sourced from the blacklist entry or a deterministic template
    })?,
}
```

The model sees the refusal as a normal tool error. The structured body is
plain text it can parse; the rationale is the operator-facing English that
explains why. Models tend to adapt: a `git_write` refusal usually results
in the model proposing the change as a suggestion in its next response
rather than re-attempting the call.

The refusal does not poison the conversation or exit the agent. The agent
continues its tool loop with the error in history; the next iteration may
attempt a different approach.

## Audit Log

Every authority decision — Allow **and** Refuse — produces an audit log
entry. The entry shape:

```
{
  "kind": "authority.decision",
  "timestamp": "...",
  "agent_id": "01HYTC...",
  "persona_name": "reviewer",
  "profile_version": 1,
  "blacklist_version": 3,
  "action": "Bash(git push --force origin main)",
  "disposition": "refuse",
  "layer": "blacklist",
  "rationale": "force-push is irreversible on shared branches"
}
```

Allow decisions carry the same shape with `disposition: "allow"` and no
`rationale` field. The volume of Allow entries is real — every successful
tool call generates one — but the audit log is already designed for this
cadence and the entries are cheap (no human in the loop).

The runtime's existing audit log infrastructure (per
`reeve-domain-model.md` § Runtime § Audit Log) receives these entries.
No new on-disk facility is introduced for them.

## TUI Changes

Two TUI surfaces are wired in ladder 3:

1. **Panopticon pending-decisions panel** — the panel ladder 2 rendered
   empty now populates with recent refusals (count + last reason). The
   `▲ N` indicator in the title bar reflects the panel's count.
   Ladder 3 ships read-only display only — no interactive approve or
   override action. Interactive approval is deferred (see § Scope Cuts
   for ladder ownership).

2. **Per-agent inspect "Decisions" tab** — the tab that's been a "not
   yet available" stub since ladder 2 now lists every authority
   decision for the focused agent, in reverse chronological order.
   Columns: time, action, disposition, layer, rationale.

In addition, the panopticon's agent table renders the new `Exiting`
state with a distinct sigil (see § Thresholds).

## System-Prompt Source Annotation

The multi-agent ladder's Non-goals note that `spawn_agent`'s `system_prompt`
field is caller-supplied and treated as trusted by the spawned agent. This
is an authority concern dressed as a transport concern: a peer agent can
shape another agent's behavior by supplying its initial system prompt.
Ladder 3 closes that gap as a small bundled scope:

1. **Provenance** — the SpawnCoordinator records the `sender_id` of the
   `spawn_agent` tool call alongside the spawned agent's other metadata.
   This is written to `agents/<name>/agent.toml` as a `system_prompt_source`
   field carrying the sender's identity ID.

2. **Length cap** — a transport-level cap (default 8 KiB; configurable in
   the team config) on the size of a `system_prompt` field in a
   `spawn_agent` invocation. Over-cap requests are refused at the dispatch
   boundary with a clear error.

3. **Trust posture** — the spawned agent's curator treats the system
   prompt as untrusted-typed input rather than trusted instruction when
   the source is a peer agent. The operator's own
   `<data_dir>/identities/personas/<persona>/config.toml` system_prompt
   is the only "trusted" prompt source. The implementation surface is
   small: the curator reads the `system_prompt_source` field and tags
   the prompt accordingly.

This is the smallest cut that closes the gap. The full transport-level
length-cap design from `reeve-transport-security.md` arrives in a later
ladder; ladder 3 ships only the `system_prompt`-specific cap.

## Reading Order

For implementers, read in this order:

1. This document — what ladder 3 ships, narrative, scope cuts
2. `reeve-domain-model.md` § Capability Profile, § Blacklist — canonical
   schema and composition rules
3. `reeve-actor-interior.md` § Dispatch authority — where the check sits
   in the broader curator architecture (most of which is deferred)
4. `reeve-multi-agent.md` § The Tool Subsystem — the tool actor topology
   ladder 2 established and ladder 3 fills in

## Scope Cuts

Things deferred to later ladders or out of scope:

- **Classifier integration** — ladder 4 (`reeve-gatekeeper`). The
  composition slot exists in the design but no code lands in ladder 3.
- **Profile sharing across personas** — the domain model says profiles
  may be shared; ladder 3 ships per-persona profiles only.
- **Interactive approve/override on pending decisions** — deferred to
  a future ladder; ladder ownership not yet assigned. The current
  roadmap (ladders 4–7) does not name this capability explicitly.
  Ladder 3 ships read-only display of the panopticon panel; the
  interactive surface (approve / override / acknowledge actions on a
  focused decision row) is open scope.
- **Full transport-level length-cap framework** — ladder 3 ships only
  the `system_prompt`-specific cap; the broader framework arrives with
  later transport-security work.
- **Profile versioning lifecycle** — bumping a profile version
  retroactively (and the persona/skill versioning machinery that owns it)
  arrives in ladder 6 (`reeve-skills-versioning`). Ladder 3 treats the
  version field as a stable integer, snapshotted at spawn, and does not
  reason about bumps.
- **Audit log viewer TUI** — ladder 3 surfaces decisions via the
  panopticon panel and the inspect Decisions tab. A standalone log viewer
  screen is a separate concern.
