# Reeve — Memory Composer

## Context

This is the first contributing satellite specified against the Actor Interior
architecture. The Memory Composer queries the actor's memory stores when query
state changes materially, scoring and offering candidate memories to the
curator. It is mechanical, deterministic, and cheap. It contains no generative
model.

The composer does not inject memories into cognition's context. The curator does
that, integrating composer candidates with the rest of the working context. The
composer's job is retrieval and candidacy, not integration.

## Two Invariants

The composer rests on two invariants. The rest of the document is consequences.

The composer never mutates working context. It retrieves, scores, and offers
candidates. The curator is the only subsystem that integrates memory into
working context or cognition snapshots.

Composer output is data, not instruction. Retrieved memories retain source
attribution, authority tier, and trust markers through integration. The composer
offers; the curator's content-boundary policy disposes.

## Problem

Actors need memory. Loading every relevant memory at spawn time is wasteful.
Most of it will not be used in any given turn. Fetching memory on demand inside
cognition requires the LLM to recognize that it should fetch, which is itself
non-trivial cognition that costs tokens. The composer handles retrieval
mechanically, leaving cognition to focus on deliberation.

## Approach

A retrieval pipeline maintains query state derived from the curator's working
context and runs queries against the actor's indexes when meaningful triggers
fire. Results are scored, suppressed against recently rejected candidates, and
offered to the curator with attribution.

The pipeline uses two complementary mechanisms. A lexical index (`tantivy`)
handles exact term matches and fast keyword retrieval. An embedding model
handles semantic similarity, catching cases where surface terms differ but the
underlying concept matches. Both run in parallel. Their scores are combined and
thresholded.

The embedding model is not a generative model. It converts text to vectors. It
does not produce free-form output and is not vulnerable to prompt injection in
the generative sense. Retrieval poisoning and semantic collision remain real
concerns: poisoned memories, semantic stuffing, near-duplicate spam, and content
that is safe as data but dangerous if integrated as instruction. The composer
therefore returns attributed candidates with trust markers; the curator applies
content-boundary policy before integration.

Behavior is replayable given the same query state, index contents, embedding
model version, vector index configuration, scoring policy version, and threshold
values. The composer can be replayed from the tape and inspected.

## Triggers

The composer does not query on every event. It maintains query state
continuously and fires retrieval only when:

