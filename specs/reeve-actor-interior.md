# Reeve — Actor Interior

## Context

Reeve runs AI coding agents as long-lived, addressable actors. Each actor has a
stable filesystem address, an inbox, a durable event log, and supervision under
a tree above the actor boundary. From outside the actor boundary, an actor is
one address that accepts signed messages.

This document describes what is inside that boundary. The interior is not a
single LLM loop. It is a curator that maintains a coherent working context, a
brainstem of mandatory infrastructure beneath it, and cognition as a stateless
function the curator invokes when its policy says deliberation is warranted.
Cognition does not run continuously and does not have agency. The curator does.

## Five Invariants

The interior rests on five invariants. The rest of the document is consequences.

The curator is the actor's locus of agency. Nothing else in the actor decides
what the actor does.

Cognition is never directly addressable. External messages route to the curator;
the curator decides if and when to invoke cognition.

Models propose; the curator disposes. Cognition output, embedding similarity,
and small-model fallback output are all typed input to curator policy. They
never apply directly as authoritative state changes.

Every state transition of consequence is taped. The bus tape is the causal
record.

Direct subsystem addressing is authorized, taped, and bounded by declared
vocabulary. It is a control surface, not an escape hatch around the curator.

## The Inversion

Most agent frameworks treat the LLM as the actor. The model decides when to call
tools, when to respond, when to think harder, and the framework is plumbing
around the model loop. Reeve inverts this. The LLM is a stateless function. The
curator is the central loop. The curator maintains the working context,
integrates inputs, decides when cognition is needed, invokes cognition,
integrates the result, and continues.

The actor's lifecycle is the curator's lifecycle. Cognition is purely
demand-driven. The provider behind cognition (Claude, GPT-4o, a local model) is
interchangeable in a deeper way than configuration. Switching providers swaps
out a function, not the actor.

The curator's most consequential responsibility is the question of attention:
deciding when deliberation should fire.

## The Boundary

An actor has one external address. Senders write to its inbox. Replies leave
through its outbox. The event log records the full history of what the actor
did. From the outside, the boundary is uniform.

Inside the boundary live three tiers of subsystem: the brainstem, the curator,
and cognition. Around the curator sit a small set of contributing satellites.

Actors are supervised from above the boundary. The supervisor is not part of the
actor; it is a separate actor in the runtime supervision tree that monitors
actor health and restarts actors on failure. Inside the actor, a status writer
in the brainstem maintains heartbeat and health information on disk that the
external supervisor reads. This split matters: a supervisor co-located with the
actor cannot restart the actor if the whole actor dies.

## The Brainstem Tier

The brainstem is mandatory infrastructure. It runs continuously regardless of
what the actor is doing.

The cost meter observes invocations and tags them with token consumption. It
does not enforce; it records. Budget enforcement is a curator policy decision:
before invoking cognition, the curator consults the cost meter's recent record
against current budget thresholds. Thresholds are configured per persona and may
be adjusted at forge tier. The supervisor can halt actors that exceed hard
limits.

The status writer maintains the actor's status file on disk so the external
supervisor can monitor health.

The event log writer writes the bus tape.

Brainstem subsystems do not deliberate, are not gated by the dispatcher's
authority checks for routine operations, and cannot be turned off without
breaking the actor.

The brainstem is below the satellite tier. Satellites can be tuned, halted,
added, removed. Brainstem subsystems cannot. They are part of how the actor
exists, not part of how it deliberates.

## Satellites

Satellites contribute to or observe the curator. The memory composer offers
candidate memories. An attention monitor flags risk signals. A safety satellite
can mark items for halt or careful integration. None of these run as pipeline
stages at turn boundaries. They run continuously, alongside the curator,
contributing or observing as their work allows.

The Memory Composer is specified in its own document and establishes the
satellite pattern: a declared vocabulary, attachment to the curator's flow,
tunable thresholds, structured contributions to the bus tape. Future satellites
follow this pattern.

## The Working Context

The working context is the structured representation that cognition reads. It is
not a flat conversation. It is a typed collection of items with metadata and
structural markers.

