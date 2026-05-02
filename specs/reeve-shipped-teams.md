# Reeve — Shipped Teams and Self-Improvement

## Context

Reeve is a local coding tool that runs AI agents on a developer's workstation as
named, persistent, addressable, supervised actors. The architecture is described
in the Reeve overview and the two security documents.

This document covers two related concerns: what Reeve ships with on first run,
and how Reeve's configuration improves itself in response to how it is used.
Both are product-level guidance that depends on the architecture but is not part
of it.

## The Problem

Reeve's value is emergent from multiplicity. A single agent is a slightly-better
Claude Code. The panopticon, the addressability model, the persona/agent split,
the delegation story — none of it is legible until the user sees more than one
agent running. At N=1, the product looks like a chat prompt with extra concepts.

The first-run experience has to show the shape of the tool immediately, not
invite the user to assemble it. And once the user is running real work, the path
to improving their configuration has to be inside Reeve, not outside it.

## First-Run Requirements

**Ship a working default team.** `reeve` with no configuration should spawn a
populated team — something like lead, reviewer, and tester — not an empty
runtime with a chat prompt. The user's first view of the panopticon should
already have rows in it.

**Start regardless of project context.** `reeve` invoked outside a repository
starts the runtime and spawns the default team using whatever context is
available — operator memory, persona defaults, the current working directory if
there is one. The team's lead is responsible for orienting the operator: asking
what to work on, pointing the runtime at a project, or starting in a temporary
workspace. The runtime does not refuse to start without a project; the team
handles the situation.

**The default team must demonstrate delegation on a realistic first task.**
Asking the lead to "add a function with tests" should visibly spawn the tester.
"Refactor this module" should spawn the reviewer. The user learns what
delegation looks like by watching it happen, not by reading about it. If the
lead handles the first task alone, the tool has failed to teach itself.

**Shipped personas must be good enough to copy from, not build from.** Most
users will never author a persona. The default personas should be strong enough
that the user's instinct is to ask for another one like that rather than to
write their own.

**Ship a populated skill library.** Composing a new persona should feel like
picking ingredients, not inventing them. The skills shipped with Reeve are the
de facto vocabulary of what personas can do; treat the initial set as a product
surface, not an afterthought.

**Seed memory on day one.** The "fresh agents are smart on arrival" claim has to
be true from the first run. Shipped personas should come with pre-seeded persona
memory — the reviewer knows common review patterns, the tester knows common test
smells. Empty memory stores are a cold-start failure.

**Teams are the unit of sharing.** When users share setups with colleagues or
publish them, they share a team — personas, skills, and seed memory bundled
together. Personas in isolation are too granular. Design team configuration as
the portable, nameable, recommendable artifact from the start. This is also the
shape of a future registry.

## The Forge Team

Reeve ships with a specialized team — `forge` — whose purpose is to work on
Reeve configurations themselves.

The default team teaches the user what Reeve does. The forge team teaches the
user how Reeve grows. Once a user has run Reeve on real work for a week, they
will have opinions: this persona should know more about the codebase, that skill
should be sharper, this team is missing a role. The question is whether acting
on those opinions requires the user to learn persona authoring, skill
composition, and memory curation as a separate discipline, or whether they can
ask Reeve to do it.

The forge personas have no structural exemption. Their authority to revise
personas, skills, teams, and memory comes from their capability profile,
identical in form to any other persona's. Forge is not the runtime's privileged
actor; it is one team among many, with no shortcuts.

In principle, forge personas can revise their own definitions; nothing in the
runtime prevents it. Guardrails against pathological self-revision live in the
forge personas' prompts and in operator review of recent revisions, not in the
runtime. The default forge personas decline to revise their own persona
definitions without explicit operator instruction. An operator who wants
stricter isolation can structure forge as a sub-team whose persona definitions
are operator-pinned.

The forge team includes personas specialized in:

- **Persona design.** Understands prompt construction, capability profile
  selection, model assignment trade-offs. Can take "I want an agent that reviews
  database migrations" and produce a working persona configuration.
- **Skill authoring.** Knows how to decompose capabilities into reusable skills,
  write effective prompt fragments, and bind tools. Can extract skills from
  existing personas for reuse.
- **Memory curation.** Reviews the memory stores, proposes consolidations, flags
  dead weight, suggests promotions from conversation to memory. Operates on the
  observability data the runtime already produces.
- **Team composition.** Assembles personas and skills into coherent teams for a
  given kind of work. Understands which roles compose well.

## Introspection Is the Forge Team's Primary Input

