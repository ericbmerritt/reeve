# Reeve — The Organization: Agents, Teams, and Engagements

## Context

Planning the effectors ladder surfaced three questions that turned out to be one
question. Where does the working root live, if the daemon is ephemeral and
agents are not? What owns work-specific memory, if "project" is a coding-tool
concept Reeve doesn't actually have? And what is the general shape of
constraints — the blacklist, the capability profile, the egress allowlist, the
file jail — when they need to apply to a group of agents, not just one?

All three are the same missing piece: Reeve had a runtime model and a security
model but no **organizational model**. This document supplies it. It defines
what is durable and what is disposable, what a team is once agents are
long-lived, what an engagement is, where context and constraints attach, and how
knowledge flows between them.

This spec amends `reeve-domain-model.md` (the Team section, the Memory Entry
scopes, the naming rules) and supersedes one decision in `reeve-effectors.md`
(Decision B, the daemon-global working root). The amendments are listed in §
What This Changes. The effectors ladder is this model's first consumer: the
working root the file jail enforces is engagement context under this model.

## The Central Principle: Durability Is Identity-Level

**The runtime is disposable; the organization is durable.** The daemon that
executes agents holds no state worth keeping. Everything that matters — who the
agents are, what teams exist, what work is open, what has been learned — lives
on disk and survives any restart. We call that durable whole the **estate**: the
organization of agents, teams, and engagements that one operator runs.

**Agents are designed to be extremely long-lived.** An agent is a durable
identity: a name, a keypair, a conversation history, an accumulated memory. That
identity persists across restarts of the process that animates it. We call one
continuous run of that process an **incarnation**: the durable agent persists;
incarnations come and go. Configuration and grants are snapshotted per
incarnation (§ Constraints); identity, history, and memory belong to the agent
across all of them.

