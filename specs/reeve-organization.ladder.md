## Phase 1: Engagements exist

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-07-10 | 2026-07-11 |

Greenfield: no struct in the codebase carries a working root or work record today. The operator->daemon command channel is decision A1: operations are signed envelopes deposited to a system inbox for a new EstateCoordinator actor (start it in launch_actors, crates/reeve-runtime/src/daemon.rs ~880), which validates operator-tier trust via the existing trust-tier machinery, mutates the durable record, and appends audit events. This is the roadmap's 'filesystem is the TUI<->runtime protocol' decision extended to operations - do NOT add a socket/RPC. The CLI already deposits signed envelopes (crates/reeve-cli/src/send.rs); reuse that path with new Commands variants in crates/reeve-cli/src/main.rs (~33). Engagement record fields per specs/reeve-organization.md section Engagement: purpose, context (working root, immutable after open - reopening restores the same root; a different root is a different engagement), lifecycle state, staffing (empty this phase), parent (None this phase). Context resolution default is the VCS toplevel of the invoking directory, jj-colocated aware. Audit: add AuditEvent variants with explicit #[serde(rename="engagement.opened")] etc. in crates/reeve-runtime/src/audit.rs (~73) - dotted kinds require explicit rename; update the each_event_serializes_under_pipe_buf test (~706). Data-root cleanup: resolve_reeve_data_root (crates/reeve-runtime/src/fs_util.rs ~321) currently appends BOTH reeve AND identities, so agents/, personas/, teams/, audit/ all nest under a dir named identities/ - fix before layering engagements/ on top; RuntimeLayout (crates/reeve-runtime/src/agent_fs.rs ~52) vends all paths so the change is centralized. Gotcha: names are forever (spec section Gotchas) - the store must reject reuse even of closed engagements' names. The working root is carried and audited but NOT enforced this ladder - the file jail is the effectors ladder (5); build the record and snapshot plumbing so effectors can consume it.

#### Delivers

- EstateCoordinator actor handling signed operator-tier operation envelopes
- Durable engagement store (engagements/ under the reeve data root) with open/close/reopen lifecycle and immutable context
- Data-root cleanup: un-nest agents/, personas/, teams/, audit/ from the misnamed .../reeve/identities/ directory (identities keep their own subdir); simple one-shot migration for existing installs
- reeve engagement open|close|reopen|list CLI subcommands (signed envelopes to the coordinator)
- TUI chat slash-command (/engagement ...) signing the same operator envelopes
- engagement.* audit event kinds (opened, closed, reopened, context_resolved)

#### Done When

- Given a repo directory, `reeve engagement open --name <n> --purpose <p>` resolves context to the VCS toplevel (jj/git; explicit --root override wins) and writes a durable record that survives daemon restart
- Reopening a closed engagement restores the identical context; any attempt to alter context on an existing engagement is refused and audited
- Engagement names are unique per estate and never reused after close/reopen cycles
- Operations arriving without operator-tier trust are refused and quarantine/refusal is audited
- The TUI slash-command produces the same audit trail as the CLI verb
- validate-wrap just validate passes

#### Depends On

- (none)

## Phase 2: Teams stand

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-07-13 | 2026-07-15 |

Teams today are bootstrap-only: TeamConfig/TeamMember (crates/reeve-runtime/src/config.rs ~178-263) are read once in prepare_agent_startup (crates/reeve-runtime/src/daemon.rs ~644) to pick the lead persona and prompt cap; count is parsed and never acted on; the lead is the hardcoded string "lead" (daemon.rs ~718, ~795, ~1080). This phase makes the template:team :: persona:agent split real per specs/reeve-organization.md section Team: the template (existing TOML at teams/<name>.toml) stays the shippable config artifact; forming instantiates it into a durable roster record. Minting reuses the spawn machinery's provisioning steps (AgentDirs::provision, generate_or_load_keypair, identity mint, snapshot write - see the spawn sequence in crates/reeve-runtime/src/spawn_coordinator.rs ~390-721) but with deterministic operator-chosen names instead of the random {persona}-{hex} suffix (~414). Formed members start incarnations immediately but are unstaffed (no engagement, no root - the librarian pattern from the spec narrative is legal). AgentRegistry (crates/reeve-runtime/src/agent_registry.rs ~320, registry.toml keyed by name, upsert-accumulating) needs an explicit never-reuse check on mint; the identity ID (already globally unique, never reused) stays the stable key underneath - names are human-facing handles. Dissolution semantics per spec section Team: operator act, per-member disposition, learnings survive via existing graduation paths (nothing new to build for that here). Operations go through the phase-1 EstateCoordinator as signed operator envelopes. Gotcha: resume_persisted_subagents (daemon.rs ~1159) iterates every non-lead record - it must handle team-member and teamless records uniformly after de-hardcoding.

