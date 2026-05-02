# Reeve — Domain Model and Code-Level Architecture

## Context

Reeve is a local coding tool that runs AI agents on a developer's workstation as
named, persistent, addressable, supervised actors. The architecture is described
in the Reeve overview; transport security and content security are described in
their respective documents; defaults and self-improvement are described in the
shipped teams document.

This document is the source of truth for the runtime's domain model. It
enumerates the entities the code maintains, the relationships between them, the
invariants that always hold, the boundaries between subsystems, where state
lives, and the naming conventions that have to be consistent for the system to
compose.

This document exists to prevent conceptual drift during implementation. The four
prior documents describe what Reeve is and what it does. This document describes
the shape of the runtime that implements them. When implementation surfaces a
question the architecture documents do not answer, the answer is added here
first and the implementation follows.

## Conventions

### Identifiers

All identifiers are stable, opaque, and locally unique within their scope. The
runtime never derives meaning from an identifier's text content.

- **Names** — human-readable strings used for personas, skills, teams, agents,
  identities. Names are unique within their entity type. Names are user-facing.
- **IDs** — UUIDv7 tokens used for messages, audit events, classifier outputs,
  memory entries, key records, authority decisions, tool invocations, and model
  API calls. UUIDv7 is chosen for chronological sortability, which is useful for
  audit logs and ledger pruning. IDs are not user-facing in normal operation.
- **Keys** — `key_id` is a UUIDv7 identifier for a specific keypair record; an
  identity may have many keys over its lifetime.

Content hashes are a separate concept and appear as fields in some entities
(`payload_hash` in the message envelope, `content_hash` in a classification,
content-derived hashes used for portability across Reeve installations). Content
hashes are not identifiers.

### Versions

Every configuration artifact is versioned. Versioning is monotonically
increasing and immutable: a given (artifact, version) pair always refers to the
same content. Configuration artifacts that are versioned: persona, skill, team,
memory entry, capability profile, blacklist, classifier model, disposition
policy.

Version format is `<integer>` for monotonic revisions within a single instance
of Reeve, supplemented by a content hash for portability across instances. A
persona at version 7 in one operator's environment is not the same artifact as
version 7 in another operator's environment unless the content hashes match.

Runtime entities (agents, messages, tool invocations) are not versioned. They
are events with timestamps.

### Naming Scopes

- Persona names: unique per Reeve installation.
- Skill names: unique per skill library; a skill is referenced by name and
  version.
- Team names: unique per Reeve installation.
- Agent names: unique per running runtime instance. Agents are instances and
  their names are reused as agents come and go.
- Identity IDs: globally unique across the runtime's history. An identity, once
  retired, is not reused.

## Configuration Layer

Configuration is the set of artifacts that define behavior but do not run.
Configuration is durable, versioned, and stored on the filesystem.

### Revision semantics

All configuration revisions are default-open. A revision authored by any agent
with `write_configuration` capability — typically a forge persona — produces a
new version that takes effect at the next agent spawn. Prior versions are
retained; the operator may revert any configuration entity to any prior version
through the panopticon's review surface. There is no proposed-then-approved
workflow at the configuration layer.

The trade-off matches the memory write model: lower friction in exchange for
reactive quality control. The blast radius of a bad configuration revision is
bounded by the next-spawn boundary — agents already running on the prior version
are unaffected — and by the panopticon's review surface, which surfaces recent
revisions for operator inspection. Versioning makes every revision identifiable,
attributable, and revertable; the supervised observability is what makes the
trade-off workable.

### Persona

A persona is the definition of a role.

Fields:

- name
- version
- system prompt
- model requirements: required capabilities (e.g., tool calling, vision,
  reasoning, structured output) and minimum context window
- model preferences: ranked list of acceptable models in preference order
- model exclusions: models known not to work for this persona
- optional model preferences: per-call cost ceiling, latency tolerance
- capability profile (referenced by name and version)
- skill set (list of skill names and pinned versions)
- memory scope subscriptions (which persona memory store this persona reads and
  writes)

Lifecycle:

- created at version 1
- revised → new version with incremented number; prior versions remain valid for
  already-spawned agents
- deprecated → marked for retirement; no new agents may be spawned at deprecated
  versions
- never deleted: prior versions are preserved for audit and rollback

A persona is not an agent. A persona has zero or more running agents at any
time.

### Skill

A skill is a discrete capability a persona can apply.

Fields:

- name
- version
- description
- prompt fragment (text appended to the persona's system prompt when the skill
  is active)