Ephemeral agents still exist as a _usage pattern_ — a subordinate spawned for
one task, a team formed for one engagement and dissolved after. The pattern is
safe because the memory model makes ephemerality non-lossy: what a short-lived
agent learned survives it through graduation (working context → short-term →
long-term → promotion to persona memory or the engagement file). The design
center is the long-lived agent; the throwaway agent is the special case, not the
other way around. The prior domain-model language ("agents are instances and
their names are reused") described the special case as the rule; this document
corrects it.

**Reeve has no project concept.** The estate _does_ projects; it is not
organized around them. Reeve is a general organization of agents that takes on
work of any kind — some of it repo-bound, much of it not. "Project memory" in
the prior domain model was a coding-tool worldview smuggled into an organization
runtime; this document retires it (§ Memory and Knowledge).

## Narrative

You open an engagement: "modernize the billing reconciler," with its context set
to the reconciler repository. The engagement gets a durable record — a purpose,
a working root at the repo's toplevel, an empty memory file.

You form a team from the shipped `working-team` template. Forming the team mints
its members as new durable agents — a lead, a reviewer, a tester — each with its
own name, keypair, and empty long-term memory. You staff the team to the
engagement. Each member incarnates with a snapshot of its grants and of the
engagement's context; their file effectors are jailed to the reconciler repo
because that is the engagement's working root, not because the daemon was
started there.

The lead decides the schema migration deserves focused attention and opens a
sub-engagement — "migrate the ledger schema" — whose context is the `schema/`
subtree. The sub-engagement's root nests inside the parent's; it could not have
been wider. The lead staffs a spawned subordinate to it — delegated staffing
within the engagement's own tree, no operator round-trip needed.

Work proceeds for weeks. The daemon restarts several times; nobody notices — the
agents' conversations, the engagement, the memory all persist. Midway, you
blacklist a dangerous command estate-wide; the ban lands on every running
incarnation immediately. Separately you tighten the reviewer persona's grants;
that lands at each reviewer's next incarnation.

The sub-engagement closes; its distilled learnings graduate into the parent
engagement's memory. Eventually the engagement itself closes. Its memory file is
archived with it — reopenable if the work resumes. The team is now idle. You
staff it to a new engagement in a different repository: each member
re-incarnates with the new context. Nothing about the daemon changed; the _team_
moved.

Six months later a differently-composed team reopens the reconciler engagement.
They start with everything the estate learned about that work, because the
knowledge lived in the engagement file — not in the heads of the agents who
happened to do it first.

Alongside all of this, a teamless utility agent — the estate's librarian —
answers questions all day. It belongs to no team and serves no engagement; it
has no working root and its file effectors refuse everything. It doesn't need
them.

## The Organization

Three durable organizational things, plus the persona as the template dimension:

### Agent

A durable identity that does work. Name, keypair, conversation history,
short-term and long-term memory. Belongs to **at most one team** (teamless
agents are allowed). Executes as a sequence of incarnations; grants and context
snapshot at incarnation start, identity and memory persist across incarnations.

Lifecycle: minted (at team formation, by spawn, or directly by the operator) →
active across incarnations → retired. A retired agent's record and history are
archived; its name is **not reused** — durable identity means the name belongs
to that agent permanently. The identity ID (already globally unique and never
reused, per the domain model) remains the stable identifier underneath; the name
is the human-facing handle. (This amends the domain model's naming rule that
agent names are reused per runtime instance.)

Incarnation invariants: an agent has **at most one live incarnation** at a time.
Re-incarnation happens on daemon restart, on re-staffing, or on explicit
operator restart — and on nothing else; a persona or grant revision never forces
one, it waits for the next.

### Team

A **standing roster of durable agents**, formed from a team template. The
template — what the prior domain model called a team: (persona, version, count,
role label) with a lead role — remains the shippable, publishable configuration
artifact that `reeve-shipped-teams.md` depends on. Forming a team instantiates
the template: it mints the member agents as new durable identities and binds
them into a named, durable roster.

Template : team :: persona : agent. The template is configuration; the team is a
durable organizational unit.

A team serves **at most one engagement tree at a time**: it is staffed
(allocated) to a top-level engagement, and its members may be sub-staffed to
sub-engagements within that tree (§ Engagement) — that is still service of the
same work, not concurrency. Between engagements the team is idle; it can be
staffed to successive engagements over its lifetime. Concurrent _unrelated_ work
means another team, not a busier one. (Start strict; loosen only if use forces
it.)

Lifecycle: formed (from template; members minted) → cycles of staffed / idle →
dissolved. Dissolution is an operator act with a per-member disposition: each
member is either **retired** with the team or **released** to teamless standing.
Recruiting _existing_ agents into a newly formed team is out of scope for v1 (§
Scope Cuts).

### Engagement

A durable, named piece of work the estate has taken on. The account-file pattern
from human organizations: the work itself has identity, context, and an
accumulating file of knowledge, independent of who is currently doing it.

An engagement carries:

- **Purpose** — what the work is, in prose.
- **Context** — what the work is _on_: a working root when the work is repo- or
  directory-bound, other durable context otherwise, possibly nothing. The
  working root that the file effectors jail to is engagement context — this
  supersedes the effectors spec's daemon-global root. **Context is immutable
  after open.** Reopening restores the same context; work on a different root is
  a different engagement. Without this rule, "edit the engagement's root" would
  be a stealth widening path around the narrowing law.
- **Memory** — the engagement file (§ Memory and Knowledge).
- **Staffing** — at most one staffing unit at a time; serially re-staffable; the
  engagement outlives any staffing. What may serve as the unit depends on the
  engagement's level — see _Staffing authority follows the tree_ below.

Engagements **nest**. The operator opens top-level engagements; an agent staffed
to an engagement may open sub-engagements under it — a capability-gated act,
parallel to spawning subordinates. A sub-engagement's context must sit
**inside** its parent's (a sub-root inside the parent root): the narrowing law
(§ Constraints) applies to context exactly as it applies to authority, which is
what makes delegated engagement-opening safe. Containment against absent context
contains nothing: sub-engagements of a rootless engagement are themselves
rootless.

**Staffing authority follows the tree.** The staffing unit rule is per level:

- **Top-level engagements** are staffed by the operator, with a **team** or a
  **teamless agent** (the degenerate unit of one). A team member is never a
  top-level unit on its own.
- **Sub-engagements** are staffed by the agent that opened them, with agents
  that agent **commands**, under the same `open_engagements` capability that
  authorized the opening.

**Commands** is a defined relation, not a figure of speech: an agent commands
its spawn subtree (the subordinates it spawned, and theirs, transitively); the
team's lead additionally commands the team's members. No other command relation
exists — a non-lead member does not command its peers, and nobody commands
agents outside their own team and spawn subtree.

A member sub-staffed within the tree is still serving its team's engagement; it
is not a second allocation. What an agent must never do is staff across trees,
staff a top-level engagement, or staff agents it does not command — those remain
operator acts.

**Unstaffing cascades.** Sub-staffings derive from the top-level allocation:
when the operator unstaffs the unit from a top-level engagement, every
sub-staffing in that tree is recalled with it. The sub-engagements themselves do
not close — they are durable work records and remain open, awaiting staffing —
but nobody is left serving any part of the tree.

Lifecycle: opened → active (staffed or awaiting staffing) → closed. A closing
engagement's memory is archived with it; a closed engagement can be reopened
with its file intact. A closing _sub_-engagement distills its learnings upward
into its parent's memory before archiving.

### Persona

Unchanged in role: the template dimension. A persona defines what _kind_ of
worker an agent is — system prompt, model requirements, capability profile,
skills, memory subscriptions. Teams answer "whose agent"; personas answer "what
kind." The persona is orthogonal to the organizational chain: it stamps an
agent's starting grants at minting and re-stamps at each incarnation, but it is
not a scope in the constraint chain and owns no organizational state.

### Relationships, stated once

- An agent belongs to at most one team.
- A team serves at most one engagement tree at a time.
- An engagement is served by at most one staffing unit (team or lone agent) at a
  time; members of that unit may be sub-staffed within the engagement's tree.
- Engagements nest; a sub-engagement's context is contained in its parent's;
  engagement context is immutable after open.
- Personas stamp agents; they do not own them.

## Constraints and the Narrowing Law

Reeve already has several constraint mechanisms, designed locally and answering
different questions. Named as kinds:

- **Grants** — the capability profile's categories. _What you may attempt._
- **Floors** — the blacklist. _What no one may do, regardless of grants._
- **Ceilings** — the egress allowlist. _The universe reachable at all._
- **Context** — the working root. _Where you exist._
- **Quantitative** — thresholds and budgets. _How much._

What was missing is the scope axis — and there are **two independent chains**,
because context and authority are conferred by different structures:

```
policy chain:    operator  →  team  →  agent-at-incarnation
context chain:   engagement  →  sub-engagement  →  agent-at-incarnation
```

Grants, floors, ceilings, and budgets are evaluated as the meet of the **policy
chain** — the organizational reporting line. Context (the working root and any
other containable work parameter) is evaluated as the meet of the **context
chain** — the work's nesting line, entered through staffing. Engagements scope
context, which is security-relevant (the file jail enforces it); they do not
carry authority policy — work that needs a tighter envelope of grants or bans
gets it from the team. The persona stamps the agent's starting grants at minting
and each incarnation but is a template input, not a scope in either chain. For a
teamless agent the policy chain is simply operator → agent-at-incarnation.

**The narrowing law: every scope may only narrow what its parent allows, never
widen it.** Effective authority is the meet of the chain. Per kind:

- Grants **intersect** down the chain — a team enables a category for its
  members only if the operator's envelope enables it.
- Floors **union** — every ancestor's bans apply; a team can add bans, never
  remove the operator's.
- Ceilings **intersect** — a team's egress list is a subset of the operator's.
- Context **nests** — an agent's root sits inside its engagement's root; a
  sub-engagement's inside its parent's.
- Budgets **partition** — a team budget is a shared pot drawn down across
  members (the tree-aggregate semantics `cost_per_session` already established),
  not a per-member allowance.

The law is what makes delegation safe with no additional checking: a lead
constraining a subordinate, a team constraining a member, an agent opening a
sub-engagement — in every case widening is unrepresentable, so the act needs no
review to be trusted with the _scope_ of what it creates. The effectors spec
stated one instance of this law ("operators may additionally blacklist narrower
paths inside the root, but they cannot widen the jail"); this is the general
form.

**Write authority flows downward.** Constraints at a scope are written by that
scope's owner and are read-only to the constrained. Nothing an agent can write —
memory included — participates in its own confinement. This is the firm line
between the constraint lane and the memory lane: memory is agent-written
knowledge; constraints are operator- and structure-written policy. The two never
share a store.

Constraints are grounded in durable artifacts, not runtime assertions:
operator-scope policy lives in operator-edited files under the data dir
(`blacklist.toml`, `egress_allowlist.toml`, and their successors, per
`reeve-disk-substrate.md` conventions); a team's constraint envelope is part of
the durable team record, operator-edited; an agent's effective grants and
context are the incarnation snapshot, recorded at incarnation start and visible
in inspect and audit. Agents have no write path to any of these — the same
property the effectors spec establishes for the file jail ("structural, not a
blacklist entry") holds for every scope's constraint store.

**Propagation is split by kind.** Grants and context **snapshot per
incarnation**: a running incarnation is never silently re-granted or re-rooted;
changes land at the next incarnation. Floors and ceilings are **live**: a
blacklist ban or an allowlist removal takes effect on running incarnations
immediately, mid-flight (a ban that waits for a restart is not a ban). This
preserves both properties that matter — behavioral stability for the agent,
immediacy for the deterministic floor — and it re-reads the existing "profile
cannot be widened during the agent's lifetime" rule as: the snapshot boundary is
the **incarnation**, not the (possibly months-long) agent lifetime.

Two consequences worth stating:

- Re-staffing a standing team to a new engagement changes its members' context,
  so **rotation implies re-incarnation** — the members restart with the new
  root. This is also exactly when pending persona and grant updates land, so
  `reeve-shipped-teams.md`'s "changes propagate on next spawn" becomes "on next
  incarnation" and the forge feedback loop survives long-lived agents intact.
- An unstaffed agent (teamless, or on an idle team) has **no working root**; its
  file effectors refuse for want of context. Context flows only from
  engagements. There is no default root, no daemon-cwd fallback — a rootless
  agent that needs to act on files needs an engagement.

## Memory and Knowledge

The model follows how human organizations actually retain knowledge. Durable
organizational knowledge lives in **files**; working knowledge lives in
**heads**. Human orgs have no durable "team memory" — it walks out the door when
the people do, and organizations compensate by writing things down. So does
Reeve.

**The files:**

- **Engagement memory** — the engagement file. Knowledge about _the work_: what
  was tried, what the constraints turned out to be, where the bodies are buried.
  Written by staffed agents, survives staffing changes, archived with the
  engagement, inherited by whoever is staffed next — the rotation test this
  scope exists to pass.
- **The estate library** — operator-scope memory. Estate-wide knowledge that
  belongs to no single engagement or persona. Distillation target for closing
  engagements and dissolving teams when knowledge outgrows its origin. Writes
  are deliberative, mirroring persona memory's model: agents propose, and a
  curator commits at forge tier (or the operator writes directly); direct agent
  writes are rejected. Engagement memory, by contrast, is direct-write for
  staffed agents — the file is the work's own notebook; the library is the
  estate's canon.

**The heads:**

- **Agent short-term / long-term memory** — unchanged from
  `reeve-memory-composer.md` and `reeve-actor-interior.md`. Belongs to the
  durable agent, persists across incarnations.
- **Persona memory** — the profession's trained intuition, shared by every agent
  of the persona. Unchanged, including the deliberative `propose-memory`
  promotion path.

**There is no team memory store.** Not deferred — correctly absent, per the
human model. A team that wants durable knowledge writes it into the engagement
file or proposes it to the estate library.

**Memory is never policy.** Memory content — engagement files included — is
advisory knowledge for cognition. No tool, gate, or scope reads memory to decide
authority; nothing an agent writes into any memory store can widen (or narrow)
what any agent may do. Engagement memory read by later agents is an injection
surface like any other content, and gets the gatekeeper's treatment when that
ladder ships.

**Project memory is retired.** The composer's store list becomes: agent
short-term, agent long-term, persona, **engagement** (replacing project), plus
the estate library. What was valuable about `<repo>/.reeve/memory/` — memory
that travels with the artifact, committable and reviewable like code — is
preserved as a **storage location policy**, not a scope: an engagement whose
context is a repository may keep its memory file in-repo; a research
engagement's memory lives in the estate's data dir. Same concept, different
shelf. `reeve-memory-composer.md` § The Stores is amended accordingly when the
memory ladder builds it.

**Graduation paths**, uniform in direction (upward, with distillation):

- working context → short-term → long-term (within the agent, existing)
- agent → persona (existing `propose-memory` deliberative path)
- sub-engagement → parent engagement (on close)
- engagement → estate library (on close, for knowledge that outgrew the work)
- dissolved team → nothing new; its members' learnings have already graduated
  through the paths above

## Operations Vocabulary

The callable surface this model requires. Shapes are implementation; the verbs
and their authority are domain:

| Operation             | Authority                                            |
| --------------------- | ---------------------------------------------------- |
| `open-engagement`     | Operator (top-level)                                 |
| `open-sub-engagement` | Staffed agent, capability-gated (`open_engagements`) |
| `close-engagement`    | Operator (any); the opener, for a sub-engagement     |
| `reopen-engagement`   | Operator                                             |
| `form-team`           | Operator (from template; mints members)              |
| `staff` / `unstaff`   | Operator (top-level); the opener, within its tree    |
| `dissolve-team`       | Operator, with per-member disposition                |
| `mint-agent`          | Operator (teamless standing agent)                   |
| `retire-agent`        | Operator                                             |

The delegated rows share one capability category (working name:
`open_engagements`), parallel to `spawn_agents` — it covers opening a
sub-engagement, staffing it with agents the opener already commands, and closing
what it opened. The operator can always do any of these to any engagement. It is
the same trust step as subordinate spawning — delegation of organizational
structure — and gets the same treatment: category in the profile, action
descriptor at the tool boundary, audit event on the decision.

Every operation above is an audited event. The organizational history of the
estate — who was minted, formed, staffed, dissolved, retired, when and by whom —
is part of the same observability surface as everything else.

## What This Changes

Amendments to existing documents, to be applied as their subsystems are built:

- **`reeve-domain-model.md` § Team** — split into team template (configuration
  artifact, shippable; the current content) and team (durable standing roster;
  this document).
- **`reeve-domain-model.md` § Memory Entry** — project scope retired; engagement
  scope and estate library added; repo-anchored storage becomes a per-engagement
  location policy.
- **`reeve-domain-model.md` § Naming Scopes** — agent names are durable and
  never reused (were: reused per runtime instance). Engagement and team names
  unique per estate; retired/closed names stay reserved to their bearers.
- **`reeve-domain-model.md` § Capability Profile › Categories** — add
  `open_engagements`.
- **`reeve-effectors.md` Decision B** — superseded. The working root is
  engagement context delivered through the incarnation snapshot, not a
  daemon-global recorded at `<state_dir>/working_root`. The effectors ladder
  should build the jail against a per-incarnation root value from day one. The
  daemon-start resolution logic (VCS toplevel of a directory, overrides)
  survives as the _default context proposed when opening an engagement from a
  directory_.
- **`reeve-memory-composer.md` § The Stores** — project store becomes engagement
  store; estate library added. (Applies when the memory ladder builds; the
  composer is not yet implemented.)
- **`reeve-shipped-teams.md`** — intent unchanged; "teams are the unit of
  sharing" now refers to team _templates_. "Changes propagate on next spawn"
  reads "next incarnation."
- **`reeve-roadmap.md`** — this model needs a ladder of its own (durable agents,
  engagements, teams, staffing) and it re-sequences relative to effectors; that
  is a roadmap/planning decision, not made here.

## Scope Cuts

Deliberately out of the first cut:

- **Team memory** — absent by design, per the human model (§ Memory), not merely
  deferred.
- **Multi-engagement teams / multi-team engagements** — strict 1:1-at-a-time
  staffing. Loosen only when a real use case forces it.
- **Recruiting existing agents into new teams** — team formation mints; transfer
  of standing agents between teams is future work.
- **Per-engagement constraint envelopes** — engagements carry context, not
  constraints. If work needs a tighter envelope, that is the team's.
- **Agent-initiated top-level engagements** — agents open sub-engagements only,
  under an existing engagement's context. The estate's front door stays with the
  operator.
- **Registry / publishing mechanics** — team templates remain the portable
  artifact; the sharing story stays in `reeve-shipped-teams.md`.

## Gotchas and Constraints

- **Unstaffed means rootless.** An agent not staffed to an engagement (directly
  or via its team) has no working root; file effectors refuse. This is intended
  — context flows only from engagements. A teamless utility agent that never
  touches files never notices.
- **Rotation restarts.** Staffing a team to a new engagement re-incarnates its
  members, forcibly: each running incarnation winds down (in-flight tool
  invocations complete; no new ones are accepted — the same wind-down as a
  `max_task_duration` trip) and the agent re-incarnates with the new context.
  The runtime must not migrate a running incarnation between contexts, and it
  does not refuse the re-staff — interrupting in-flight work is the operator's
  call to make, with eyes open.
- **Grant changes wait; bans don't.** Tightening a persona or team grant reaches
  a long-lived agent only at its next incarnation. If it must land now,
  blacklist it (floors are live) or force re-incarnation. Operators should
  internalize which lever is which.
- **Sub-engagement opening is a real trust grant.** `open_engagements` lets an
  agent create durable organizational structure. The narrowing law bounds the
  _scope_ of what it creates (context nests, staffing inherits the chain), but
  not the _quantity_; runaway sub-engagement creation is a
  quantitative-threshold concern, same family as `max_concurrent_subordinates`.
- **In-repo engagement memory is visible to the repo.** An engagement that keeps
  its file in-repo is choosing committable, reviewable — and
  collaborator-readable — memory. That is the point; it is also a disclosure
  decision the operator makes per engagement.
- **Names are forever.** Durable identity means agent, team, and engagement
  names are never reused. Naming hygiene matters more than it did when names
  recycled per runtime instance. Names are human-facing handles over the stable
  identity IDs; a future import/restore that collides on a name renames at
  import — identity IDs never collide.
- **The written domain model lags this document** until the amendments in § What
  This Changes are applied. Where they conflict, this document wins on
  organizational structure; the domain model remains canonical for everything it
  covers that this document doesn't touch.