Items have types: established fact, open question, recent input, retrieved
memory, in-flight tool call, prior cognition output. Items have attribution:
source, timestamp, access count, store of origin for retrieved memories. Items
have structural markers: load-bearing, tentative, resolved, deprecated.

When cognition is invoked, the curator produces a snapshot of the working
context. The snapshot is serialized to whatever the provider's API requires (a
chat-style message list, a structured prompt, a tool-aware payload), but the
internal representation is the curator's, not the provider's. Provider format is
an output adaptation, not an internal structure.

The snapshot is atomic. The curator can keep updating the working context while
a cognition call is in flight. The in-flight call sees the snapshot it was
given. The next call will see whatever the curator has integrated since.

## The Mechanical Core

Almost all of what the curator does is bookkeeping. The mechanical core handles
the work directly in Rust without invoking any model. The principle is to push
semantics into the structure of the data so the model is needed only at the
residual edges.

**Routine bookkeeping.** Appending new inputs to the working context in arrival
order. Tagging items with metadata at integration time. Maintaining structural
markers as items transition states (an open question becomes resolved when a
tool result answers it). Snapshotting the current state when cognition is
invoked. Batching incoming events within a small time window. Routing memory
candidates from the composer into a candidate queue. Recording exposure counts
when items appear in cognition snapshots.

**Hot-path compression as structural reduction.** Compression in the hot path is
representational, not synthetic. When a tool call resolves an open question, the
curator replaces the question and the discussion that led to it with a
structured resolved-fact item that points back to the underlying tape entries.
No prose synthesis happens. The reduction in context size comes from the
structure: many items collapse into one structured pointer item, deprecated
items are excluded from snapshots, items past a configured age are evicted.
Where natural-language synthesis is genuinely required, it is a consolidation
task and runs through the small-model fallback in the background, not on the hot
path.

**Intent propagation through dispatch.** Tool calls carry references to the
questions they are meant to answer. Peer messages carry the operations they
intend. Operator messages carry their type. When results return, the curator
consults the intent metadata to update structural markers without interpreting
content.

Intent metadata is structural input, not truth. The dispatcher trusts intent
fields according to sender tier. Trusted automation can mark operational intent.
Lower-authority senders can mark claimed intent, which the curator records with
attribution and trust level but does not act on without further checks. Forged
intent from low-trust senders cannot manipulate the curator into treating
untrusted content as authoritative.

**Embedding-based deduplication.** When a composer candidate arrives, the
curator checks similarity against items already in the working context using the
same embedding model the composer uses. Near-duplicates are skipped. This is a
vector similarity check, not a generative call.

**Eviction with disposition flags.** When the curator evicts an item to
short-term, it can mark the item with a "promote" disposition based on item type
and structural markers. Resolved schema decisions, established constraints, and
resolved-question items are flagged automatically. No model judgment is required
for the common cases.

The mechanical core is designed to be deterministic, local, and cheap. Ordinary
integration work is bookkeeping-scale, not model-scale. The contrast that
matters is mechanical work versus generative deliberation, not specific timing
claims.

## The Small-Model Fallback

A small generative model handles the residual cases that cannot be reduced to
structure. The fallback proposes; the curator disposes. Fallback output is never
applied as authority. It is typed input to curator policy.

The cases where the fallback is invoked:

**Drift detection.** A periodic background pass, not a per-event check. Once
every N minutes per actor, the small model reviews recent integrations against
established facts and flags semantic drift that mechanical structural comparison
would miss. The flag is a typed report. The curator decides what to do with it.

**Ambiguous cognition policy.** Mechanical heuristics handle the obvious cases.
The genuinely ambiguous cases fall back to the small model for classification.
The small model's output is a recommendation; the curator's policy maps
recommendations to actions.

**Consolidation passes.** A periodic background pass that proposes long-term
memory items as candidates for further promotion. The small model surfaces
candidates with reasoning. The candidates flow as proposals to the persona's
inbox; what happens next is described under Persona Promotion below. The
fallback never directly mutates persona memory.

