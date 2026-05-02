# Reeve — Positioning

## Core Thesis

Reeve is a local actor runtime for AI coding agents.

It is not an agent. It is not an autonomous project manager. It is not an agent
team product. It is not a marketplace.

Reeve is the substrate that makes agent teams governable.

The central claim: once AI coding agents have real authority, they need
identity, supervision, scoped permissions, revocation, auditability, and
operator control.

Modern coding agents read files, write files, run commands, create commits, open
pull requests, consume tool output, and increasingly communicate with other
agents or automation processes. They are still described as assistants, but
operationally they behave like local actors with delegated authority.

That changes the engineering problem.

Prompt discipline is not enough. Good intentions are not enough. A chat
interface is not enough. A loose collection of shell scripts is not enough.

If agents are going to act inside real development environments, the local
runtime needs to make their authority explicit, inspectable, enforceable, and
revocable.

That is Reeve's purpose.

## Market Context

The agentic coding market is filling quickly.

Model-backed coding agents — Claude Code, Codex, Gemini CLI, Goose, OpenCode,
and others — execute work. They are increasingly capable and increasingly
interchangeable at the execution layer.

Emerging orchestration products — Gas Town-style systems and similar —
coordinate multiple agents, assign work, manage handoffs, track state, and
present higher-level project workflows.

Both categories are real and useful. Neither, by itself, provides a durable
governance substrate.

A coding agent can execute work. An orchestration product can coordinate work.
Neither gives those activities identity, audit, scoped authority, revocation, or
operator supervision.

The missing layer is the local control plane for agents with authority. That is
the layer Reeve targets.

## Positioning

Reeve is a local supervision and authority runtime for AI coding agents. It
gives agents stable identities, authenticated message transport, durable
mailboxes, scoped authority, inspectable state, revocation, and operator
supervision.

Reeve is not the team. Reeve is the runtime that governs the team.

Tools like Gas Town can be built on top of Reeve. Reeve does not replace them;
it supplies the actor runtime they need once agent coordination moves from demo
to daily engineering practice.

```
Claude Code, Codex, Goose, OpenCode execute work.
Gas Town-style tools coordinate work.
Reeve governs actors.
```

## Why This Distinction Matters

The obvious trap is to build another multi-agent coding orchestration tool. That
lane is crowded and will be shaped by distribution, UX, integration with
existing agent tools, and whatever the major agent vendors decide to absorb.

Reeve's stronger position is underneath that layer.

The durable problem is not "how do I get five agents to work on a task?" The
durable problem is:

```
Who is this actor?
Who authorized it?
What is it allowed to do?
Who sent this instruction?
Was the sender verified?
What untrusted content entered context?
What files did it touch?
What commands did it run?
What messages led to the action?
Can I stop it?
Can I revoke it?
Can I reconstruct what happened?
Can I safely reuse this team definition later?
```

Those questions get more important as agents get better.

When agents are weak, governance feels like overhead. When agents are strong,
governance becomes the difference between leverage and chaos. Reeve exists for
the second world.

## Conceptual Model

Three terms, deliberately distinct.

**Agent.** A model-backed execution engine with tools. Examples: Claude Code,
Codex, Goose, OpenCode, Gemini CLI. An agent can reason and act, but by itself
it is not necessarily a governed runtime entity.

**Actor.** A named, addressable, supervised runtime entity managed by Reeve. An
actor has stable identity, runtime address, inbox and outbox, message history,
declared authority, adapter binding, lifecycle state, audit trail, and
revocation semantics. Actors are the unit Reeve governs. An actor is bound to an
agent through an adapter: the actor is what exists in Reeve; the agent is what
executes.

**Team.** A declared topology of actors, roles, routes, permissions, prompts,
tools, and gates. A team is not merely a collection of agents. It is a runtime
shape: which actors exist, what each is responsible for, what authority each
has, how they communicate, what artifacts they produce, what gates must be
passed, where human review is required.

A _team package_ is a signed, versioned, reviewable team definition. The
marketplace unit, if one ever exists, should not be an opaque autonomous
software company in a box. It should be a recipe for actors, authority, routing,
prompts, tools, and gates.

Note on vocabulary: the implementation specs (`reeve-domain-model.md` and below)
sometimes use "agent" as shorthand for the supervised runtime entity that this
document calls an "actor." The strategic distinction is sharp. The
implementation collapses it.

## Layer Model

```
Models / Agent CLIs
  Claude Code, Codex, Goose, OpenCode, Gemini CLI, etc.

Agent Adapters
  Translation layer between Reeve and specific agent execution engines.

Reeve Core
  Stable actor identity, authenticated transport, scoped authority,
  durable mailboxes, supervision, audit, revocation, content security.

Team Definition Layer
  Declarative actor topologies: roles, prompts, tools, permissions,
  message routes, gates, expected artifacts, lifecycle rules.

Team Execution Layer
  Instantiates a team definition against a repo, workspace, task,
  or project.

Team Products
  Gas Town-like coordination systems, project workflows, review tools,
  planning tools, custom engineering workflows.

Marketplace
  Discovery, versioning, review, installation, trust, update, and
  distribution of signed team definitions.
```

