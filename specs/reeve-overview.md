# Reeve — Overview

## Thesis

Reeve turns AI coding agents from terminal sessions into addressable, supervised
local workers.

## Why This Matters

As agentic development moves from one task at a time to multiple concurrent
lines of work, the terminal session stops being the right unit of control. The
operator needs to see the estate, not just the current conversation. Work needs
stable addresses, durable state, cost visibility, and scoped authority. Reeve
makes those properties primitive.

## The Problem

Today's coding agents — Claude Code, Codex, OpenCode — are interactive sessions.
You can talk to them, but nothing else can. There is no mechanism for another
process, a script, or a peer agent to dispatch work to a running instance and
get results back. They encode assumptions about which models are first-class,
making anything outside that set a second-class citizen that breaks
unpredictably. They are opaque: no cost visibility, no way to inspect a running
agent from outside, no way to reach in without being the one who started the
session.

Most tools have optimized for interactive chat sessions rather than addressable,
supervised runtimes. Reeve is the latter.

## The Product

Reeve is a coding tool. It runs AI agents on your behalf, keeps them running,
and gives you a control surface to see what they are doing and intervene at any
moment. Which model backs each agent is an implementation detail. The user
experience is: start work, watch it happen, reach in when needed.

The name comes from the medieval reeve — an overseer of a working estate who
managed ongoing operations, kept things running, and had the authority to
intervene. That is the tool.

Underneath, Reeve is a generic actor runtime. The first teams that ship with it
are coding agents because coding is the highest-leverage use; nothing about the
runtime is intrinsic to coding work.

## A Day With Reeve

You start Reeve at the top of your repository. The TUI opens a chat with the
lead. You ask it to refactor a service module: a short description, a couple of
pointers to existing patterns in the codebase, the desired interface.

Behind the lead is a configured team. The lead persona runs on Claude. The
reviewers and the tester run on DeepSeek, because DeepSeek is fast and cheap and
the work is bounded. You did not have to fight the tool to get that mix; each
persona declared its own model.

The lead spawns subordinates. Each one is a named, addressable agent, not a
hidden sub-conversation. You switch to the panopticon. Three agents are working
in parallel. Their threads are visible, with attribution on every message. You
watch the lead ask the tester a clarifying question and watch the tester reply.

You close the laptop.

When you come back, `reeve attach` reconnects to the lead's thread where you
left it. The runtime kept running; the work continued. One agent has finished
and exited. Another is mid-flight. A third row is highlighted: the tester tried
to install a new dev dependency, and the classifier flagged it — not blocked,
but surfaced. You look at the action, the reasoning, the dependency. You approve
it. The flag is recorded; the next time the same persona attempts the same kind
of call, you will see the prior decision in context.

Later, you notice the reviewer is producing comments that lean too procedural
for this codebase. You add a short note to the reviewer persona's memory and
save it. The next reviewer agent that spawns picks it up. The running reviewer
keeps its current memory generation; you do not interrupt it mid-task.

At the end of the day, you have a stack of reviewable diffs. You read, edit, and
merge what you want. Nothing was committed without you. You shut down the TUI;
the runtime stays alive.

## Principles

The decisions throughout this document follow from a small set of stances. They
are listed once here so later sections can refer to them by name.

**Addressability is primitive.** Every running agent has a stable name, a stable
filesystem address, and a unified conversation thread. Anything else — peer
dispatch, external tooling, durable cost accounting, post-hoc audit — is built
on that.

**Supervised agency, not autonomous agency.** Agents are delegated work; they do
not own the authority to do it. Operators retain inspection, override, and
revert at every layer.

**Layered defense, not classifier perfection.** Irreversible actions sit on a
deterministic blacklist where no judgment layer is consulted. Routine actions
pass without prompts. The classifier is the judgment layer for what remains.
There is no global override.

**If it happens in the runtime, you can see it.** Every model call, tool
invocation, message, authority decision, and memory reference is recorded and
inspectable. Observability is the precondition for the authority model, not an
add-on.

**Curators, not turnover.** Reeve scales context with curation, not agent
turnover. Each agent has a curator that maintains a working context, evicts
items that have served their purpose, and decides when to invoke cognition.
Long-lived agents stay tractable because the curator continuously edits.