**Natural-language consolidation compression.** When the curator wants to
compress a stretch of natural-language exchange that has stabilized but cannot
be reduced through structural collapse alone, the consolidation pass produces a
compressed representation. This runs in the background, not on the hot path. The
original tape entries remain untouched; only the working context representation
changes.

The fallback is a shared service across the runtime, not a per-actor resource.
One small-model service serves all actors. Requests are batched per-actor rate
limits prevent any single actor from monopolizing the service. If the service is
unavailable, the curator degrades gracefully: drift detection pauses, ambiguous
cognition decisions fall back to per-message-class defaults, consolidation
pauses, hot-path compression continues to work because it is structural. The
actor keeps working.

The fallback's output is structured. Every invocation returns a typed response:
a drift flag and reason, a yes-or-no cognition recommendation with confidence, a
list of promotion candidates, a compressed representation. This keeps the
integration logic mechanical and the audit trail clean.

## Cognition as Function

Cognition is a stateless function. Snapshot in, output out. No persistent state,
no initiative, no continuous presence. In a Rust function signature, roughly
`cognition::deliberate(snapshot: &Context) -> Output`. Synchronous from the
curator's perspective even when implemented over a network call to a model
provider.

Cognition is not addressable from outside. There is no `target: cognition`.
External messages route to the curator, the curator integrates them, the curator
decides if and when cognition is invoked. This prevents senders from forcing
cognition calls by addressing the model directly, and it places policy in the
right place: with the actor that maintains the context.

## Model Resources

The runtime uses three distinct model resources, each with very different
operational properties. None of them runs continuously per actor.

**Cognition.** The main LLM. Per-actor invocation, demand-driven. Expensive,
slow, episodic. Provider-portable. Configured per persona. Cost is dominated by
this resource.

**Embedding model.** Shared across actors. Used by the composer for retrieval
and by the curator for deduplication. Not a generative model. Converts text to
vectors. Fast, small, runs locally by default. Configurable per persona.

The embedding model does not follow instructions and is not vulnerable to prompt
injection in the generative sense. Retrieval poisoning and semantic collision
remain system-level concerns: poisoned memories, semantic stuffing,
near-duplicate spam, and content that is safe as data but dangerous if
integrated as instruction. These concerns are addressed at the curator's
integration layer, not at the embedding model itself.

**Small generative model.** Shared runtime service for the curator's fallback
work. Called rarely. Can run locally or hosted. Batches requests across all
actors. Rate-limited per actor.

The cost meter tracks each stream separately. Per-actor cost is dominated by
cognition. The embedding model and the small generative model contribute small
bounded amounts. With hundreds of concurrent actors, the dominant variable cost
is cognition, which is itself demand-driven and inspectable per actor.

## Cognition Invocation Policy

The curator decides when to invoke cognition. Most integrations do not warrant
deliberation. Some do. The policy is class-aware: "do not deliberate" is not
always conservative.

Mechanical heuristics handle most cases. For ambiguous cases, the small-model
fallback classifies the recommended action. The curator maps recommendations to
action by message class:

For ambiguous ordinary noise: do not fire cognition. Conservative is
cost-conservative.

For ambiguous operator instruction: fire cognition or, if the small model's
confidence is low, ask for clarification. Conservative is acting on operator
intent rather than dropping it.

For ambiguous safety or authority issues: halt or escalate. Conservative is
refusal to act, not silent integration.

For ambiguous peer action requests: queue as pending rather than silently
ignore. The peer can re-request and the curator will deliberate then.

For ambiguous tool results tied to an open question: mark unresolved and
reconsider on the next deliberation cadence. Open questions don't get silently
dropped because their tool results were ambiguous.

This map is itself a policy that the persona configures. Forge tier may adjust
the defaults for specific actors.

The curator can batch. When deliberation is warranted, the curator may wait
briefly to see if more inputs are arriving, then fire one cognition call against
a more complete context.

## Memory Tiers Within the Actor

The actor's memory has three tiers within itself, plus persona and project tiers
above.

**Working context.** What cognition currently sees. Bounded, freshly curated,
the curator's primary state.

