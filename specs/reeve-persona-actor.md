# Reeve — Persona as Live Actor

## Context

Reeve distinguishes between a persona, which is a defined role with a prompt,
capabilities, default model, skills, and defaults, and an actor, which is a
running instance of a persona. The Reeve overview introduces this split. This
document specifies the live relationship between the two.

The persona is not a static configuration loaded once at actor spawn. It is a
live actor with its own address, its own dispatcher, and its own evolving state.
Actors draw from their personas at spawn time and contribute back to their
personas over their lifetime, mediated by persona-curator actors that do the
deliberative work of deciding what belongs in persona memory.

## The Persona Boundary

Every persona has an address. `persona:coder` is a real address that accepts
signed messages. The persona's filesystem layout mirrors an actor's: a directory
containing configuration, an inbox, an event log, and subsystem state for the
parts of the dispatcher that handle persona-level concerns.

The persona has no cognition subsystem. It does not deliberate. Its dispatcher
accepts a narrow vocabulary related to its own state: memory writes, default
updates, skill changes. Anything else is rejected.

## Downward Materialization

When a persona spawns an actor, the persona's current state materializes into
the new actor. The persona's defaults flow into the actor's subsystem state. The
persona's memory selection rules flow into the actor's memory composer. The
persona's skill set flows into the actor's available skills. From that point
forward, the actor reads from its own state, not the persona's.

Downward materialization is unconditional. Spawning is the only operation that
causes it. Once an actor is running, changes to its persona's state do not
propagate automatically. This avoids surprising mid-flight reconfigurations and
keeps the runtime model predictable.

## Upward Proposals

Actors can address their persona for a small set of operations. These are
proposals, not direct writes. The actor constructs a message, signs it with its
instance keypair, and addresses `persona:<name>`. The persona's dispatcher
receives the message and routes it for evaluation.

Direct writes from instances to personas are rejected. Personas are
organizational memory. A single instance with a weird run should not be able to
corrupt the persona. Proposals must be evaluated by an actor with appropriate
authority before they become writes.

The operations that flow upward are a narrow set.

Memory promotion. An instance proposes that something it learned during its run
is worth retaining for future instances.

Tuning promotion. An instance's current subsystem configuration is proposed as a
new persona-level default.

Skill update. A refined skill fragment is proposed.

Failure registration. An encountered pitfall is proposed as a guardrail for
future instances.

Each upward operation has an explicit message type. The persona's vocabulary
declares which of them it accepts and at what authority tier.

## Persona-Curator Actors

Persona promotion requires deliberation. Whether something belongs in persona
memory depends on its content, its source, the persona's current state, and
patterns across other recent proposals. This is not a question rules can answer
well. It is the kind of question actors are good at.

Each persona has, by convention, a designated curator actor (or team of actors)
whose job is to evaluate proposals to that persona. This actor is not
architecturally privileged. It is an actor like any other, with its own working
context, its own memory, and its own learning over time about which kinds of
proposals belong in its target persona's memory. Its keys are registered at the
tier the persona's dispatcher accepts for writes, called forge tier by
convention.

When a proposal arrives at a persona's inbox, the persona's dispatcher routes it
to the persona's designated curator actor. The curator actor deliberates,
possibly using cognition for hard cases, and decides whether to accept, defer,
or reject. Accepted proposals become signed write messages from the curator
actor to the persona's mutable state, at forge tier.

The curator actor's decisions are taped, just as any other actor's decisions are
taped. Operators can inspect why a proposal was accepted or rejected by reading
the curator actor's bus tape. Over months, the curator actor's tape becomes a
record of how the persona has been shaped.

Operators can replace, augment, or constrain persona-curator actors. The runtime
spawns defaults but operators decide what they actually want. Multiple curator
actors can serve a single persona for redundancy or specialization. The
architecture does not care.

The same pattern applies to other cross-actor operations. The propagation
operation (push a persona's new defaults to all running actors of that persona
when the persona version changes) is performed by an actor whose job is exactly
this. It reads new persona versions, enumerates running instances, and sends
instance-tuning messages. Same pattern as everything else: an actor doing work
through messages.

This produces a real architectural property: there are no privileged actors in
Reeve. Configuration, tuning, promotion, and maintenance are all performed by
actors through the standard message system. The runtime provides substrate, not
policy. Trust differences are tier differences in the key registry, not
structural differences in the runtime.

## Three Operations on Personas

Three operations dominate cross-actor work.

**Instance tuning.** An actor at forge tier sends `set-threshold` or other
vocabulary messages directly to a running actor, scoped to that instance. The
actor's subsystem state changes. The persona is unaffected. This is local and
does not require persona-level deliberation.

**Default promotion.** An actor at forge tier reads a running actor's current
subsystem state and proposes it as the persona's new default. The proposal flows
to the persona. The persona-curator actor evaluates and decides. Accepted
proposals become writes to the persona file. New actors spawned afterward
inherit the new default. Existing actors are unaffected.

**Propagation.** An actor at forge tier reads the persona's current default and
pushes it to all running actors matching that persona. Each actor receives an
instance-scoped tuning message. The dispatcher applies it. The on-disk state of
each actor is updated. The bus tape records the propagation for every actor.

These three operations cover the common cases. Instance tuning handles
experiments. Default promotion captures wins. Propagation forces synchronization
when the desired change is "every running instance should adopt this."

## Persona Evolution

Personas evolve. They are not static configuration. They accumulate memory,
tuning, and skill refinements as their instances do work and as their curator
actors make decisions. A six-month-old `coder` persona is meaningfully different
from a one-week-old one, in ways that are inspectable on disk and reviewable in
the project's version control history.

The persona file is the source of truth for what the persona has learned.
Because personas are committable, the project's history includes the history of
how its actors have grown.

Rollback is a one-line operation. Repoint the persona's `current` version. The
next actor of that persona spawns from the older defaults. No code change, no
deploy. The Versioned Disk Substrate note specifies how this works.

## Authority

Writes to personas require forge tier in the default policy. Persona-curator
actors typically hold these keys. Operators may also hold them directly when
they curate personas themselves. Instance-tier actors propose only; they cannot
write to personas.

The persona's dispatcher enforces this. Proposals that arrive without
curator-actor involvement (or operator authority) are queued. The queue is
itself observable through the panopticon, with the same patterns used everywhere
else.

Forge tier is a tier, not a team. Anyone holding the appropriate keys can act at
this tier, including operators, curator actors, propagation actors, and any
other actors granted the keys for their work. The runtime checks signatures; it
does not care about the holder's identity beyond what tier their keys are
registered at.

## The Persona's Vocabulary

The persona accepts the following message types on its dispatcher.

`propose-memory` submits a candidate item for persona memory. Any signed sender
at instance tier or above. Routes to the curator actor for evaluation.

`propose-tuning` submits a current subsystem configuration as a candidate
persona default. Forge tier required (typically from a curator actor acting on
behalf of an instance's accumulated state).

`propose-skill` submits a refined skill fragment. Forge tier required.

`register-failure` submits an encountered pitfall as a candidate guardrail. Any
signed sender at instance tier or above.

`commit-write` applies an evaluated proposal to persona state. Forge tier
required. Curator actors send these after deliberation.

`report-state` returns the persona's current version, recent proposal queue,
recent commit history, and curator-actor attribution. Any signed sender.

The vocabulary mirrors the proposal-and-deliberation pattern. Senders propose.
Curator actors evaluate. The persona's state advances only when a curator actor
commits a write.