This model keeps Reeve from absorbing every adjacent concern. Reeve should make
higher-level tools possible; it should not become all of them.

## Scope Boundary

**Reeve owns the runtime and governance substrate:**

- actor identity, lifecycle, addressing
- authenticated message envelopes, delivery, inbox / outbox semantics
- durable message history
- sender verification, authorization boundaries, revocation before delivery
- gatekeeping of untrusted content
- adapter abstraction over agent CLIs
- runtime supervision, operator intervention
- audit / event log, local state model
- explicit authority grants, permission inspection

**Reeve does not own (early or possibly ever):**

- project planning, autonomous task decomposition
- team topology optimization
- sprint / work tracking
- "mayor" or project-manager agent behavior
- agent performance scoring
- business workflow semantics
- PR stack planning
- marketplace discovery UX, marketplace reputation systems
- product-specific orchestration

Reeve may _enable_ these. It should not absorb them.

The rule:

> Reeve should make agent teams safer and more inspectable, not become the team
> product itself.

## Relationship to Orchestration Tools

Gas Town-style tools coordinate agent teams. They focus on task decomposition,
team formation, work assignment, parallel execution, progress tracking,
handoffs, project-level workflow, user-facing UX. Those are application-level
concerns.

Reeve's focus is lower: stable actor identity, authenticated transport, durable
mailboxes, scoped authority, runtime supervision, revocation, content
gatekeeping, operator visibility, auditability.

A Gas Town-like tool could be built on top of Reeve by treating Reeve actors as
its execution substrate. That is the right relationship. Gas Town is not the
enemy; Gas Town is an example of the kind of thing Reeve should make safer.

## The Strategic Wedge

The wedge is _governability_.

Not autonomy. Not swarms. Not agent magic. Not "software company in a box." Not
another coding assistant.

> Agent teams are only useful in daily engineering practice if their actors are
> identifiable, permissioned, supervised, revocable, inspectable, and auditable.

That is a CTO-shaped problem. It is not a demo problem. It is an operational
problem.

The industry is currently excited about delegation. That excitement is rational;
the tools are becoming powerful. But power without runtime boundaries creates
risk, confusion, and eventually organizational rejection.

Reeve's purpose is to make delegated local agency safe enough to use seriously.

## Marketplace Optionality

A future marketplace is possible but should not drive early implementation.

The marketplace makes sense only if teams are declarative, signed, permissioned,
versioned, and locally supervised. The marketplace should distribute team
definitions, not arbitrary agent code.

A team recipe might include team name, version, role definitions, actor prompts,
adapter requirements, required capabilities, filesystem permissions, shell
command permissions, network / tool permissions, message routes, human review
gates, expected artifacts, lifecycle hooks, compatibility constraints, signature
metadata.

The early design implication is not "build the marketplace." The implication is:
don't make the marketplace impossible later. That means team definitions are
explicit files; teams have stable IDs; actors have stable IDs; team versions are
part of the model; prompts are versioned assets; authority grants are explicit;
permissions are diffable; audit events tie back to actor identity and team
version; local overrides are first-class; packages are hashable or
content-addressable; signatures fit naturally even if initially local-only.

## Trust and Permission Diffs

Trust is central. If team definitions become portable, operators need to
understand what they are accepting.

Updating a team package should produce a permission and behavior diff:

```
rust-feature-team 0.1.0 → 0.2.0

Added:
- implementer can now write migrations/
- reviewer can now run cargo clippy
- planner can message reviewer directly

Changed:
- implementer prompt changed
- commit gate now requires cargo test and cargo clippy

Removed:
- implementer can no longer write Cargo.toml
```

Permission diffing is not a nice-to-have. It is part of the trust model.
Operators should not need to read raw YAML or prompts to understand whether an
update is materially more powerful than the previous version.

## Runtime Enforceability

Declarations are not enough. Reeve must enforce declared authority at runtime.

If a team definition says an actor may only write `plans/`, Reeve prevents that
actor from writing `src/`.

If a team definition says an actor may only run `cargo check` and `cargo test`,
Reeve prevents arbitrary shell execution.

If a message sender is unsigned, revoked, or unauthorized, the actor does not
receive the message in context.

If external content is untrusted, it is delivered as data, not instruction.

This is the difference between governance and theater. Reeve's credibility
depends on enforceability.

## Auditability

For any consequential action, Reeve answers:

```
Who asked for this?
Which actor did it?
Which team definition created that actor?
Which version of the team definition was active?
Which model or adapter executed it?
What authority grant allowed it?
What messages led to it?
What files or commands were involved?
What changed?
What gate allowed it?
Was human review required? Was it performed?
```

This is one of Reeve's strongest differentiators. Agentic engineering will
create more work, faster — that is the point. But faster work without
explainability creates review bottlenecks, trust failures, and operational mess.
Reeve makes the work traceable.

## Final Position

The durable value is not multi-agent orchestration; that layer will be crowded.
The durable value is _governable local agency_.

Reeve is the runtime that supplies it.

```
Agents execute. Teams coordinate. Reeve governs.
```

```
Reeve is not the team. Reeve is the runtime that governs the team.
```

```
Once agents have authority, identity and audit stop being optional.
```

```
Prompt discipline is not a runtime boundary.
```

Everything else is a client.