**Short-term memory.** Items recently evicted from working context. Hot,
time-bounded, fast index. Lifetime is measured in minutes to hours. Items can be
retrieved back to working context by the composer if relevance returns. Items
not retrieved during their lifetime, with low usage signals, expire.

**Long-term memory.** Items promoted from short-term. Durable, larger index,
retained for the actor's lifetime. Promotion is governed by usage signals and
curator flags.

The bus tape remains the source of truth. Short-term and long-term are
derivative indexes over tape entries. They can be rebuilt if lost.

## Eviction and Promotion

Eviction from working context is not deletion. It is movement to short-term
memory. The tape is invariant; the item is still in the durable record. The
mechanical core decides what to evict based on age, relevance threshold, and
structural state.

The curator flags items at eviction time with a "promote" disposition based on
item type and structural markers. Confirmed schema decisions, established
constraints, and resolved-question items are flagged automatically.

Promotion from short-term to long-term is governed by usage signals plus
structural flags. Usage signals are tracked at multiple levels:

- **Retrieval count.** The composer surfaced this item as a candidate. This is a
  weak signal: the retrieval system thought the item might be relevant. It does
  not mean the item was useful.
- **Integration count.** The curator accepted this item into working context.
  This is a stronger signal: the item passed the curator's filters.
- **Exposure count.** This item appeared in a cognition snapshot. Stronger
  still: cognition actually saw it.
- **Reference count.** Where detectable, cognition output or tool calls
  explicitly referenced this item. Strongest signal of usefulness.

Promotion policy weights integration, exposure, and reference heavily. Retrieval
contributes weakly or not at all. This prevents a feedback loop where vaguely
related candidates self-promote by repeatedly matching broad query states. Items
that pass the composer's filter but never pass the curator's do not advance.

The convergent pathways for promotion:

If integration and exposure counts exceed the per-persona promotion threshold,
promote.

Else if the curator flagged the item as important at eviction time, promote.

Else if the consolidation pass has identified the item as structurally connected
to other retained items, promote.

Else, expire.

Counters are derivative index state. The tape records the source events. If an
index is rebuilt, counters are recomputed from tape.

Once in long-term, items continue accumulating usage signals on retrieval,
integration, and exposure. The consolidation pass can propose to the persona
that long-term items with sustained high usage be promoted further to persona
memory. The proposal flows to the persona's inbox; the persona-curator process
described below handles it.

## Persona Promotion

Persona memory crosses actor lifetime. An item promoted to persona memory
becomes available to every future actor of that persona. This is high-leverage
and requires deliberation, not just policy rules.

Persona promotion is itself actor work. Each persona has, by convention, a
designated curator actor (or team of actors) whose job is to evaluate and decide
on persona modifications. This is not architecturally privileged. It is an actor
like any other, with its own working context, its own memory, its own learning
over time about which kinds of proposals belong in its target persona's memory.
Its keys are registered at the tier the persona's dispatcher accepts for writes,
which by convention is called forge tier.

When a long-term memory item is proposed for persona promotion, the proposal
flows to the target persona's inbox as a structured message. The persona's
dispatcher routes proposals to its designated curator actor. The curator actor
deliberates, possibly using cognition for hard cases, and decides whether to
accept, defer, or reject. If accepted, the curator actor sends a signed write
message at forge tier to the persona's mutable state.

Operators can replace, augment, or constrain persona-curator actors. The runtime
provides defaults; operators decide what they actually want. Multiple curator
actors can serve a single persona for redundancy or for specialization. The
architecture does not care.