- Query state crosses a configured change threshold (significant new content has
  entered working context, an item's structural marker has changed materially)
- An open question is added to the working context or its content is materially
  changed
- The curator's cognition invocation policy is about to fire and requests fresh
  candidates
- An operator sends `query-now` explicitly

This trigger model keeps retrieval responsive without turning memory into event
spam. Small changes update query state silently; only meaningful deltas produce
candidates.

## Query Construction

Query construction is mechanical. The composer extracts a query state from the
curator's recent integrations using:

Term extraction over recent items, weighted by item type, structural markers,
and recency. Load-bearing items contribute heavily. Tentative items contribute
less. Deprecated and resolved items are excluded.

Entity recognition through a small heuristic library: file paths, function
names, identifiers, named concepts, error strings.

Open-question items contribute their normalized text and trusted structural
metadata. Untrusted quoted content inside an open question is included as data
with reduced weight, not as authoritative query terms.

Negative evidence is excluded explicitly: deprecated items, resolved items
unless still load-bearing, raw tool noise, repeatedly rejected candidates,
content from low-authority untrusted senders, and large blobs not reduced to
structural form.

The result is a structured query: a weighted term set for the lexical index and
a piece of normalized text for the embedding model. The same query state feeds
both retrieval mechanisms.

There is no generative model in the query construction path. The curator's own
work in maintaining typed items with structural markers does most of the
semantic work; the composer reads the structure and constructs a query
mechanically.

## The Stores

The composer queries four stores and tags every candidate with source
attribution. Implementations may query short-term first for latency, but scoring
does not collapse store identity into a single undifferentiated relevance score.
Per-store thresholds apply.

**Short-term memory.** The actor's recent evictions from working context.
Per-actor index, hot, ephemeral. Items live for a configurable retention window
measured in minutes to hours.

**Long-term memory.** The actor's promoted items. Per-actor index, durable,
opened lazily. Items arrive here through promotion from short-term.

**Persona memory.** Items at persona scope, available to every actor of this
persona. Shared by persona, not duplicated per actor.

**Project memory.** Items at project scope, available to every actor operating
in this codebase. Shared by project, not duplicated per actor.

Per-actor footprint is small: a hot short-term index and a lazily-opened
long-term index. Persona and project indexes are loaded once per scope and
shared across all actors that need them. Inactive actors do not keep their
indexes hot. With hundreds of actors on disk, this composes to a tractable
resource footprint.

The curator uses tier attribution as one signal during integration. A hit from
short-term means "you literally just saw this." A hit from persona means "you
knew this before the session began." Tier attribution preserves the texture of
memory the way the brain does.

## Shared Embedding Model

The embedding model is a runtime resource shared across all actors. It is not
loaded per actor. The composer queries it. The curator queries it for
deduplication when integrating composer candidates. One model, multiple
consumers, batched inference.

This matters operationally. With hundreds of actors on disk, per-actor model
loading would be untenable. A single embedding service handles all actors.

The embedding model is configurable per persona. A coder persona might use a
code-aware embedding. A research persona might use a general-purpose embedding.
The runtime loads the relevant models once and routes queries by persona.

## Candidate Lifecycle

Candidates have explicit lifecycle states. The composer tracks state per
candidate and uses it to suppress noise.

**Generated.** Retrieval scored this item above threshold. Candidate exists
internally to the composer.

**Offered.** The composer has surfaced this candidate to the curator.

**Skipped as duplicate.** The curator's dedup check found the candidate already
present in working context. The composer records this and applies a suppression
window.

**Rejected by policy.** The curator declined to integrate the candidate (content
boundary, authority, structural conflict). The composer records this and applies
a suppression window.

**Integrated.** The curator accepted the candidate into working context.

**Exposed.** A snapshot containing the integrated item was passed to cognition.

**Expired.** The candidate's offer aged out without the curator acting on it.

**Superseded.** A newer candidate replaced this one for the same query
signature.

Once a candidate is offered and rejected or skipped, it is not re-offered for
the same query signature until either the query state changes materially or the
suppression window expires. This rule prevents the same candidate from
cluttering the curator's queue every few seconds.

## Usage Counters

The composer records retrieval events, but retrieval alone is a weak signal. It
means the retrieval system thought the item might be relevant; it does not mean
the item was useful.

Each item maintains separate counters:

**Retrieval count.** The composer surfaced this item as a candidate.

**Integration count.** The curator accepted this item into working context.

**Exposure count.** This item appeared in a cognition snapshot.

**Reference count.** Where detectable, cognition output or tool calls explicitly
referenced this item.

The composer increments retrieval count as a side effect of surfacing. The
curator increments integration count when accepting. The snapshot mechanism
increments exposure count when a cognition call is fired against context
containing the item. Reference count requires post-cognition analysis and is
best-effort; not every reference is detectable.

Promotion policy weights integration, exposure, and reference heavily. Retrieval
contributes weakly or not at all. This prevents a feedback loop where vaguely
related candidates self-promote by repeatedly matching broad query states.

Counters are derivative index state. The tape records the source events. If an
index is rebuilt, counters are recomputed from tape.

## Pipeline Position

The composer is a contributor to the curator, not a pipeline stage on the path
from input to cognition. It runs continuously alongside the curator. When
triggers fire, retrieval runs in the background and offers candidates. The
curator absorbs candidates whenever it next runs an integration pass.

There is no synchronous "compose then deliberate" sequence. Cognition fires when
the curator's policy says it should, against whatever the curator's current
snapshot reflects.

## Vocabulary

The composer accepts the following message types on the dispatcher.

`set-threshold` adjusts the relevance threshold for candidacy. Per-store
thresholds can be set independently. Forge tier required for durable changes;
thresholds affect cognition input and are not pure inspection.

`set-threshold-temporary` allows an operator to set thresholds for the current
actor session, scoped to expire on restart or after a duration. Operator tier
required. Always taped.

`set-embedding-weight` adjusts the relative weighting of embedding similarity
versus lexical matching in the combined score. Forge tier required.

`set-suppression-window` adjusts how long rejected or skipped candidates are
suppressed before being eligible to re-offer. Forge tier required.

`query-now` runs an immediate query against specified stores and returns
candidates without offering them to the curator. Useful for inspection and
testing. Operator tier.

`clear-cache` evicts cached intermediate state. Operator tier.

`report-state` returns current thresholds, store sizes, recent query history,
candidate lifecycle statistics, and counter summaries. Any signed sender.

The vocabulary is small by design. The composer's job is narrow.

## Tuning

Per-store thresholds are the primary tuning knob. Higher thresholds offer fewer
candidates and miss more. Lower thresholds offer more candidates and add noise.

The relative weighting of embedding similarity versus lexical matching is the
second knob. Code-heavy work benefits from heavier lexical weight.
Conversational work benefits from heavier embedding weight.

The suppression window is the third knob. Longer windows reduce candidate churn
but slow recovery when an item's relevance genuinely changes. Shorter windows
respond faster but accept more noise.

Tuning is empirical. Every candidacy is logged on the bus tape with the
candidate item, the store it came from, the score, the lifecycle state, and what
the curator did with it. A tuning pass over the tape can adjust thresholds and
weights based on integration and exposure rates, not raw retrieval rates.

## Cost Discipline

The composer adds work for the curator (each candidate requires evaluation) and
potentially adds context to cognition input. Per-store thresholds and
suppression windows are the levers for managing this cost.

Token cost is observed by the cost meter satellite, which tracks cognition
invocations. The composer does not consume tokens directly. Its cost is CPU and
index size, plus a small share of the shared embedding model's load.

## Failure Handling

The lexical index is a local file. If it becomes unavailable, retrieval fails
for that store and the composer logs an error. Other stores continue to
function.

The embedding model service can be unavailable. The composer falls back to
lexical-only retrieval until the service returns. Quality degrades (semantic
generalization is lost) but the actor keeps working. The curator's dedup checks
similarly degrade during the outage; the composer's own duplicate suppression
continues to work since it operates on candidate identity, not embedding
similarity.

If both fail, the composer pauses and the actor operates without retrieval.
Cognition still has access to the working context the curator maintains. The
actor reasons within its current context until retrieval is restored.

## Summary

The composer is a non-generative retrieval satellite. It maintains a
mechanically derived query state from the curator's working context, retrieves
candidate memories from short-term, long-term, persona, and project stores using
lexical and embedding indexes, suppresses duplicates and recently rejected
candidates, and offers attributed candidates to the curator. It does not
integrate memory, invoke cognition, or promote memories. Its output is
structured evidence for curator policy.

The composer's job is boring: maintain query state, retrieve candidates, score
them, attribute them, suppress noise, tape everything, and stay out of the
curator's way.