**Memory is curated, not raw history.** What agents start with is the distilled
signal — facts, heuristics, preferences — versioned and revertable, never a dump
of past conversations.

**Model selection is per-agent.** No model is privileged at the interface
boundary. Adapters are per (provider, model) pair because nominal compatibility
does not deliver real interchangeability.

**Agent-friendly tooling.** Reeve is built with tools that agents work well in:
a strict, fast compiler closes the round-trip loop quickly. The runtime is meant
to extend itself.

## Core Concepts

**Runtime.** A long-lived background process that manages the agent supervision
tree. It starts once and persists. The TUI connects to it; closing the TUI does
not stop the runtime.

**Persona.** A defined role and a live actor. As a role, the persona declares
system prompt, default model assignment, capability profile, and skill set. As
an actor, the persona has its own address (`persona:<name>`) and accepts
proposals from running agents — memory promotion, tuning promotion, skill
refinement. Direct writes from agents are rejected; proposals flow through
_persona-curator agents_ that evaluate at forge tier. The same persona can have
many running instances simultaneously, each materializing from the persona's
current state at spawn. Personas are versioned on disk; rollback is repointing
the `current` file. See `reeve-persona-actor.md`.

**Skill.** A discrete capability a persona can apply: review a PR, write tests,
refactor a module, investigate a flaky test. Skills are composable units — name,
description, prompt fragment, optional tool bindings. A persona declares which
skills it has; an agent invocation applies one or more skills to the current
task. Skills are versioned and shareable across personas and across teams.

**Persona vs skill — why both.** Personas are roles; skills are vocabulary. A
persona owns capability profile and model resolution — the things that determine
what an agent is allowed to do and what model resolves it. A skill owns prompt
fragments and tool bindings — the things that change how an agent expresses a
capability it already has. Skills cannot grant authority; personas declare it. A
persona without skills is still a fully addressable, fully authorized role;
skills sharpen behavior but are not required for an agent to function.