The same pattern applies to other cross-actor operations. Propagation (pushing a
persona's new defaults to all running actors of that persona) is performed by an
actor whose job is to do this when persona versions change. Index pruning,
archival, and other long-running maintenance are also actor work, not runtime
infrastructure.

This is the core property: there are no privileged actors in Reeve.
Configuration, tuning, promotion, and maintenance are all performed by actors
through the standard message system. The runtime provides substrate, not policy.
Trust differences are tier differences in the key registry, not structural
differences in the runtime.

## Snapshot Semantics and Latency Hiding

The canonical working context is held in a shared cell. Updates produce new
versions and atomically swap. Readers always see a consistent view.

When cognition is invoked, a reference to the current version is captured and
serialized for the call. The curator can keep updating the underlying state
without affecting the in-flight call. This is the same atomicity discipline used
elsewhere in Reeve, applied at the in-memory layer rather than the disk layer.

This produces latency hiding for free. While a cognition call is in flight, the
curator can be integrating new tool results, peer messages, and bus tape events.
By the time the call returns, the next snapshot is already most of the way
prepared.

## Serialization for the Provider

The curator's internal representation is structured for the curator's benefit:
typed items, attribution metadata, lifecycle states, intent markers,
store-of-origin tags, access counters, structural relationships. None of this is
for cognition. Cognition needs the content with appropriate framing, not the
bookkeeping that the curator uses to manage it.

A serialization adapter sits between the curator and the provider. Its job is to
produce the smallest representation of the snapshot that preserves what
cognition needs. The internal representation can be as rich as it needs to be.
The serialized output should be parsimonious.

The serialization principle is that internal metadata that helps the curator
track items does not appear in the cognition input. Cognition does not need to
know a memory's source store, access count, or lifecycle state. It needs the
content and a brief marker of how to weight it. Conventional brief framing
("FACT:", "OPEN:", "RECENT:") costs little and orients cognition usefully
without leaking bookkeeping.

This matters because input cost on most providers scales with serialized input
size, regardless of how the chat or completion API is structured. A naive
adapter that exports the curator's full structured representation would multiply
the per-invocation cost without improving cognition's reasoning. A parsimonious
adapter pays only for content cognition can actually use.

The adapter is also cache-aware. Most major providers offer prompt caching:
stable prefix regions of input cost a fraction of the full price when they hit
cache. The adapter produces serialized output in two regions: a stable substrate
(persona prompt, established load-bearing facts, long-lived structural framing)
and a changing tail (recent inputs, current open questions, the immediate
query). The substrate hits the cache; the tail pays full price. Each cognition
invocation pays full input cost only for the tail.

The substrate / tail split is a property of the adapter, not of the curator.
Different providers cache differently, and adapters are configurable per
provider. The curator does not need to know how its snapshot will be cached; it
produces structured output and lets the adapter handle provider specifics.

This combines well with the curator's continuous editing. A standard chat-agent
loop accumulates conversation history monotonically and the cacheable region
grows messy as old turns linger. The curator continuously evicts items that have
served their purpose, which keeps the substrate stable and meaningful and the
tail short. Long-running actors do not accumulate runaway input costs because
the curator is editing the substrate, not appending to it.

## The Bus Tape

Internal events use Tokio channels. They are fast, ordered, never serialized.
They cost almost nothing.

Every event of consequence is taped to a structured stream. The tape is the
observation layer. The panopticon reads it. Subsystems that need to react to
other subsystems' activity subscribe to it. The tape is what makes the
high-performance internal substrate introspectable from outside. There is no
second communication channel for visibility. There is one substrate for speed
and one tap for observability, and the tap captures everything.

The tape records every cognition invocation, including the snapshot reference
and the output. Every small-model fallback invocation with input and structured
output. Every embedding call with query and candidates. Every curator
integration. Every dispatcher decision. Every eviction, every compression, every
promotion. Every authority rejection.

The actor's decision history is causally reconstructable from the tape. Given
the taped snapshots, policy versions, model invocations, structured outputs, and
integration events, an operator can inspect why the actor reached a state or
invoked cognition. Byte-exact replay is not guaranteed across provider changes,
model version updates, or external side effects, but the causal chain of
decisions is recoverable.

For replay-grade reconstruction, the tape carries version qualifiers: model
provider and version, prompt adapter version, embedding model version, scoring
policy version, threshold values in effect, and curator code version. With these
in hand, replay across compatible versions is meaningful. Across incompatible
versions, the tape still answers "why did the actor do this" even if it cannot
answer "would this happen again."

## The Dispatcher

External messages addressed to the actor carry a target field in their envelope.
The default target is the curator. Any addressable subsystem can be addressed
directly: `target: memory.composer`, `target: curator`, `target: cost.meter`.
The dispatcher reads the target and routes accordingly.

Direct subsystem addressing is a control surface, not an escape hatch. Every
message routes through the dispatcher's authority check. Every accepted message
lands in the bus tape. Every state change a subsystem makes in response to an
addressed message is itself taped. The curator may not be the recipient of every
message, but no subsystem receives untaped, unaudited, or curator-invisible
state changes. Operations that affect the curator's working context still flow
through the curator. Operations that tune a satellite's parameters land at the
satellite and are recorded; the curator sees the tape and can react.

Senders that do not know about subsystems omit the target and reach the curator.
The curator integrates the message into the working context. Senders that do
know, including the panopticon, persona-curator actors, and operators who have
learned the vocabulary, can address any subsystem with precision.

Each subsystem declares a vocabulary, which is the set of message types it
accepts with schemas. The vocabulary is what the panopticon renders as controls
and what other actors program against. Subsystem replies route back through the
actor's outbox with subsystem-level attribution.

Messages also carry intent metadata: the operation type, references to prior
items they relate to, and other structural hints. The dispatcher and the curator
operate on this metadata according to sender trust level.

## Authority

The dispatcher enforces authority before routing. Each subsystem declares
per-tier access in its vocabulary. The dispatcher consults the declaration and
rejects unauthorized messages before the subsystem ever sees them.

Defaults are deny. If a subsystem does not declare access for a tier, that tier
cannot address it. New satellites do not accidentally inherit broad access.

The curator is the most permissive target. Peer actors, operators, and trusted
automation all need to reach it. Other subsystems ratchet up. Forge tier sits
above operator tier and is required for durable tuning of the curator's policy
or any satellite's parameters.

Forge tier is a tier, not a team. Any actor (or operator) holding keys
registered at this tier can perform tuning operations. Persona-curator actors
typically hold these keys. Operators may also hold them directly. The runtime
does not care who holds the keys; it only checks that the signature matches a
registered tier.

Every authority rejection is recorded with full attribution. The bus tape shows
what was attempted, by whom, against which target.

## The Curator's Vocabulary

The curator accepts the following message types on the dispatcher.

`set-compression-threshold` adjusts how aggressively the curator compresses
earlier context. Forge tier required for durable change.

`set-eviction-threshold` adjusts how aggressively the curator moves items from
working context to short-term. Forge tier required for durable change.

`set-promotion-threshold` adjusts the thresholds at which short-term items
promote to long-term. Forge tier required for durable change.

`set-cognition-policy` adjusts the per-message-class defaults the curator uses
to decide when to invoke cognition. Forge tier required for durable change.

`set-batching-window` adjusts how long the curator waits to accumulate inputs
before firing cognition. Forge tier required for durable change.

`set-drift-cadence` adjusts how often the small-model fallback runs drift
detection. Forge tier required for durable change.

`override-temporary` allows an operator to set any of the above values for the
current actor session, scoped to expire on restart or after a duration. Operator
tier required. Always taped.

`compress-now` triggers an immediate consolidation pass. Operator tier required.

`force-cognition` invokes cognition immediately regardless of policy. Operator
tier required.

`report-state` returns the current working context size, recent integration
history, cognition invocation history, fallback invocation history, and tier
statistics. Any signed sender.

The vocabulary is the surface the panopticon renders as controls and
persona-curator actors program against.

## Failure Handling

The mechanical core cannot fail in the same sense the model layers can.
Bookkeeping bugs are fixable; they are not stochastic. The curator's main
failure modes live in the model layers and in operational degradation.

The small-model fallback can be wrong. It can flag spurious drift, misclassify
ambiguous cognition decisions, propose poor consolidation candidates. The cost
is bounded: cognition reasons against slightly noisier context, or fires at
slightly the wrong moment. Recoverable on subsequent turns.

The small-model service can be unavailable. The curator degrades gracefully.
Drift detection pauses. Ambiguous cognition decisions take per-message-class
defaults. Consolidation pauses. The mechanical core continues unaffected.

The embedding model can be unavailable. The composer falls back to lexical
retrieval only. Quality degrades but the actor keeps working. Dedup checks at
the curator are skipped during the outage; some duplicates may enter working
context until the model returns.

If the curator process crashes, the supervisor restarts it. On restart, the
curator rebuilds working context from the recent tape. This takes longer than
normal startup but is bounded. Short-term memory may have lost some recency in a
crash. This is acceptable because short-term is by nature ephemeral.

## Restart Behavior

On actor restart, the curator rebuilds its state from the durable record.

The working context is reconstructed by replaying recent tape integrations until
a configured time horizon or item count is reached. This produces an
approximation of the pre-crash state. Cognition's first invocation after restart
sees a context that is consistent if not byte-identical to what it would have
seen.

Short-term memory's index is rewarmed from recent tape entries. Some recency may
be lost.

Long-term memory's index is loaded from disk. It is durable across restart by
design.

The curator does not attempt to recover in-flight cognition calls. If a call was
in flight at the moment of crash, it is recorded as failed. The curator
integrates the failure and reasons about it on the next deliberation.

## Properties

Several properties are worth claiming.

The curator owns cognition's input. The LLM does not see whatever happens to be
in the conversation. It sees what the curator has decided should be in the
conversation.

The curator's hot path is mechanical. Bookkeeping-scale work, deterministic
given the same inputs. The model layers are bounded fallbacks for residual
cases, not the primary mechanism. This makes the architecture testable,
debuggable, and tractable at scale.

Per-actor cost is bounded and dominated by cognition. Embedding queries and
small-model fallback invocations contribute small fixed components. With
hundreds of concurrent actors on disk, the dominant variable cost is cognition,
which is itself demand-driven and inspectable per actor through the cost meter.

Per-invocation input cost is bounded by the snapshot size, not by session
history. Long-running actors do not accumulate runaway input costs because the
curator continuously edits the working context rather than letting conversation
history grow monotonically. Combined with cache-aware serialization that places
stable substrate ahead of a short changing tail, the variable input cost per
cognition invocation is small relative to the chat-agent baseline. Cognition
invocation frequency is also lower than a chat-agent loop, because the curator
fires deliberation only when its policy says it is warranted; most events
integrate mechanically without invoking cognition at all. The two effects
compound.

The actor's apparent memory span is its full lifetime. Working context is small.
Short-term and long-term extend the span. The composer's retrieval pulls items
back to working context as needed. Most agent frameworks today have an apparent
span equal to the context window. Reeve's apparent span is bounded only by the
tape.

The actor has two modes of cognition: a fast, narrow, near-continuous curator
that maintains coherent state, and a slower, broader, episodic deliberation
function invoked only when policy warrants. The curator maintains reality.
Cognition deliberates against the reality the curator constructs.

Memory is self-organizing. Items integrated and exposed often migrate toward
durable storage. Items that fail to pass the curator's filters do not advance
regardless of how often they are retrieved. The migration is observable through
the tape and tunable through persona thresholds.

Provider portability is structural. Switching cognition from one provider to
another does not affect the curator, the working context, the integration
policy, or any satellite. The actor is the curator. The LLM is a function the
curator consults.

There are no privileged actors. Configuration, tuning, promotion, and
maintenance are all performed by actors through the standard message system. The
runtime provides substrate, not policy. Trust differences are tier differences
in the key registry, not structural differences in the runtime.

These are the properties to defend.

## Relation to Other Documents

The Memory Composer note specifies the first contributing satellite, scoped to
retrieval and candidacy across short-term, long-term, persona, and project
memory. It establishes the pattern future satellites follow.

The Persona as Live Actor note specifies how subsystem state and defaults flow
between actors and personas, mediated by persona-curator actors operating at
forge tier.

The Versioned Disk Substrate note specifies how state is persisted and
versioned, and how the bus tape relates to derivative indexes.

The Actor Interior is the foundation. The other notes build on it.