#### Delivers

- Durable team registry: standing rosters of named durable agents, formed from team templates
- form-team operation honoring TeamMember.count, minting members as durable named agents (deterministic names, e.g. <team>-<role>[-<n>])
- dissolve-team operation with per-member disposition (retire | release to teamless standing)
- mint-agent / retire-agent operations for teamless standing agents
- Name-permanence enforcement in the agent registry (no reuse of any past agent name)
- De-hardcoded lead: first run forms the default team from teams/default.toml; attach resolves to the team's lead role
- team.* / agent.* audit kinds (formed, dissolved, minted, retired, released)

#### Done When

- Given the shipped default template, `reeve team form default` mints one durable agent per (persona,count) with stable names, all resumable across daemon restart
- Dissolving a team with mixed dispositions retires some members and releases others to teamless standing; retired names are permanently unavailable for new mints
- A fresh install boots by forming the default team and attach lands on its lead (walking-skeleton demo still works verbatim)
- Attempting to mint an agent with any previously used name is refused and audited
- just validate passes

#### Depends On

- engagements-exist

## Phase 3: Staffing moves teams

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ⬜ not-started  |            |            |

The heart of the model: context flows only from engagements, through staffing, into per-incarnation snapshots. SpawnSnapshot (crates/reeve-runtime/src/model_resolution.rs ~22) is the existing per-incarnation capture written to agent.toml and re-read verbatim on resume (resume_one_subagent, crates/reeve-runtime/src/daemon.rs ~1234) - extend it with optional engagement context (engagement name + working root); also fix its stale agent_id doc-comment claiming identity is transient (it is durable). Re-incarnation semantics per specs/reeve-organization.md sections Constraints and Gotchas: grants and context snapshot per incarnation; re-staffing is forcible - each running incarnation winds down exactly like a max_task_duration trip (see the threshold trip handling in crates/reeve-runtime/src/agent.rs ~660, ~976: no new tool invocations or model calls, in-flight completes), then the actor restarts with the new snapshot. The runtime must not migrate a running incarnation between contexts and does not refuse the re-staff - interrupting work is the operator's call. Incarnation invariants (spec section Agent): at most one live incarnation per agent; re-incarnation only on daemon restart, re-staffing, or explicit operator restart; persona/grant revisions wait for the next one. Staffing-unit rule is per level (spec section Staffing authority follows the tree): top-level units are teams or teamless agents ONLY. Rootlessness: unstaffed means no root in the snapshot and no fallback - when effectors land in ladder 5 they refuse for want of context; nothing in this ladder may invent a default root. Operations via EstateCoordinator (phase 1); staffing state lives on the engagement record.

#### Delivers

- staff/unstaff operations (team <-> top-level engagement; lone teamless agent as the degenerate unit)
- Engagement context enters the incarnation snapshot (agent.toml gains engagement name + working root)
- Forced re-incarnation on re-staffing: wind-down (in-flight tool invocations complete, no new ones) then restart with the new snapshot
- Unstaff cascade: recalling the unit recalls every sub-staffing in the tree (tree machinery arrives in phase 5; the cascade contract is built here)
- staffing.* audit kinds (staffed, unstaffed, reincarnated)
- Panopticon engagement surface: engagements listed with name, state, root, and the staffed unit; whoami reports the invoking agent's engagement and root

#### Done When

- Staffing a team writes the engagement's root into each member's per-incarnation snapshot, visible in agent.toml and the inspect surface
- The panopticon shows every engagement with its state, root, and currently staffed unit, updating on reload when staffing changes; an agent calling whoami sees its engagement name and working root
- A team staffed to engagement A then re-staffed to engagement B has every member wind down and re-incarnate with B's root; the old incarnation accepts no new tool invocations after the re-staff lands
- Staffing a second unit to an already-staffed engagement is refused; staffing a team already serving an engagement is refused (strict 1:1-at-a-time)
- A top-level lone staffing is accepted only for teamless agents; a team member offered as a top-level unit is refused
- An unstaffed agent's snapshot carries no root (no daemon-cwd fallback anywhere)
- just validate passes

