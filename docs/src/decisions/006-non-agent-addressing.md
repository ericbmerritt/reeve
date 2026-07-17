# ADR 006 — System actors are not agents: `SystemRegistry` and `IdentityType::System`

## Context

The estate coordinator (`crate::estate::EstateCoordinator`) needs a name,
identity, and inbox so the CLI and TUI can address it exactly like any
other message recipient — decision A1 (`specs/reeve-organization.ladder.md`)
makes the filesystem the protocol for every operation, agent-directed or
not. The original implementation got this by reusing `AgentRegistry`: the
daemon provisioned `agents/estate/` via `AgentDirs::provision`, minted an
`IdentityType::Agent` identity, and registered an `AgentRecord` under the
reserved name `estate`.

`AgentRecord` carries `persona_name`, `status` (`Running`/`Stopped`/
`Retired`), and `spawned_at` — incarnation-lifecycle fields with no meaning
for the estate coordinator, which has no persona, no model calls, and no
incarnation to restart. Every one of the following had to carry a special
case, or silently break, because estate looked like an agent to code that
never expected a registry entry without those semantics:

- The daemon's resume pass (`resume_persisted_subagents`) needed an
  explicit `if record.name.as_str() == ESTATE_AGENT_NAME { continue; }`
  guard so it did not try to resume a nonexistent incarnation.
- Clean shutdown needed a bespoke branch to mark estate `Stopped`, because
  nothing else in its lifecycle touched that field.
- The TUI panopticon, `reeve attach <name>`, and chat-submit all walk
  `AgentRegistry::list()`/`lookup()` expecting every record to be a
  model-backed agent with an `agent.toml` (`SpawnSnapshot`). Estate never
  had one. `reeve attach estate` followed by typing a message reached
  `submit_message` → read `agent.toml` → `io::ErrorKind::NotFound`, an
  unhandled error that unwound the whole TUI event loop and crashed the
  process (`Error: Submit(Io { path: ".../agents/estate/agent.toml", ... })`).

The crash was not a missing filter. It was a symptom: `AgentRegistry`
promises "every entry is a durable, model-backed agent," and estate broke
that promise. Adding a filter at each call site would have fixed the
symptom at the cost of one more thing every future agent-walking call site
has to remember. The question was whether estate should behave enough like
an agent to earn the promise (get a persona, an incarnation, model calls),
or whether it should stop making the claim at all.

Estate deterministically processes structured `EstateOp` commands. It has
no LLM in the loop, no conversation history, no persona. It does not
belong in `AgentRegistry` under any definition of "agent" this codebase
uses elsewhere.

## Decision

Introduce a general **non-agent addressing** mechanism rather than a
one-off carve-out for estate:

- **`IdentityType::System`** (`reeve-types`) — a fourth identity category
  alongside `Operator`/`Agent`/`External`, for runtime-internal actors that
  are not model-backed and have no incarnation. `Identity::new_system(...)`
  mints one. Every `match` on `IdentityType` in the codebase is closed (no
  wildcard arm), so adding this variant was a compile-time audit: the
  compiler listed every decision point that needed one, rather than
  requiring a manual grep.
- **`SystemRegistry`** (`reeve-runtime::system_registry`) — a registry
  parallel to `AgentRegistry` but minimal: `name → (identity_id,
inbox_dir)` only. No persona, no status, no `spawned_at`. Same
  filesystem-safety posture (`0o700` directories, no symlink-following,
  atomic tmp → fsync → rename writes) at `<data-root>/system/registry.toml`.
- The daemon provisions estate's identity and inbox the same way as
  before (`AgentDirs`, `generate_or_load_keypair` — that machinery is pure
  filesystem layout, not agent-specific), but registers it in
  `SystemRegistry` under `Identity::new_system`, not in `AgentRegistry`.
- `reeve send`/`reeve engagement`'s CLI transport (`reeve-cli::send::send`)
  resolves a recipient name against `AgentRegistry` first, then
  `SystemRegistry` — the same two-tier lookup any future system actor gets
  for free, without a name-specific branch.
- The TUI's `/engagement` slash command resolves estate via
  `SystemRegistry` directly (it always targets the coordinator, never an
  arbitrary name).
- Chat-style submission (`reeve attach <name>`, panopticon Enter, quarantine
  convert-and-resend) now checks `AgentRegistry` membership before
  attempting to read `agent.toml`, surfacing a recoverable notice ("`'x'`
  is not a chattable agent") instead of an unhandled IO error. This is a
  general robustness fix, not an estate-specific check: any mistyped or
  stale chat target gets the same clean refusal.

No migration ships for existing `AgentRegistry` entries written by a
pre-ADR-006 daemon — this project has exactly one installation right now,
and its stale `estate` record was deleted by hand rather than carrying
migration code for a population of zero other installs. A real migration
is warranted once there are other installs to protect.

`estate` remains a reserved _agent_ name — `mint-agent`/`retire-agent`
still refuse it (`"reserved"`) — so an operator cannot mint a real agent
that shadows the coordinator's name in a different registry.

## Consequences

- The daemon's resume pass, shutdown handler, and every agent-walking TUI
  surface (panopticon, attach, whoami) no longer need to know estate
  exists. It is structurally absent from `AgentRegistry`, not filtered out
  — the special cases these call sites used to carry are deleted, not
  relocated.
- A future non-agent system actor (if one is ever needed) gets addressing,
  CLI `send` support, and exclusion from every agent-walking surface for
  free by registering in `SystemRegistry` — no new call-site audits.
- `IdentityType::System` is a real trust-tier category now, not folded
  into `Agent`. Any future code that branches on identity type for
  authority or audit purposes will get a compile error until it decides
  what a system actor's tier means there — the same forcing function that
  made this change itself safe to land.
- `reeve-cli::send::send` grew a second registry dependency
  (`system_registry_path`), threaded through its two callers
  (`cmd_send`, `reeve engagement`'s `send_and_await`). This is the
  "intentional friction" pattern from ADR 001: a new non-agent recipient
  is reachable without a call-site change, but the two-registry lookup
  itself is visible in the function signature, not hidden.