**Memory.** Tiered, curated knowledge. Within an agent: _working context_ (what
cognition currently sees), _short-term_ (recently evicted from working context,
hot, time-bounded), _long-term_ (promoted, durable for the agent's lifetime).
Across agents: _persona memory_ (heuristics that travel with a persona) and
_project memory_ (facts about a specific codebase, committable markdown in
`<repo>/.reeve/`). The memory composer (a satellite) queries all four stores
when query state changes materially, scores candidates, and offers them to the
curator; the curator integrates. Promotion is governed by integration / exposure
/ reference signals, not raw retrieval. Entries are versioned and revertable.
See `reeve-memory-composer.md`.

**Agent.** A running instance of a persona. Has its own name, address, keypair,
durable bus tape, current task scope, and cost meter. An agent records the
versions of the persona, skills, and memory generation it was instantiated from.
Inside the agent live three tiers: a _brainstem_ (mandatory infrastructure: cost
meter, status writer, bus tape writer), a _curator_ (the locus of agency that
maintains a working context and decides when to invoke cognition), and
_cognition_ (a stateless function the curator invokes when policy says
deliberation is warranted). Contributing _satellites_ (the memory composer is
the first) run alongside the curator. The LLM is not the agent; the curator is.
Cognition is interchangeable. One persona can have many running agents in
parallel — each addressing a different task, each independently inspectable.
Agents are supervised: a panicking actor is restarted without quiescing the
runtime. See `reeve-actor-interior.md`.

**Lead.** The persona the chat interface attaches to by default — the operator's
first point of contact. Any persona can be designated the lead; team
configuration determines which one fills the role. Its agents have the same
address structure, inbox, and status file as any other persona's agents.

**Team.** A configured set of persona instantiations — for example, "1 lead, 3
reviewers, 2 testers." Starting a team spawns running agents accordingly. Teams
are supervisable as a unit, versioned, and defined in a configuration file.
Teams are also the unit of sharing: a published team bundles personas, skills,
and seed memory at specific versions.

**TUI.** The primary user interface. It is a client of the runtime, not the
runtime itself. Closing it does not stop work. Multiple TUI instances can attach
to the same runtime simultaneously.

**Panopticon.** The second screen of the TUI. A live dashboard of the full agent
tree: status, elapsed time, cost, and recent activity for every running agent.
The operator's view of the estate.

## Primary Workflow

Reeve launches into a chat interface with the lead. You give it work; it runs.
You can close the terminal. When you return, `reeve attach` reconnects to the
live session. The work continued.

From the chat interface you can switch to the panopticon at any time to see the
full picture, focus a subordinate agent, inspect its conversation history, or
send it a message directly. You then switch back.

From outside the TUI — a shell script, another tool, a peer agent — you address
any running agent by name:

```
reeve                          # open the TUI, attach to or start the runtime
reeve attach <agent>           # attach chat interface to a specific agent
reeve send <agent> "..."       # convenience wrapper: builds a signed message and performs the tmp/new rename
reeve status                   # print runtime state to stdout
reeve team start <name>        # start a named team of agents
```

## Addressability

Every running agent has a stable filesystem address under the runtime data
directory.

```
~/.local/share/reeve/          # Linux (XDG); ~/Library/Application Support/reeve/ on macOS
  personas/
    <name>/
      config.toml    # Persona definition: prompt, capabilities, skills, model
      memory/        # Persona-level memory (cross-project)
  skills/
    <name>/          # Skill bundle: prompt fragment, tool bindings, metadata
  operators/
    <name>/
      memory/        # Operator-level preferences and context
  teams/
    <name>.toml      # Team membership and shared configuration
  agents/
    <name>/          # Running agent instance
      inbox/
        tmp/         # sender staging area
        new/         # signed messages awaiting runtime pickup
        cur/         # verified messages durably delivered to agent context
        quarantine/  # failed verification or trust-tier block
      status         # Plain file updated by runtime (idle/working/error)
      log/           # Rolling log directory
      config.toml    # Agent runtime configuration (persona, task scope)

<repo>/.reeve/        # Project memory lives in the repository, not the global runtime
  memory/             # Project conventions, decisions, recurring patterns; intended to be committed
```

Sending a message requires no Reeve-specific client library. A sender writes a
signed message file into `inbox/tmp/`, then renames it atomically into
`inbox/new/`. The runtime picks it up, verifies it, and delivers it to the
agent. Shell scripts, internal tooling, and peer agents all use the same
mechanism.

This Maildir-style delivery model gives Reeve atomic writes without
coordination, durable at-least-once delivery semantics across runtime crashes,
and a transport surface that any local process can speak to without linking
against anything. The agent never reads the inbox directory directly — the
runtime is the only path into agent context.

Each agent has one conversation thread. Messages into that thread can come from
multiple sources — human operators, peer agents, automated tooling — and each
message carries its sender as part of the envelope. The thread is unified and
chronological; provenance is a property of every message. When you attach to an
agent you see the full thread with attribution intact.

## Authority Model

Reeve is built around supervised agency. Agents may be delegated work, but
authority is structured in three layers: a coarse capability profile that
defines what categories of action a persona can attempt at all, a deterministic
blacklist of actions that are never permitted, and a runtime classifier that
judges everything in between.

**Capability profile.** Each persona has a category-level on/off filter for the
kinds of actions its agents can attempt: read files, write files, execute shell
commands, modify git state, create commits, spawn agents, message peers, make
network calls. The profile also carries quantitative thresholds — most
importantly a cost ceiling per agent and per session, beyond which model calls
are refused. The capability profile is configuration. An action whose category
is not enabled, or which would exceed a configured threshold, is refused without
further evaluation.

**Blacklist.** Within enabled categories, a deterministic blacklist enumerates
actions that are never permitted regardless of context. Irreversible operations
live here by default: force-push to shared branches, history rewrites, deletion
outside the working tree, writes outside the repository root, network egress to
non-allowlisted hosts. The blacklist is short, conservative, and configurable
per persona and per team. Organizations can extend it with policy-specific
rules. Blacklist hits are hard refusals, not approval prompts. There is no
global override.

**Classifier.** Everything not blacklisted goes through a runtime classifier
when an agent attempts it. The classifier looks at the action, the agent's task
scope, recent context, and recent activity, and renders a disposition: pass,
flag, or block. Pass means proceed silently. Flag means proceed but log
prominently and surface in the panopticon. Block refuses this specific instance;
the operator can review and explicitly approve if they choose.

The classifier is the layer with real trade-offs. Every action it inspects has a
cost — model call latency and token cost if the classifier is itself a model, or
a smaller cost if it is a local heuristic or fine-tuned small model. False
passes on destructive actions are the failure mode that matters most. Reeve's
answer is not classifier perfection but layered defense: irreversible actions
sit on the blacklist where the classifier is not consulted, the operator sees
flagged and blocked decisions in the panopticon, and the audit log makes every
disposition reviewable after the fact. The classifier is a judgment layer, not a
guarantee.

This inverts the prevailing agent permission model. Most actions are not
blacklisted, and the classifier passes most non-suspicious ones, so routine work
proceeds without prompts. The bypass-everything failure mode common to
allowlist-with-prompts systems does not exist because there is no global
approval toggle to disable. Dangerous actions are refused outright; ambiguous
actions are judged; routine actions just happen.

The authority decision composes with the security layers. A privileged action
requires that the message arrived from a verified sender (transport security),
that the content driving the action was not classified as adversarial
(gatekeeper), that the action category is within the persona's capability
profile, that the specific action is not blacklisted, and that the runtime
classifier passes or flags it. Five gates, each handling a different kind of
risk.

## Security

Reeve has two complementary security layers, each documented separately.

**Transport security** authenticates who sent a message. Senders sign messages
with a private key; the runtime verifies signatures against a registry of known
public keys before delivery. Trust tier — operator, agent, external, untrusted —
is determined by verified sender identity, not by anything the message claims
about itself. Untrusted messages are quarantined; the agent never sees them. See
the _Transport Security Model_ document for full details.

**Content security** addresses prompt injection — adversarial content embedded
in legitimate inputs that attempts to hijack agent behavior. A small local
classifier inspects content at context-promotion boundaries and produces a
structured risk signal. The runtime, not the classifier, enforces delivery:
pass, flag, or block. The classifier is a sensor; the runtime is the boundary.
See the _Content Security: The Gatekeeper Model_ document for full details.

The two layers are non-overlapping. Transport handles provenance; the gatekeeper
handles content. Authentication does not grant content authority. Both compose
with the authority model above into Reeve's defense-in-depth: every gate handles
a different kind of risk, and a privileged action must clear all of them.

## Observability

Reeve's principle: if it happens in the runtime, you can see it. Every action an
agent takes is recorded and inspectable — not just conversations and tool calls,
but every layer of the system. Existing coding agents are opaque sessions whose
activity disappears when the session ends. Reeve is transparent: nothing the
agents do is hidden, and the operator's authority to see is total.

The panopticon is the live operator view. From it you can inspect:

- **Conversations.** Every agent's full thread with attribution on every
  message: operator, peer agent, external sender, system.
- **Inter-agent messages.** Who sent what to whom, with the full envelope and
  trust tier visible. The message graph across the agent tree.
- **Model API calls.** Every request to a model provider — prompt, response,
  token counts, latency, cost — per call, per agent.
- **Tool invocations.** Every shell command, file read, file write, git
  operation, and external API call, with arguments and output.
- **Authority decisions.** Every blacklist refusal, classifier disposition, and
  operator approval, with the reasoning and the action.
- **Memory references.** Which memories were loaded into which agent at spawn,
  when an agent's reasoning draws on a specific memory entry, and how often each
  memory is referenced over time. Memory observability surfaces dead weight —
  entries that load but are never used — and high-value entries that pay for
  themselves repeatedly.
- **Cost.** Token consumption and estimated cost per agent, per persona, per
  team, and for the runtime session as a whole, updated in real time as model
  calls complete.
- **Lifecycle events.** Agent spawn, restart, supervised failure, exit. Persona
  instantiation. Skill activation.

Every event in this list carries version attribution: the persona version, skill
versions, and memory generation in effect on the agent that produced the event.
An action is not just "agent X did this" — it is "agent X, instantiated from
persona Y at version N with skills A:v1 and B:v3, did this." This makes
regressions traceable to specific configuration changes and supports rollback
when a new version performs worse than its predecessor.

Every event in this list is durable. The panopticon shows the live view; the
underlying log is queryable, exportable, and survives runtime restart. An
agent's full activity history is reconstructable after the fact for post-mortem,
audit, or operator review.

This level of observability is the operator's leverage. It is also a
precondition for the authority model: classifier dispositions, capability
decisions, and security trust assignments are only credible if they are
inspectable.

## Context and Memory

Context windows are finite. Real coding work is not. Reeve's answer is curation,
not turnover.

Each agent has a curator (see `reeve-actor-interior.md` for the full model) that
maintains a working context, integrates inputs, evicts items that have served
their purpose, and decides when to invoke cognition. The curator is mechanical
for routine bookkeeping; a small-model fallback handles the residual cases. The
result: a long-lived agent whose apparent memory span is its full lifetime,
whose per-invocation input cost is bounded by the curated snapshot rather than
session history.

Memory has tiers within an agent — working context (what cognition currently
sees), short-term (recently evicted, hot), long-term (promoted, durable) — plus
persona and project memory above. The memory composer (a satellite) queries the
four stores when query state changes materially, scores candidates, and offers
them to the curator. The curator integrates; the composer never does.

Compression is continuous and structural on the curator's hot path: when a tool
call resolves an open question, the question and the discussion that produced it
collapse into a structured pointer item. Where natural-language synthesis is
genuinely required, it runs as a background consolidation pass through the
small-model fallback, not on the hot path.

Persona memory is curated by _persona-curator agents_ — agents whose job is to
evaluate proposals from running instances at forge tier. Reeve's default for
memory writes is permissive with reactive quality control: agents propose;
persona-curator agents evaluate and write; the panopticon surfaces recent writes
for operator review; any write can be reverted to a prior version. Bad writes
can pollute future spawns until the operator reverts them; the supervised
observability plus versioned rollback are what make that trade-off acceptable.

A fresh-spawned agent is not starting from zero. The persona's current state
materializes into the new agent at spawn — defaults, memory selection rules,
skill set — so the curator begins with an informed working context rather than
an empty one.

## Shipped Defaults

Reeve's value is emergent from multiplicity. A single agent is a slightly-better
Claude Code; the panopticon, addressability model, and delegation story are only
legible at N greater than one. Reeve therefore ships with two configured teams
out of the box: a default working team that demonstrates delegation on real
tasks, and a `forge` team specialized in improving Reeve's own configuration
based on observability data. The default team is what makes the tool legible on
first run; the forge team is what makes it self-extending. See the _Shipped
Teams and Self-Improvement_ document for full details.

## Architecture

The runtime is written in Rust. The choice is functional, not aesthetic: a
long-lived supervision tree benefits from predictable memory and no GC pauses,
single-binary distribution simplifies installation, and — pointedly — coding
agents tend to produce correct Rust on the first try, with a strict, fast
compiler that closes the round-trip loop quickly when they do not. Reeve is
meant to extend itself; the runtime is written accordingly.

Internally, each agent is an actor — a lightweight, in-process unit with its own
mailbox — managed under a supervision tree. The actor model provides location
transparency (senders do not know or care what model backs an agent), fault
isolation (a panicking actor is restarted by its supervisor without quiescing
the runtime), and composability (any actor can dispatch work to any other actor
by name).

Model integration goes through an adapter layer keyed on (provider, model)
pairs. Each adapter translates Reeve's internal protocol to the actual API of
one specific model. This is necessary because nominal compatibility — including
OpenAI-compatible endpoints and routing services — does not deliver real
interchangeability: tool calling formats, system prompt handling, streaming
shapes, reasoning content, vision payloads, and cost reporting all differ in
ways that matter. DeepSeek, Claude, GPT, Gemini, and OpenRouter targets all get
adapters; none is privileged at the interface boundary. Personas declare
required capabilities; the runtime refuses to instantiate a persona on a model
whose adapter does not support what the persona needs.

## Prior Art

The service directory and fifodir patterns from **s6** and **daemontools**
inform the filesystem layout and supervision design. Each agent directory
follows the same structure as an s6 service directory: configuration, state, and
event channels are files in a known location, and the supervisor watches the
directory rather than a PID.

**Maildir** semantics inform the durable event log design: entries are written
atomically, without coordination between writer and reader, using filesystem
primitives that have been proven reliable for decades.

The **Erlang/OTP** supervision tree model — let-it-crash, supervised restarts,
location-transparent messaging — is the direct inspiration for the actor layer.

## Coding Agent Behavior

Reeve is not replacing basic coding-agent behavior; it is changing the runtime
model around it. Agents can read and write files, execute shell commands within
their capability profile, run tests, and search the codebase. They understand
the repository context: current branch, staged changes, commit history, and
Jujutsu stack structure where applicable. Long-running agents maintain state;
short-lived agents exit cleanly; context management is explicit per agent. Model
output streams to the TUI and to any subscriber in real time. Model selection is
per-agent, not global. Agents produce reviewable diffs; human review before
commit is first-class, not an afterthought.

## Non-Goals

The list follows from Reeve's scope as an actor runtime that currently hosts
coding agents.

- **Not itself a coding agent.** Reeve runs them. The agent role is
  configuration; the runtime is generic.
- **Not an orchestration framework with a DSL or graph model.** Coordination is
  supervisor-and-mailbox, not declared workflows.
- **Not a model router or gateway.** Adapters exist to make supervision work
  across heterogeneous models, not to traffic-shape or cost-optimize across
  providers. Reeve may grow that capability, but not now.
- **Not multi-machine.** Reeve is a local workstation tool. The runtime, agent
  tree, and audit log live on one machine.
- **Not a CI system.** Reeve produces code; what it produces could become a CI
  system. It is not one itself.
- **Not a memory product.** Reeve contains a memory subsystem because agents
  need one. The store is a means, not the product.
- **Not a cloud-managed service.** Reeve does not require a server or network
  connectivity beyond the model API calls agents make.

## Further Reading

This overview is the front door. Eleven sibling documents carry the depth.

- **Positioning** — strategic context: who Reeve is for, where it sits relative
  to coding agents and orchestration tools, what it owns and what it does not,
  why governability is the wedge.
- **Domain Model and Code-Level Architecture** — the formal definitions:
  identifiers, configuration layer, runtime layer, security layer, state
  ownership, subsystem boundaries, and invariants. Read this next if you are
  implementing or reasoning about Reeve internals.
- **Actor Interior** — what is inside an agent: the brainstem (mandatory
  infrastructure), the curator (locus of agency), cognition as a stateless
  function, contributing satellites, the working context, the bus tape, the
  dispatcher with subsystem addressing, and the three model resources.
- **Persona as Live Actor** — the persona has its own address and dispatcher.
  Downward materialization at spawn, upward proposals from agents,
  persona-curator agents that evaluate at forge tier, and the three operations
  (instance tuning, default promotion, propagation).
- **Memory Composer** — the first contributing satellite. Mechanical retrieval
  pipeline (lexical plus embedding) across short-term, long-term, persona, and
  project stores; candidate lifecycle and usage counters; suppression windows.
- **Versioned Disk Substrate** — versioning as a property of the disk format:
  numbered version files, `current` pointer, manifest. Rollback as repointing.
  Disk + bus tape + panopticon as the three sources of truth, no metrics layer.
- **Transport Security Model** — message provenance, signed envelopes, the
  maildir state machine, replay and delivery ledgers, identity and key model,
  trust tiers.
- **Content Security: The Gatekeeper Model** — the pre-delivery classifier for
  prompt injection, task scope as jurisdiction, the runtime disposition policy,
  and the separation between sensor and boundary.
- **Shipped Teams and Self-Improvement** — the default working team and the
  forge team that improves Reeve based on observability data.
- **TUI Design** — design principles, information architecture, the five access
  modes, cross-screen conventions (sigils, color, keybindings), boundaries, open
  questions.
- **TUI Screens** — wireframes and per-screen notes for the first ship: lead
  chat, panopticon home, per-agent inspect, memory review, configuration
  revision review, quarantine. Includes the seven userflows that exercise these
  surfaces.

A reasonable order: Positioning to understand what Reeve is for and what it is
not; then Domain Model since it grounds the vocabulary the others use; then
Actor Interior, Persona as Live Actor, Memory Composer, and Versioned Disk
Substrate together for the architecture inside an actor and across the actor
fleet; then Transport Security and Gatekeeper in either order; then Shipped
Teams; then TUI Design and Screens together when you are ready to think about
the operator surface.