#### Depends On

- engagements-exist
- teams-stand

## Phase 4: The policy chain composes

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ⬜ not-started  |            |            |

The narrowing law made executable, per specs/reeve-organization.md section Constraints: two independent chains - policy (operator -> team -> agent-at-incarnation) evaluated as the meet, context (engagement -> sub-engagement -> agent) built in phases 3/5. Today the layers are: CapabilityProfile snapshot checked per-tool via check_authority (crates/reeve-runtime/src/tool.rs ~181), global BlacklistRegistry hot-reloaded via WatcherActor (crates/reeve-runtime/src/daemon.rs ~934, crates/reeve-runtime/src/blacklist.rs), thresholds in the agent (crates/reeve-runtime/src/agent.rs ~660). This phase inserts the team scope: the team record carries an envelope (categories to intersect with the persona profile at incarnation-snapshot time; blacklist entries to union with the global registry at evaluation time; budget pot). Composition rules per kind are in the spec verbatim - grants intersect, floors union, ceilings intersect (egress allowlist arrives with effectors; build the union/intersect plumbing so it slots in), budgets partition (shared pot, the tree-aggregate semantics cost_per_session already established). Propagation split is load-bearing (spec section Constraints, Propagation): floors LIVE (a team ban must land mid-incarnation - reuse the blacklist hot-reload pattern), grants SNAPSHOT (computed intersection written into the per-incarnation profile.toml at incarnation start; never mutated mid-flight). Refusal enum (tool.rs ~122, serde tag=layer, non_exhaustive) and authority.decision audit (crates/reeve-runtime/src/audit.rs ~157) gain scope attribution. install_defaults (crates/reeve-runtime/src/config.rs ~314) currently writes NO profile.toml so defaults run unrestricted (daemon.rs ~986 logs it) - ship conservative default profiles for lead/deepseek-r1/glm-5.2. Write authority flows downward: nothing an agent can write participates in its own confinement - team envelopes are operator-edited parts of the durable team record; agents have no write path.

#### Delivers

- Team constraint envelope on the durable team record (enabled-category narrowing, additional blacklist entries, team budget pot)
- Composed authority check: effective = operator floor/ceiling AND team envelope AND agent incarnation snapshot (grants intersect, floors union, budgets draw from the team pot)
- Refusal and audit name the deciding scope (layer=profile|team|blacklist|threshold)
- install_defaults ships real profile.toml for every default persona (closes the silent-unrestricted hole)
- Live-vs-snapshot propagation honored: blacklist (floors) stays hot-reloaded mid-incarnation; team grant changes land at next incarnation

#### Done When

- A category enabled in a member's persona profile but disabled in the team envelope is refused with layer=team, and the audit event names the team
- A blacklist entry added at the team scope refuses a member's matching action even though the global blacklist has no such entry
- Team budget: model spend across members draws down one shared pot; on exhaustion further model calls across the team are refused (cost_per_session trip semantics), audited
- A fresh install's default personas run with installed profiles, not unrestricted; the daemon no longer logs profile.toml missing for defaults
- Editing a team envelope mid-incarnation does not change a running member's grants; its next incarnation picks the change up; blacklist edits land immediately (existing hot-reload preserved)
- just validate passes

#### Depends On

- teams-stand

## Phase 5: Delegation goes organizational

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ⬜ not-started  |            |            |