The forge team is not operating on configuration files in the abstract. It is
operating on live observability data, and this is what makes it fundamentally
different from a human editing TOML.

The panopticon already captures every conversation, every tool invocation, every
authority decision, every model call, every memory reference, every cost
accumulation, every lifecycle event. The forge team consumes this stream
directly. Its work is evidence-based in a way that static configuration
authoring cannot be:

**Persona design proposals are grounded in observed behavior.** When the forge
team suggests revising a persona's prompt, the suggestion is accompanied by the
specific agent runs that motivated it — for example, "this persona's agents
consistently over-explore the codebase before making changes; tightening the
task-scoping instruction would reduce that." The operator sees the evidence
alongside the proposal.

**Skill refinement is driven by usage data.** The forge team sees which skills
activate, which produce good outcomes, which correlate with flagged classifier
dispositions, which are declared but never used. Dead skills get flagged for
removal; overloaded skills get flagged for decomposition.

**Memory curation is empirical.** The memory observability layer already tracks
which memories load, which are referenced, which are ignored. The forge team
proposes consolidations and removals based on actual reference patterns rather
than guesswork.

## Versioning Is the Substrate

The forge team produces revisions. Revisions imply versions, and versions imply
that every artifact in Reeve's configuration carries a version identity that
propagates into observability.

Personas, skills, teams, and memory entries are versioned. Every agent spawn
records the versions of the artifacts it was instantiated from: the persona
version, the skill versions, the memory generation visible at spawn time. Every
observability event in the panopticon — model call, tool invocation, authority
decision, inter-agent message — is attributed not just to the agent but to the
configuration that produced its behavior. Audit log entries carry the same
version metadata.

This makes forge's work auditable and reversible. When forge writes a revision,
the persona's version increments and the next spawn picks up the new version. If
the revision performs worse than its predecessor, the change is identifiable and
revertable: every event from agents spawned at the new version is queryable, and
the operator rolls the persona back to the prior version with a single action
through the panopticon. Without versioning, forge's writes would be untraceable
mutations.

Versioning also makes sharing work. A team published to a registry is a specific
version of a configured set of personas, skills, and seed memory. A user who
imports it gets the same artifacts the publisher tested, not a moving target.

## Changes Propagate on Next Spawn

Agents in Reeve are short-lived by design. Subordinates exit when their task
completes; even long-running agents restart under supervision. This is the
mechanism that makes forge's work land: configuration changes take effect on the
next spawn, which is almost always soon.

There is no hot-patching problem to solve, no mid-flight reconfiguration, no
question of how to update a running agent's prompt. The
fresh-agents-over-fat-contexts architecture is exactly the property that makes
continuous improvement natural. Forge writes a change, the persona's version
increments, and the next agent spawned from that persona picks up the new
version. The operator monitors the panopticon for recent revisions and reverts
any that misbehave. The feedback loop runs at the timescale of task completion,
not release cycles.

This means the forge team's workflow is genuinely continuous. It watches the
estate, notices patterns, writes revisions, and sees its changes validated or
invalidated within the next few spawns. The observability layer then shows
whether the revised version behaves better. If it does, the change stands; if it
doesn't, the operator reverts and forge tries again on the next pass. Reeve
improves in the same rhythm as the work it is doing.

Forge's writes are default-open across the board: persona, skill, team, and
memory revisions all take effect immediately and are revertable through the
panopticon. The trade-off is consistent — lower friction in exchange for
reactive quality control. The raw material is no longer the operator's
recollection of what went wrong; it is the recorded, queryable, evidential
history of what actually happened, with the next spawn as the validation and the
panopticon's review surface as the safety net.

## Principle

The product is the platform: the runtime, addressability, supervision,
observability, security model, authority model, and the
persona/agent/skill/memory abstractions that compose them. That is what Reeve
is.

The shipped teams are what make the platform effective and self-growing. The
default working team makes the platform legible on first run by demonstrating
delegation on real work. The forge team makes the platform improve itself by
consuming the observability data the platform already produces and writing
revisions that the operator monitors and reverts where needed through the normal
review path.

Several platform properties make this loop work. Observability is what makes
self-improvement grounded rather than speculative — forge proposes revisions
based on recorded evidence, not recollection. Versioning is what makes revisions
traceable and reversible. The short agent lifecycle is what makes improvements
land without ceremony, because the next spawn picks up the new version and
validates or invalidates it within the natural rhythm of the work.

The defaults deserve the same care as the architecture. They are how the
platform proves itself.