- tool bindings (which tools the skill enables, beyond the persona's defaults)
- metadata (tags, applicable content types, expected outcomes)

Lifecycle:

- created at version 1
- revised → new version; pinned-version references continue to use prior
  versions
- deprecated → no new pinning; existing pins continue to resolve

A skill is referenced by personas. A skill is not an executable thing on its
own; it is a configuration fragment activated when an agent applies it. Tool
bindings declared by a skill are subject to the persona's capability profile: a
tool whose category is not enabled in the active profile is not invocable,
regardless of skill bindings. Skills compose vocabulary; they do not grant
authority.

### Memory Entry

A memory entry is a single durable fact stored in a memory scope.

Fields:

- entry ID
- scope (project, persona name, operator name)
- version (monotonic per entry)
- content (markdown)
- author (operator identity, agent identity, or `forge`)
- core flag (true if loaded into the agent's system prompt at spawn for personas
  subscribed to this scope; false if reachable only via query)
- created_at, last_referenced_at
- core load count (incremented when loaded into an agent at spawn)
- query reference count (incremented when retrieved by an agent's `memory.read`
  or returned in `memory.search` results)

Scopes:

- **Project memory** — lives at `<repo>/.reeve/memory/`. Committable and
  reviewable as code.
- **Persona memory** — lives at `~/.local/share/reeve/personas/<name>/memory/`.
- **Operator memory** — lives at
  `~/.local/share/reeve/operators/<name>/memory/`.

#### Presentation

Memory is presented to agents in two layers.

**Cold-start core.** Entries marked `core` in subscribed scopes are loaded into
the agent's system prompt at spawn. The cold-start core is what makes fresh
agents smart on arrival; it is intended to be small and per-persona-relevant,
not the whole store. Core membership can be set by any agent with `write_memory`
capability — the same permissive model as memory writes generally — and the
forge team is the typical promoter, acting on observed reference patterns. The
operator monitors core changes in the panopticon's review surface and reverts
any that aren't desired.

**Queryable store.** The full set of active entries in subscribed scopes is
reachable through memory tools — `memory.search` and `memory.read` at minimum —
that the runtime exposes when the persona's capability profile permits memory
access. Agents pull from the store on demand for context that is not in their
cold-start core.

#### Reference counting

The two paths produce different signals. Cold-start core load counts are uniform
across spawns of personas subscribed to the entry's scope and carry little
curation signal. Query reference counts are per-agent per-entry and are the
empirical signal the forge team uses for curation: frequently-queried entries
are high-value; loaded-but-never-queried entries are dead weight.

#### Update propagation

Memory writes (revisions, new entries) go to the store. A running agent's view
of memory comes from two places: its cold-start core, loaded at spawn and frozen
for the agent's lifetime, and its queries, resolved against the current store at
the moment of query. An agent that queries an entry after a revision sees the
revised content. An agent whose cold-start core contains an entry that is later
revised continues to see the spawn-time version until it queries the store, or
until it exits and a new agent of its persona spawns. There is no in-flight push
of core updates into running agents.

This means operators who require immediate propagation of a memory change rely
on the short agent lifecycle: the next spawn picks up the new core. For
long-lived leads, this is a delay; for short-lived subordinates, it is
effectively immediate.

#### Write semantics

Memory writes by any agent with `write_memory` capability take effect
immediately. Each write produces a new version of the affected entry; prior
versions are retained for rollback. The panopticon surfaces recent writes under
a review view; the operator may revert any write to any prior version. There is
no proposed-then-approved workflow.

The trade-off is permissive by default. Writes accumulate without ceremony,
queries return the latest version, and the cold-start core picks up new content
on next spawn. Bad writes can pollute future agent spawns until the operator
reverts them. The supervised observability of the panopticon plus versioned
rollback are what make this trade-off workable; the tool optimizes for low
friction in memory accumulation with reactive quality control rather than gated
approval.

Operators who want a specific scope to be operator-only configure it by denying
`write_memory` to all personas that subscribe to it; agents can still read, but
only the operator can write.

Lifecycle:

- active → reachable via the queryable store; loaded into agent cold-start core
  at spawn if marked `core`
- revised → new version replaces prior; prior versions retained for rollback
- retired → no longer loaded or returned in queries; retained for audit

### Team

A team is a configured set of persona instantiations.

Fields:

- name
- version
- members: list of (persona name, persona version, count, role label)
- lead: the role label of the team member designated as the operator's first
  point of contact; `reeve attach` with no agent argument resolves to the
  running agent of this role
- shared configuration: cost ceiling, default capability profile overrides,
  default classifier policy
- bundled assets (when published): pinned skill set, seed memory bundle

Lifecycle:

- created → version 1
- revised → new version
- published → bundle is exported as a portable artifact for sharing
- imported → loaded from a published bundle; pinned versions are preserved

Starting a team spawns one running agent per (persona, count) combination. Teams
are supervisable as a unit: stopping a team terminates all its agents.

### Capability Profile

A capability profile is a _coarse_ policy artifact. It carries category-level
on/off filters and quantitative thresholds, and only those. Action-level
granularity — which paths, which commands, which hosts, which peers — does not
live in the profile. That granularity is split across the blacklist
(deterministic bans on irreversible or egregious specifics), the runtime
classifier (contextual judgment on the rest), the runtime's operator-level
configuration (workstation-environment concerns such as the host allowlist; see
Runtime § Owned state), and team configuration (the spawn boundary).

This separation is deliberate. A profile that carried per-resource allowlists
would reintroduce the configuration burden and the bypass-to-unblock failure
mode that the layered design exists to eliminate. Profiles answer "what kinds of
action may agents of this persona attempt at all?" The blacklist, classifier,
runtime config, and team config answer "and which specific instances of those
actions, in this context?"

Fields:

- name
- version
- enabled categories (closed enumeration; see below)
- thresholds (closed enumeration; see below)

A capability profile is referenced by name and version from a persona. The same
profile may be shared across personas. An agent's profile is the snapshot taken
at spawn from its persona's pinned reference; the profile cannot be widened
during the agent's lifetime.

#### Categories

Each category is a binary on/off. The closed enumeration:

- `read_files` — read paths under the repository working tree and any
  operator-configured shared paths
- `write_files` — modify paths under the repository working tree
- `execute_shell` — invoke a shell with arguments
- `git_read` — read git state (status, log, diff, blame)
- `git_write` — mutate git state (branch, merge, commit, stash, push)
- `spawn_agents` — instantiate a subordinate agent within the team's roster
- `message_peers` — send a signed message to another agent in the runtime
- `network_egress` — initiate outbound network calls (subject to the runtime's
  host allowlist)
- `write_memory` — write entries to subscribed memory scopes. Writes take effect
  immediately and are revertable through the panopticon (see Memory Entry §
  Write semantics)
- `write_configuration` — write revisions to personas, skills, and team
  definitions. Revisions take effect at the next agent spawn and are revertable
  through the panopticon (see Configuration Layer § Revision semantics)

Memory reads are not a category. They are governed by the persona's memory
subscriptions: a persona without a subscription to a scope cannot read it; a
persona with a subscription reads automatically.

#### Thresholds

Quantitative limits enforced by the runtime. Each has a defined trip behavior:

- `cost_per_agent` — total estimated model cost for a single agent. On trip:
  model calls refused; the event surfaces in the panopticon. The agent is not
  exited; non-model work may continue.
- `cost_per_session` — total estimated cost for the entire agent tree across the
  session. On trip: model calls refused across all agents in the tree; the
  operator is alerted.
- `max_concurrent_subordinates` — number of live subordinates the agent has at
  once. On trip: spawn requests refused until a subordinate exits.
- `max_task_duration` — wall-clock time from task scope declaration to
  completion. On trip: the agent transitions to exiting; no new tool invocations
  or model calls accepted; in-flight work completes.

Thresholds are closed in this version of the spec. Adding a new threshold
requires a profile schema version bump.

#### Composition with other layers

A tool invocation is permitted only if all of the following hold:

1. Its category is enabled in the agent's snapshotted capability profile.
2. It is not a blacklist match against action and context.
3. The runtime classifier returns pass or flag for this specific invocation.

A skill that binds a tool does not grant authority. The tool is invocable only
if the persona's capability profile enables the relevant category. A profile
enabling a category does not commit to which specific tools are exposed; that is
the responsibility of skills and persona defaults.

### Blacklist

A blacklist is a deterministic set of refused actions.

Fields:

- name
- version
- entries: list of patterns matching action+context; each entry has a
  description and a rationale

Patterns are matched deterministically against attempted actions. A blacklist
match is an unconditional refusal. The blacklist is loaded by the runtime at
startup and on configuration change.

### Classifier Policy

A classifier policy maps classifier output to runtime disposition.

Fields:

- name
- version
- inputs the policy consumes: classifier risk level, classifier category labels,
  source trust tier, content type, agent capability profile
- mapping rules from input combinations to disposition (pass, flag, block)
- failure handling: how to dispose of malformed or missing classifier output

The disposition policy version is recorded in every authority decision so that
historical decisions are reconstructable.

### Model

A model is a logical AI model with a known set of capabilities. The model is
what a persona thinks it wants; how that model is reached is a separate concern.

Fields:

- name (e.g., `claude-opus-4-7`, `gpt-5`, `deepseek-r2`)
- producer (the organization that trained it)
- declared capabilities: tool calling, vision, reasoning, structured output,
  parallel tool calls, prompt caching
- context window size
- knowledge cutoff
- deprecated flag

A model is not callable on its own. Reaching it requires a route and an adapter.

### Route

A route is a way to reach models — a provider connection point with its own wire
protocol, authentication, rate limits, and quirks.

Fields:

- name (e.g., `anthropic-direct`, `openrouter`, `bedrock`, `vertex`)
- endpoint configuration
- authentication method
- credential reference (OS keychain entry, configured env var, etc.)
- declared rate limit policy
- provider quirks documented inline

A route can host many models. The same model can be reached through multiple
routes.

### Adapter

An adapter is the (route, model) translation. It is the actual code that takes
Reeve's internal protocol and produces requests in the wire format the route
expects for that specific model, and translates responses back. Per-pair quirks
live here.

Fields:

- adapter ID
- route name
- model name
- adapter version
- declared capabilities: the subset of the model's capabilities this adapter
  actually delivers through this route. Routes can expose less than the model
  supports.
- internal-to-external translation rules
- external-to-internal translation rules
- retry and rate limit policy specific to this (route, model) pair
- known quirks documented inline

Lifecycle:

- registered → adapter is loaded and available for resolution
- deprecated → no new resolutions; existing agent assignments continue
- retired → no resolution

The same logical model can have multiple adapters: `claude-opus-4-7` on
`anthropic-direct` is one adapter; `claude-opus-4-7` on `openrouter` is another.
They behave differently in subtle ways and are treated as distinct adapters.

OpenRouter and similar routing services are themselves routes, not transparent
layers. Treating them as the universal substrate is a mistake several existing
tools have made.

## Runtime Layer

Runtime entities are the things the runtime actually creates, manages, and
supervises during operation. Runtime entities are events or processes; they are
not versioned in the configuration sense, but they record the configuration
versions they depend on.

### Runtime

The runtime is the long-lived background process. A _session_ is the lifetime of
a single runtime invocation — from start to exit — and may span hours, days, or
weeks, with many TUI attaches and detaches in between. There is exactly one
runtime, and therefore exactly one session, per machine per operator at any
time. Per-session aggregates and ceilings (cost meters, capability-profile
thresholds, replay windows) scope to this lifetime; durable state (audit log,
conversation threads, memory entries) is preserved across sessions.

Owned state:

- supervisor tree
- agent registry: name → running agent actor handle and metadata
- identity registry: identity ID → key records
- replay ledger
- delivery ledger
- classifier connection
- adapter registry: registered (route, model) adapters and their declared
  capabilities
- route registry: configured routes and credential availability
- model provider client pool (one client per active route)
- audit log writer
- filesystem watchers (inotify/kqueue) on agent inboxes
- in-memory cache of loaded configuration artifacts
- cost meters: aggregated per agent, per persona, per team, per session
- host allowlist for network egress (workstation-environment configuration;
  gates outbound calls from any agent whose capability profile enables
  `network_egress`)

Lifecycle:

- start → initialize state, scan agents/, resume any pending message processing
- running → supervises agents, accepts new spawns, processes inbound messages,
  serves the TUI
- shutdown → graceful stop of agents, flush of audit log, release of identity
  locks

### Agent

An agent is a running instance of a persona. Agents are actix actors hosted in
the runtime's process: each owns its own mailbox and per-agent state, supervised
under a tree. Restart-on-failure is provided by the actor framework; a panicking
actor is restarted by its supervisor without quiescing the runtime, and peer
actors are unaffected by panics in their own isolated state.

Identity:

- agent name (assigned at spawn, unique within the running runtime)
- agent address (actix `Addr`, valid for the actor's lifetime)
- session key: in-memory keypair minted at spawn, bound to the actor's lifetime

Configuration snapshot (recorded at spawn, immutable for the lifetime of the
agent unless failover overrides it):

- persona name and version
- skill names and versions
- capability profile name and version
- classifier policy name and version
- memory generation: a snapshot identifier of which memory entries were loaded
  into the cold-start core
- resolved model, route, and adapter ID (selected at spawn by model resolution;
  updated on failover)

Per-agent runtime state:

- conversation thread (durable, append-only)
- current task scope (declared, possibly updated during the session)
- cost meter
- recent activity buffer
- inbox state (file watcher, replay/delivery ledger entries)
- status (idle, working, awaiting input, error, exiting)

Lifecycle:

- spawning → runtime allocates resources, mints session key, snapshots
  configuration
- ready → registered with the agent registry, inbox active
- working → actively processing
- idle → no current task
- exiting → cleanup in progress
- exited → actor stopped, durable state retained for audit

State transitions: `spawning → ready` on successful spawn, or
`spawning → exited` on spawn failure (partial state is rolled back; the agent
registry never holds a half-spawned agent). `ready ↔ working ↔ idle` as work
arrives and completes. Any non-exited state may transition to `exiting` when the
runtime decides the agent should stop — task completion, supervisor signal, or
session lease loss. `exiting → exited` after cleanup completes.

An agent always corresponds to exactly one persona at one version. Two agents of
the same persona at the same version are independent actors with separate state.

### Subordinate Spawning

An agent may spawn a subordinate if its capability profile enables
`spawn_agents` and the requested subordinate persona is in the parent's team
roster. Spawning is the act of instantiating a persona; it does not narrow,
widen, or otherwise reshape the resulting agent's authority.

Contract:

- The subordinate's authority is its persona's referenced capability profile,
  unmodified by the parent.
- The parent declares the subordinate's initial task scope at spawn. Task scope
  is the only spawn-time parameter that flows from parent to child.
- The subordinate's memory subscriptions are its persona's, not inherited from
  the parent.
- The subordinate has its own per-agent cost meter against its own ceiling.
  Session-level cost aggregates across the entire agent tree and is capped at
  the runtime's session ceiling; the more restrictive ceiling wins.
- The team roster is the spawn boundary. Cross-team spawning is not supported
  through this path; an operator may start a separate team explicitly.

The privilege-escalation question — can a low-authority parent mint a
high-authority subordinate? — is bounded by the team configuration, not by a
runtime-computed lattice. Operators who want stricter isolation construct teams
whose rosters are narrower; operators who trust their team configuration accept
that any persona in the roster may be spawned by any other member with
`spawn_agents`.

### Conversation Thread

A conversation thread is the durable, ordered record of messages an agent has
sent and received.

Fields:

- agent name
- entries: ordered list of conversation entries

Entry types:

- inbound message (from a verified sender, with trust tier and disposition
  recorded)
- outbound message (to another agent or operator)
- model call (with prompt, response, token counts)
- tool invocation (with arguments and output)
- authority decision (with disposition)
- system event (compaction, memory load, lifecycle change)

Conversation threads are durable. They are stored in the agent's `log/`
directory and are append-only during the agent's lifetime.

Compaction replaces the agent's working context with a summary of prior thread
entries when a context-size threshold is reached. The primary trigger is
size-based (token budget on the working context); a secondary duration-based
trigger is available but rarely fires, since most agents are short-lived.
Compaction does not rewrite the durable thread on disk — the full thread remains
append-only and audit-true. The compaction event itself is recorded in the
thread as a system event with a reference to the generated summary.

### Task Scope

A task scope is the declared bounded purpose for which an agent is currently
working. It is the input to gatekeeper jurisdiction.

Fields:

- declared by (operator, peer agent, or self at spawn)
- declared at
- description (text)
- scope hash (used in audit logs and cache keys)

An agent has at most one current task scope. Scope changes are recorded in the
conversation thread.

### Message Envelope

A message envelope is the unit of communication between participants.

Fields:

- schema_version
- message_id (globally unique)
- sender_id (identity ID of signer)
- sender_key_id (which key signed this)
- recipient_id (identity ID of the destination agent)
- created_at
- nonce
- payload_hash
- body
- signature

Envelopes are signed by senders and verified by the runtime on pickup. The
envelope schema is versioned and forward-compatible per the rules in the
transport security document.

### Authority Decision

An authority decision is a record of how the runtime disposed of an action an
agent attempted.

Fields:

- decision ID
- agent name
- attempted action: type, parameters
- inputs:
  - capability category (and whether enabled in the persona's profile)
  - blacklist match (yes/no, which entry)
  - classification ID (when content was involved; the classification entity
    carries the full classifier output)
  - source trust tier (from the message that triggered the action, if any)
  - content type
- disposition: pass, flag, or block
- disposition policy version
- reason
- timestamp

Authority decisions are durable and recorded in the audit log before the action
executes. Where content is involved, the decision references the relevant
classification by ID rather than inlining classifier output, so that audit log
size does not grow with content size.

### Tool Invocation

A tool invocation is an agent's attempt to call a tool.

Fields:

- invocation ID
- agent name
- tool name and version
- arguments
- authority decision ID (every invocation has a corresponding decision)
- start time
- completion: success / failure / interrupted
- output reference (path under the per-agent output store; tool outputs are not
  inlined into the audit log)

Tool invocations are durable.

Tool invocations may produce observable side effects in the world — a file
written, a commit made, a network call delivered — that land before the runtime
crashes. The runtime never asserts partial completion: an interrupted invocation
is recorded as failed regardless of whether its side effect landed. The agent's
contract on resumption is to reason from world state, not from the recorded call
result. The conversation thread is a record of intent and runtime decisions, not
a source of truth about external state.

This sets the tool-design rule: a tool's side effects must be detectable by
reading world state, or the tool must be responsible for its own idempotency.
Tools that produce ephemeral, undetectable effects (a fire-and-forget HTTP POST
with no observable response) are unsafe under this contract.

### Model API Call

A model API call is a request to a model provider made by the runtime on behalf
of an agent.

Fields:

- call ID
- agent name
- model name
- route name
- adapter ID and version
- request: messages, tools, parameters
- response: content, tool calls, finish reason
- token counts: input, output, cached
- latency
- estimated cost
- timestamp

Model API calls are durable. They are surfaced in the panopticon and contribute
to cost meters in real time.

### Model Resolution

Model resolution is the runtime behavior that maps a persona's abstract model
preferences to a concrete (model, route, adapter) triple. It runs at agent spawn
time and on failover.

Inputs:

- the persona's model requirements (required capabilities, minimum context
  window)
- the persona's ranked model preferences
- the persona's exclusions
- the set of currently-registered adapters
- the operator's available routes (which routes have working credentials)
- current operational state (rate limits, recent failures)

Algorithm:

1. Walk the preference list in order.
2. For each preferred model, find adapters that serve it on routes with working
   credentials.
3. Filter adapters by required capabilities — an adapter that does not declare a
   required capability is skipped.
4. Filter by current operational state — rate-limited adapters are
   deprioritized.
5. Select the first adapter that satisfies all constraints. If none, fall
   through to the next preferred model.
6. If the entire preference list is exhausted without resolution, agent spawn
   fails with a clear error.

Resolution selects a single (model, route, adapter) triple that is recorded in
the agent's configuration snapshot at spawn.

### Failover

If the resolved adapter becomes unavailable mid-session — sustained rate
limiting, credential failure, network outage — the runtime attempts failover
within the persona's preferences. Failover prefers a different route for the
same model before falling back to a different model. Each failover is recorded
in the agent's conversation thread as a system event and is visible in the
panopticon. Failover is opt-in per persona; some personas may prefer to fail
rather than transparently switch models.

### Cost Meter

A cost meter is the running accumulation of estimated cost.

Fields:

- scope: agent, persona (aggregate), team (aggregate), session (aggregate)
- token counts and costs broken down by model
- ceiling: the lowest applicable ceiling from the agent's capability profile,
  the team configuration (if the agent is part of a team), and any session-level
  cap. The most restrictive value wins.
- ceiling exceeded flag

Cost meters are updated synchronously after each model API call returns. When a
meter crosses its ceiling, the runtime refuses subsequent model calls for the
affected scope and surfaces the event to the operator.

### Audit Log

The audit log is the runtime's canonical record of security-relevant and
operationally-relevant events: authority decisions, model API calls, tool
invocations, transport events (verification, quarantine, delivery), classifier
dispositions, lifecycle transitions, and cost-ceiling trips.

The log is stored as a JSON Lines file under the runtime data directory:
append-only, recoverable, and easily exportable for compliance or post-mortem
use. The runtime maintains an in-memory ring buffer of recent events that serves
the panopticon's hot-path queries; historical queries scan the file directly.
The file is the source of truth; the ring buffer is rebuilt from the tail of the
file on startup.

This choice trades scan cost on rare historical queries for simplicity in v1: no
embedded database, no schema migration, no concurrent-write coordination beyond
the append-only contract. A SQLite (or similar) index can be added later as a
derived view if scan times become a problem; the canonical file does not change.

Audit records carry the version attribution of the producing agent (persona
version, skill versions, memory generation). Records are runtime-owned metadata
derived from runtime decisions, not from message claims.

## Security Layer

Security entities are the identities, keys, and verification ledgers that the
transport security model maintains.

### Identity

An identity is a participant in the Reeve runtime. There are three identity
types:

- **Operator** — a human user. Created via interactive enrollment.
- **Agent** — a running agent instance. Created at spawn by the runtime.
- **External** — a process outside the runtime. Created via operator-approved
  enrollment.

Fields:

- identity_id
- type
- display name
- created_by
- created_at
- expires_at (optional)
- allowed_targets (which agents this identity may address; for external)
- allowed_message_kinds
- capability_scope
- revoked_at (populated on revocation)

Identities are durable. Once retired, an identity ID is never reused.

### Key Record

A key record is a single keypair entry associated with an identity.

Fields:

- key_id
- identity_id
- public key
- status: active | deprecated | revoked
- valid_from, valid_until

An identity may have one active key and any number of deprecated keys.
Deprecated keys verify messages whose `created_at` falls within their validity
window. Revoked keys verify nothing.

Agent session keys are a special case: they are minted at spawn, held in memory
only, never persisted, and invalidated on actor exit, runtime exit, or runtime
lease loss.

### Trust Tier

A trust tier is the runtime classification of a verified message based on sender
identity.

Values:

- operator
- agent
- external
- untrusted (failed verification or unrecognized sender)

Trust tier is assigned by the runtime at message verification. It is not a field
in the message envelope.

### Replay Ledger

The replay ledger tracks accepted message identifiers and nonces per sender,
used to reject duplicates within the retention window.

Fields per entry:

- sender_id
- message_id
- nonce
- accepted_at

Retention: at least the maximum accepted message age plus the clock skew
allowance.

### Delivery Ledger

The delivery ledger tracks message identifiers that have been durably inserted
into agent context, used to ensure idempotent delivery across crash recovery.

Fields per entry:

- recipient_id
- message_id
- delivered_at

The replay ledger and delivery ledger are distinct. Conflating them produces
incorrect behavior on restart.

## Content Security Layer

Content security entities are the classifier outputs and disposition records the
gatekeeper produces.

### Classification

A classification is the output of the gatekeeper classifier on a piece of
content.

Fields:

- classification ID
- classifier model and version
- content hash
- content type
- task scope hash
- risk level
- category labels
- confidence
- bounded rationale

Classifications are durable. They are referenced from authority decisions when
content is involved.

### Content Type

A content type is the surface from which content originates.

Values include but are not limited to: repository source file, repository
documentation, shell output, tool output, external web content, peer-agent
message, external process message, operator message, generated agent output.

Content type is determined by the runtime when content is presented for context
promotion. It is an input to disposition policy.

### Disposition

A disposition is the runtime decision made for a piece of content based on
classification and policy. Values: pass, flag, block.

Disposition is a runtime property, not a classifier property. The classifier
returns classification; the runtime returns disposition. See the gatekeeper
document for the full mapping rules.

## State Ownership

State ownership rules determine where state lives, who can read it, and who can
write it.

### Filesystem (Durable)

The filesystem is the canonical store for:

- All configuration: personas, skills, teams, capability profiles, blacklists,
  classifier policy (under the runtime data directory)
- All memory entries (project memory under `<repo>/.reeve/memory/`; persona and
  operator memory under the runtime data directory)
- Identity registry, key records (public keys), revocation status
- Audit log
- Conversation thread durable history (per-agent log directory)
- Message envelopes in transit (inbox tmp/new/cur/quarantine)
- Status files (the runtime is the writer; everything else reads)

The filesystem is the boot-from-cold source. After a runtime restart, the
runtime reconstructs in-memory state by reading the filesystem.

### In-Memory (Runtime-Owned)

The runtime maintains in memory:

- Agent registry and process handles
- Replay and delivery ledgers (durable on restart from a pruned snapshot, but
  read/write in memory during operation)
- Active classifier connection state
- Cost meters
- Filesystem watcher state
- Active conversation threads (mirrored to filesystem)
- Cached configuration loads (refreshed on file change)
- Agent session private keys (never persisted)
- Operator session private keys (held by the operator, not the runtime, accessed
  via OS keychain)

### Hybrid

Some state is held both in memory and on the filesystem with one canonical
source:

- Conversation thread: filesystem is canonical for durability; in-memory copy is
  the working set
- Status: filesystem is canonical; in-memory state is what the runtime updates
  and writes through
- Audit log: filesystem is canonical; in-memory writer buffers brief windows

## Subsystem Boundaries

Reeve's code is organized along the following boundaries. Crossing a boundary
requires a defined interface; no subsystem reaches into another's internals.

### Runtime ↔ Agent

The runtime is the supervisor. Agents are supervised actors hosted in the
runtime's process. The runtime communicates with an agent via:

- spawn arguments and configuration snapshot
- the agent's message queue (delivered to the agent's context after runtime
  verification)
- structured tool invocation requests from the agent back to the runtime

Agents do not read each other's directories. Agents do not read configuration
directly; the runtime hands them their configuration snapshot at spawn. Agents
request tool invocations from the runtime; the runtime evaluates authority and
returns results.

### Runtime ↔ Configuration

The Configuration Layer is the runtime's source of truth for what may run, how,
and with what authority. The runtime reads configuration artifacts from the
filesystem at startup and on file change (via filesystem watchers), maintains an
in-memory cache of loaded artifacts, and hands each spawning agent an immutable
configuration snapshot. Agents do not read configuration directly; the runtime
resolves persona, skill, capability profile, classifier policy, and memory
generation at spawn and embeds the resolution into the agent's snapshot.

Configuration writes — revisions to personas, skills, teams, memory entries —
come from agents with the `write_configuration` or `write_memory` capability
through tool invocations the runtime serves. The runtime is the only writer of
configuration files in normal operation; operators write through the TUI's
review surface.

A configuration revision takes effect at the next agent spawn from the affected
persona. Agents already running on the prior version are unaffected. Cache
invalidation is event-driven via filesystem watchers.

### Runtime ↔ TUI

The TUI is a client. It connects to the runtime over a local socket. It does not
own state; everything it displays is fetched from or pushed by the runtime. The
TUI may submit messages, request inspection of agent state, and request
operator-approved actions.

The runtime and the TUI ship in the same binary; `reeve` is the launcher for
both and dispatches based on subcommand. There is one runtime per machine per
operator. Multiple TUIs may connect to it simultaneously and are expected to be
the same operator working from different terminals; all TUIs attached to the
runtime sign their submitted messages with that operator's single registered
identity.

### Runtime ↔ Classifier

The classifier is invoked by the runtime and runs locally with no external
network access. Whether it runs in-process or as a sidecar process is an
implementation decision that depends on the classifier model chosen. The runtime
sends content for classification and receives a structured classification
result. The classifier has no other capabilities — no tools, no filesystem
write, no network, no memory across calls. The classifier never enters agent
context.

### Runtime ↔ Model Provider

The runtime is the only component that issues model API calls. Each call goes
through the adapter resolved for the agent's current (model, route) pair. The
adapter translates Reeve's internal protocol to the route's actual API for that
model, handles route-specific retry and rate limiting, and reports back through
the internal protocol. Agents request model calls; the runtime resolves the
adapter (or applies an existing resolution from the configuration snapshot),
executes the call, applies cost ceilings, records the call with full resolution
triple, and returns the result. Agents do not hold provider credentials, do not
talk directly to providers, and are not aware of which adapter served their
call.

### Runtime ↔ Operating System

The runtime uses OS facilities for:

- Process spawning and supervision
- Filesystem event notification (inotify, kqueue)
- OS keychain or credential service for operator key storage
- Local sockets for TUI connection

### Maildir Boundary

The agent inbox directory is the runtime's exclusive responsibility. Senders
write to `tmp/` and rename into `new/`. The runtime reads `new/`, verifies, and
moves to `cur/` or `quarantine/`. Agents do not read the inbox at all. The
agent's view of incoming messages is what the runtime delivers to its context,
not what is on disk.

## Invariants

The following invariants always hold. The runtime is responsible for enforcing
them.

### Identity and Keys

1. An identity ID is unique across the runtime's history and is never reused.
2. An identity has at most one active key at any time.
3. Two runtimes cannot simultaneously hold valid session keys for the same agent
   name.
4. Agent session private keys are in memory only and are invalidated on actor
   exit, runtime exit, or lease loss.
5. Operator and external private keys are never stored in the agent filesystem
   tree.

### Messages and Delivery

6. The runtime is the only path into agent context. Agents do not read inboxes
   directly.
7. Untrusted messages are quarantined and never delivered to agent context.
8. A message in `cur/` has been durably delivered to agent context.
9. The replay ledger and delivery ledger are distinct ledgers with distinct
   semantics.
10. A failed or untrusted message is never redelivered under its original
    claimed identity.
11. Recipient identity in the envelope must match the inbox path the message was
    picked up from.
12. Message filenames are non-authoritative; identity and recipient are taken
    only from the verified envelope.

### Authority and Content

13. Every authority decision is recorded before the action executes.
14. The classifier returns classification; the runtime returns disposition. The
    two are not the same.
15. Classifier output never enters the working agent's context as instruction.
16. Authority decisions compose in order: capability profile category check,
    threshold check, blacklist match, classifier disposition. Failure at any
    layer refuses the action.
17. Blacklist hits are unconditional refusals. There is no global override.
18. Cost ceilings are enforced by the runtime synchronously after each model
    call returns.

### Configuration and Versioning

19. An agent's configuration snapshot is immutable for the lifetime of the agent
    except for failover-driven model resolution updates, which are recorded as
    system events.
20. An agent is always an instance of exactly one persona at one specific
    version.
21. Configuration version increments are monotonic and immutable; a (artifact,
    version) pair always refers to the same content.
22. Memory writes by any agent with `write_memory` capability take effect
    immediately. Each write produces a new version; prior versions are retained,
    and the operator may revert any write through the panopticon.

### Model Resolution

23. A persona never pins to a specific model directly; it declares requirements,
    preferences, and exclusions, and the runtime resolves a concrete (model,
    route, adapter) at spawn.
24. An adapter that does not declare a capability the persona requires is never
    selected for that persona.
25. Failover prefers a different route for the same model before falling back to
    a different model.
26. Every model API call records the resolution triple (model, route, adapter ID
    and version) that served it.

### Observability

27. Every observability event records the version attribution of the agent that
    produced it (persona version, skill versions, memory generation).
28. Every authority decision records the disposition policy version that
    produced it.
29. Audit records are runtime-owned metadata. They are not derived from message
    claims.

### Lifecycle

30. The runtime is the supervisor of last resort. If an agent's supervisor
    fails, the runtime takes over.
31. Restart preserves durable state (conversation history, configuration, audit
    log) and invalidates volatile state (session keys, in-flight tool
    invocations).
32. An interrupted tool invocation on restart is recorded as failed; it is not
    pretended to have succeeded.

## Gotchas and Constraints

The following are non-obvious constraints the runtime imposes. Several appear
inline in the entity sections above; they are consolidated here so implementers
can find them without re-reading the document end to end.

**Cold-start core is frozen for an agent's lifetime.** A memory revision
propagates to running agents only when those agents query the store, not as an
in-flight push to their cold-start core. Operators who require immediate
propagation rely on short agent turnover — fast for subordinates, slow for
long-lived leads.

**The conversation thread records intent and runtime decisions, not external
world state.** A tool invocation logged as completed means only that the runtime
returned a result; the world may or may not have observed the side effect. On
restart, an interrupted invocation is always recorded as failed regardless of
whether the side effect landed. Agents must reason from world state, not from
the recorded call result.

**Tools whose side effects cannot be observed from world state are unsafe under
this contract.** A fire-and-forget HTTP POST that discards its response breaks
idempotency on restart. Tool implementations must either expose their effects to
world-state queries or implement their own idempotency.

**Classifier output never enters agent context as instruction.** It enters only
as a structured input to the runtime's disposition decision. This is enforced by
the boundary between classifier and runtime, not by classifier configuration.

**Routing services are routes, not transparent layers.** OpenRouter and similar
services get their own adapters per (route, model) pair. Treating them as a
universal substrate is a mistake several existing tools have made and is not how
Reeve resolves models.

**Failover is opt-in per persona.** Some personas prefer to fail rather than
transparently switch models. Persona configuration must declare failover
preferences explicitly; the runtime does not failover silently by default.

**Agent names are reusable; identity IDs are not.** Once an agent exits, the
runtime may reuse its name for a future agent of any persona. Identity IDs
(operator, agent, external) are append-only for the runtime's history and never
reused.

## Open Questions

The following items are not resolved by the four architecture documents and
require explicit decisions during implementation.

### Canonical Serialization

The signed envelope requires a canonical byte representation. The choice between
canonical JSON, CBOR, or a custom binary format affects implementation
complexity and forward compatibility. Recommendation pending evaluation.

### Per-Agent Git Worktrees

Several reviewers have suggested that agents operating on the same repository
should default to separate git worktrees to prevent interference. The
architecture documents do not commit to this. It is probably the right default,
but the mechanism — runtime-managed worktrees, agent-requested isolation,
persona-declared isolation — needs a decision.

### Classifier Implementation

The four documents call for a "small local classifier." The specific choice — a
fine-tuned classifier model, a zero-shot small LLM, a hybrid
rules-plus-classifier system — is implementation-deferred but consequential for
performance and accuracy.

### TUI Protocol

The wire protocol between the TUI and the runtime is not specified. JSON-RPC
over a local socket is a reasonable starting point. The decision affects whether
multiple TUI implementations are practical.

These open questions are not gaps in the design; they are the decisions that
move from architecture into implementation. As they are resolved, the
resolutions belong in this document.