Delegated organizational structure under the narrowing law, per specs/reeve-organization.md sections Engagement (Staffing authority follows the tree) and Operations Vocabulary. Add OpenEngagements to ToolCategory (crates/reeve-runtime/src/capability.rs ~34: variant + Display arm; snake_case open_engagements on disk; unrestricted profiles get it for free per the opt-in model at ~99 - which is why phase 4's installed default profiles matter: they must NOT include open_engagements except where deliberate). Tool actors follow the ladder-2 topology (Agent -> ToolActor, authority check first in Handler<InvokeTool> - see crates/reeve-runtime/src/tool/spawn_agent.rs for the canonical shape including canonical_action for blacklist matching). The commands relation is precisely defined (spec section Staffing authority): an agent commands its spawn subtree (identity.created_by already exists and is used for the max_concurrent_subordinates count, crates/reeve-runtime/src/spawn_coordinator.rs ~484 - reuse it, make it transitive) and the team lead additionally commands team members; non-lead peers command nothing. Containment: resolve and compare roots the same way the future jail will (symlink-resolved, prefix-containment) so the two checks can share an implementation when effectors land; containment against absent context contains nothing (rootless parent -> rootless subs). One quantitative note: runaway sub-engagement creation is the same family as max_concurrent_subordinates but Thresholds is a closed enum requiring a schema bump to extend (capability.rs ~73) - defer the threshold, note it in the phase's docs/comments rather than bolting it on. The one thing an agent must never do: open or staff TOP-LEVEL engagements - that authority stays with the operator (decision A1; conversational top-level operation was deliberately excluded).

#### Delivers

- OpenEngagements variant in ToolCategory plus the open_sub_engagement / staff_sub_engagement / close_sub_engagement tool actors (one actor, three verbs, or three actors - follow the ladder-2 tool topology)
- The commands relation: spawn subtree (transitive, via identity.created_by) plus team-lead-commands-members; nothing else
- Context-nesting containment check: sub-engagement root strictly inside parent root; rootless parent implies rootless sub-engagements
- Cascade recall wired end-to-end: unstaffing the top-level unit recalls all sub-staffings in the tree (contract from phase 3, tree now real)
- Close-by-opener: the opener may close what it opened; operator may close anything

#### Done When

- An agent with open_engagements staffed to an engagement opens a sub-engagement with a narrower root and staffs a spawned subordinate to it, with no operator involvement, fully audited (authority.decision allow + engagement.opened + staffing.staffed)
- A sub-engagement whose requested root is not strictly inside the parent's is refused as invalid before the authority check, and the rejection is audited
- An agent without the open_engagements category is refused with layer=profile; a matching team-envelope ban refuses with layer=team
- Staffing an agent the opener does not command (a peer, another team's agent) is refused; a non-lead member cannot staff its fellow members
- Unstaffing the team from the top-level engagement recalls the subordinate from the sub-engagement; the sub-engagement remains open, awaiting staffing
- just validate passes

#### Depends On

- staffing-moves-teams
- the-policy-chain-composes

## Phase 6: Work has a file

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ⬜ not-started  |            |            |

The files lane of the memory model, per specs/reeve-organization.md section Memory and Knowledge: durable organizational knowledge lives in files (engagement memory, estate library); heads (agent short/long-term, persona) are NOT this ladder - no composer, no retrieval, no scoring; that is the memory ladder. Keep it mechanical: the engagement file is an append-oriented store (markdown or JSONL - pick one, document why) at engagements/<name>/memory.* by default, or under the working root when the engagement was opened with the in-repo policy (spec: storage location policy, not a scope - this preserves what was valuable about the retired project memory: travels with the artifact, reviewable like code). Write path: direct for staffed agents - the file is the work's own notebook - gated by the existing write_memory category (crates/reeve-runtime/src/capability.rs); tool actors follow the ladder-2 topology. The estate library is the estate's canon: writes are deliberative per spec, and this ladder ships the minimal honest version the spec explicitly allows ('or the operator writes directly') - operator-direct writes via CLI/coordinator, agent proposals deferred to the memory ladder alongside the curator machinery. Memory is never policy (spec, verbatim rule): no tool, gate, or scope reads these files to decide authority; keep the stores physically separate from constraint artifacts. Content read from engagement files is an injection surface for the future gatekeeper - tag reads with content_source provenance the way the effectors spec does for read_file, so ladder-6 classification has its hook. Distillation is behavioral, not generative machinery: the close operation accepts distillation text (the closing agent writes it as part of concluding); mechanically it is an attributed append to the parent file.

#### Delivers

- Engagement memory file: durable per-engagement store, direct-write (append) for staffed agents via a tool, readable by staffed agents
- In-repo location policy: an engagement whose context is a repository may keep its file in-repo (committable); default stays in the estate's data dir
- Archive-on-close with the record; intact on reopen
- Sub-to-parent distillation at sub-engagement close (close op carries distillation text appended to the parent's file with provenance)
- Estate library store with operator-direct writes (reeve library add/list) and an agent read tool
- memory.* audit kinds for engagement-file and library writes

#### Done When

- A staffed agent appends to its engagement's file via the tool (gated by write_memory) and a later-staffed different unit reads the same content - the rotation test passes end-to-end
- An unstaffed agent invoking the engagement-memory tool is refused (no engagement, nothing to write to)
- Closing a sub-engagement with distillation text appends it to the parent's file attributed to the closing sub-engagement; closing the top-level engagement archives its file; reopening restores it intact
- An engagement opened with the in-repo policy keeps its file under the working root (committable); the default keeps it in the data dir
- Library writes succeed only from the operator (agents' direct writes refused and audited); agents can read library entries via the tool
- just validate passes

#### Depends On

- staffing-moves-teams

## Phase 7: The corpus tells the truth

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ⬜ not-started  |            |            |

The documentation phase is load-bearing, not ceremonial: specs are build-phase artifacts that retire to specs/completed/ when a ladder finishes, and durable rationale must land in ADRs (docs/src/decisions/, one file per decision, Context/Decision/Consequences, never edited after the fact) and rustdoc - per the project's Documentation conventions in CLAUDE.md. Mara's editorial finding applies here verbatim: an amendments ledger nobody executes is how spec corpora rot - this phase executes it. Apply each bullet of reeve-organization.md section What This Changes to its target: reeve-domain-model.md (Team section split into template vs standing roster; Memory Entry project scope retired in favor of engagement + estate library; Naming Scopes - agent names durable, never reused; ToolCategory gains open_engagements), reeve-effectors.md (Decision B gets a superseded-by header pointing at the organization spec - do not rewrite the rest; the effectors ladder consumes the root from the incarnation snapshot), reeve-memory-composer.md (store list note: project becomes engagement, estate library added - annotation only, composer is unbuilt), reeve-shipped-teams.md (teams-as-unit-of-sharing refers to templates; next-spawn reads next-incarnation). Roadmap: organization slots as ladder 4, everything after shifts by one; record the rationale in Key Decisions (effectors' jail consumes engagement context; building effectors first would have meant building against a stub). ADR topics are the three decisions future readers will interrogate: why constraints compose by narrowing across two chains, why durability is identity-level, why operations are signed envelopes rather than RPC.

#### Delivers

- All spec amendments from reeve-organization.md section What This Changes applied: domain model (Team split, Memory Entry scopes, Naming Scopes, open_engagements category), effectors Decision B superseded-by note, memory-composer store list note, shipped-teams template/incarnation wording
- Roadmap resequenced: organization is ladder 4 (shipped), effectors ladder 5, gatekeeper 6, memory 7, skills-versioning 8, shipped-teams 9; Key Decisions gains the resequence rationale
- ADRs: the narrowing law and two-chain scope model; identity-level durability (runtime disposable, organization durable); the estate-coordinator operations channel (A1)
- Rustdoc (//!) for every new module; mdBook updated where operational behavior changed

#### Done When

- Every amendment bullet in reeve-organization.md section What This Changes is either applied to its target document or explicitly marked deferred-with-reason in that bullet
- reeve-roadmap.md sequence and status tables reflect the new order and this ladder's completion
- Three ADRs exist in docs/src/decisions/ with Context/Decision/Consequences and are linked from the decisions index
- cargo doc --no-deps produces zero warnings; prettier-check passes on all touched markdown
- just validate passes

#### Depends On

- delegation-goes-organizational
- work-has-a-file

## Notes

### Non-goals

- No effector enforcement of the working root - the file jail, shell, and web effectors are ladder 5 (reeve-effectors); this ladder builds the record, snapshot, and containment plumbing they consume.
- No memory retrieval: no composer, no scoring, no promotion machinery - the heads lane and library proposal/curation path are the memory ladder.
- No conversational top-level estate operations (decision A1): top-level open/form/staff/dissolve are operator acts via CLI or TUI slash-commands signing operator envelopes; agents operate conversationally only within an engagement tree (phase 5).
- No team-template registry/publishing mechanics (stays in reeve-shipped-teams).
- No agent transfer between teams; formation mints, dissolution retires or releases.

### Dependency note

Phase 2 (teams-stand) depends on phase 1 only for the EstateCoordinator operations channel; the team/engagement stores themselves are independent. If parallelizing, the coordinator lands first, then 1's remainder and 2 can proceed side by side.

### Sequencing note

This ladder is ladder 4; reeve-effectors moves to ladder 5 and its Decision B (daemon-global working root) is superseded by engagement context - the effectors ladder should be re-read against reeve-organization.md before planning.
