# Reeve — Effectors

## Context

The fourth ladder in Reeve's build sequence (see `reeve-roadmap.md`). The
first three ladders built a governed estate: agents that spawn, address, and
message each other under supervision, with signed transport, quarantine, a
panopticon, and — from ladder 3 — a live authority check at the tool boundary
(capability profile, blacklist, thresholds).

What the estate cannot do yet is _act on the world outside itself_. Every tool
an agent can call today is an estate tool: `spawn_agent`, `send_message`,
`whoami`, `whois`, `list_agents`, `list_personas`. None of them reads a file,
writes a file, runs a command, or reaches the network. An agent is a chatbot
that can coordinate other chatbots. Ask the lead to "refactor a module" and it
can discuss the refactor, spawn a peer to also discuss it, and message you the
result — but it cannot touch the repository.

This is the ladder that gives agents hands. It ships the effector tools —
`read_file`, `write_file`, `edit`, `shell`, `web_fetch` — routed through the
authority gates ladder 3 already built. Those gates were installed against
capability categories (`ReadFiles`, `WriteFiles`, `ExecuteShell`,
`NetworkEgress`) that no tool carries yet. Ladder 3's own spec reserved this
slot: its action-descriptor section names `Bash(...)`, `Read(...)`,
`Write(...)`, `WebFetch(domain:...)` as "future tool kinds shipped in later
ladders [that] declare their match semantics when they register." This ladder
registers them.

Reeve is not solely a coding tool — it is a generic supervised-actor runtime,
and effectors are the general category through which an agent of any kind
affects anything. File and shell are the first effectors because they are the
highest-leverage and because they exercise the `WriteFiles` / `ExecuteShell`
gates. Web is included because reaching the network is table stakes for most
agent work. The architecture is uniform: every effector is a tool actor that
declares a capability category and a canonical action descriptor, and the
ladder-3 check gates it. Later effectors (a structured HTTP client, an MCP
bridge) slot in the same way.

This spec is the front door for ladder 4 — narrative, scope, and the decisions
that resolve open forks. It does not re-state the authority check; that is
canonical in `reeve-authority.md` (now in `specs/completed/`) and
`reeve-domain-model.md`. Where this ladder resolves an implementation detail
those documents left open, it says so.

## Narrative

You start Reeve at the top of a repository. The daemon records that directory
as the **working root** for the session — the sandbox every file and shell
effector is confined to.

You spawn a worker-persona agent. Its capability profile enables `read_files`,
`write_files`, and `execute_shell`; `network_egress` is disabled. The
SpawnCoordinator snapshots that profile as before.

You ask the lead to fix a failing test, and it delegates to the worker. The
worker's model calls `read_file` on `src/parser.rs`. The tool resolves the
path, confirms it is inside the working root, checks `ReadFiles` against the
worker's profile (enabled) and the blacklist (no match), reads the file, and
returns its contents as a tool result. The worker calls `shell` with `cargo
test parser` — `ExecuteShell` enabled, action descriptor `Bash(cargo test
parser)`, no blacklist match. The command runs with its `cwd` set to the
working root; stdout, stderr, and exit code come back as the tool result. The
worker reads the failure, calls `edit` on `src/parser.rs` to change three
lines, re-runs the test, and it passes.

The worker's model then decides to `git commit`. Git runs through the shell
tool — there is no separate git effector — so the action descriptor is
`Bash(git commit -m ...)`. `ExecuteShell` is enabled and no blacklist entry
matches, so the commit runs. But when the model tries `git push --force origin
main`, the descriptor `Bash(git push --force origin main)` matches the
blacklist entry the operator seeded, and the tool refuses unconditionally with
`layer=blacklist` — exactly the ladder-3 refusal path, now firing on a real
command.

You switch to the panopticon and watch the diff the worker produced. Nothing
was pushed; nothing left your review. The tool invocations — every read, every
shell command with its argv, every write with its path — are in the audit log
and the inspect view, per the observability principle.

Later you spawn a research-persona agent whose profile enables `read_files`
and `network_egress` but **not** `execute_shell`. Its model calls `web_fetch`
on `https://docs.rs/tokio`. The host `docs.rs` is on the operator's egress
allowlist, `NetworkEgress` is enabled, no blacklist entry bans it, so the fetch
runs and the page text comes back — tagged with its source URL and marked
**unclassified content**: it entered agent context without passing a content
classifier, because the gatekeeper (ladder 5) does not exist yet. A fetch to a
host _not_ on the allowlist is refused before any socket opens.

That is what ladder 4 ships: agents that read, write, run commands, and fetch —
each action gated by the machinery ladder 3 built, each confined to a working
root, each observable.

## The Effector Tools

Five tool actors, each declaring one capability category and one action-
descriptor shape. All follow the ladder-2 tool topology (Agent → ToolActor,
authority check in the actor's `Handler<InvokeTool>` before any work).

| Tool         | Category        | Action descriptor         | Notes                                           |
| ------------ | --------------- | ------------------------- | ----------------------------------------------- |
| `read_file`  | `ReadFiles`     | `Read(<resolved-path>)`   | Path jailed to working root                     |
| `write_file` | `WriteFiles`    | `Write(<resolved-path>)`  | Full-content write; path jailed                 |
| `edit`       | `WriteFiles`    | `Edit(<resolved-path>)`   | Targeted string replacement; path jailed        |
| `shell`      | `ExecuteShell`  | `Bash(<command string>)`  | `cwd` = working root; git runs here; not jailed |
| `web_fetch`  | `NetworkEgress` | `WebFetch(domain:<host>)` | Host must be on the egress allowlist            |

`edit` performs a literal, unique-match string replacement — an exact `old`
string and a `new` string, refusing if `old` is absent or occurs more than
once. That uniqueness precondition is the concurrency guard: a blind edit
cannot corrupt the wrong occurrence or silently clobber a file that changed
since the agent read it. `write_file` replaces whole-file contents. Both are
`WriteFiles`.

Match semantics for the blacklist adopt the ladder-3 design: `Bash(...)` is a
prefix match against the command string; `Read`/`Write`/`Edit` are path globs
(`*`, `**`, `~` for home, `.` for working-root); `WebFetch` matches the
normalized host with `*` wildcards. The on-disk / profile name of each category
is its snake_case form (`read_files`, `write_files`, `execute_shell`,
`network_egress`) per the existing `ToolCategory` serialization from ladder 3;
the PascalCase names in this document are the Rust enum variants.

**The `shell` interface, and what the blacklist can promise (correction from
adversarial review).** `shell` takes a command _string_ executed via the
system shell (`/bin/sh -c`), matching the `Bash(command)` mental model
operators already have from Claude Code; the action descriptor is that string.
This is deliberate — agents naturally emit pipes, redirects, and globs — but it
has a consequence the spec states plainly: **for `shell`, the blacklist is a
coarse guardrail, not a deterministic floor.** A prefix match on a command
string is trivially evaded (`/usr/bin/git` for `git`, `env git`,
`python -c "os.system('git push --force')"`, `a; b`), so a shell blacklist
entry deters an honest model, not an adversarial one. The blacklist stays a
true deterministic floor for the _structured_ effectors —
`Read`/`Write`/`Edit`/`WebFetch` — whose descriptor faithfully and unforgeably
represents the action. Real containment for `shell` is the deferred OS-sandbox
pass; until then `ExecuteShell` is a high-trust grant (§ Sandbox Posture).

**Why git has no effector of its own (decision A).** The domain model defines
`GitRead` and `GitWrite` categories, but git in this ladder runs through the
`shell` tool and is therefore gated by `ExecuteShell`, not by the git
categories — which remain defined but **not separately enforced** in ladder 4.
Git-write protection is therefore only whatever the shell blacklist catches
(`Bash(git push --force*)`, `Bash(git reset --hard*)`, history-rewrite
patterns) — and per the honesty caveat above, for `shell` the blacklist is a
coarse guardrail, not a deterministic floor. The working-root jail does **not**
protect git either: git runs in the unconfined shell process, so
`git config --global`, credential helpers, submodule fetches, and
`.git/hooks/*` (which execute arbitrary shell on commit) all reach outside the
root. An implementer who wants to blunt these runs git with hooks and global
config neutered (e.g. `GIT_CONFIG_GLOBAL=/dev/null`,
`core.hooksPath=/dev/null`) — flagged here as a known footgun, not mandated in
ladder 4. A dedicated git effector that splits read from write is deferred —
more faithful to the categories, but adds surface no current use case demands.
This is a deliberate scope cut, recorded below.

## Working Root

**Decision B — the working root is global to the runtime.** At daemon start the
runtime resolves one working root and records it at `<state_dir>/working_root`,
fixed for the daemon's lifetime. Resolution: the VCS toplevel (jj/git root) of
the launch directory if one is detectable, else the launch directory itself.
Using the VCS root — not the raw cwd — means starting Reeve from a
subdirectory still sandboxes to the repository top rather than silently
narrowing the jail to the subdir (the earlier draft's "captures its launch
cwd" contradicted the "top of your repo" intent; this resolves it). A
`--working-root <path>` flag and `REEVE_WORKING_ROOT` env var override
resolution for uncommon cases (daemon under a service manager, a non-repo
directory). Changing the root means restarting the daemon.

Every **file effector** (`read_file`, `write_file`, `edit`) resolves its path
argument and confirms the result is inside the working root before doing
anything else. The same jail applies to **reads and writes** (decision C): an
unrestricted read plus `web_fetch` is a clean exfiltration path — read
`~/.aws/credentials`, POST it to an allowlisted host — so reads are confined
symmetrically with writes. This matches the project's "comparable inputs at the
same boundary receive comparable normalization" default and the blacklist's
existing `Read(**/.env)` vocabulary. Note the scope of the word "jail": it
confines the three _file effectors_. It does **not** confine the `shell`
process, which can touch any absolute path (§ Sandbox Posture).

Path resolution has to handle files that do not exist yet (a `write_file` that
creates a file), so canonicalizing the full path — which fails on a missing
leaf — is wrong. The rule: reject absolute paths and any path whose normalized
form escapes the root via `..`; canonicalize symlinks on the **existing
prefix** (for a create, the parent directory) and join the leaf; then check
prefix-containment against the canonical working root. A symlink inside the
root that points outside it is resolved and rejected — the containment check is
on the resolved target, not the link. An out-of-root path is rejected by the
tool as an invalid argument, and — unlike a clean pass — the rejection **still
emits an audit event** (`effector.path_rejected`, carrying the attempted raw
path and the reason) so an attempted escape is visible in the same channel as
every other decision, not a forensic blind spot.

**The jail is structural, not a blacklist entry.** A path outside the working
root is rejected by the tool itself as an invalid argument, _before_ the
authority check runs, and the rejection is not something an operator can remove
by editing `blacklist.toml`. Operators may _additionally_ blacklist narrower
paths inside the root (e.g. `Write(**/.env)`), but they cannot widen the jail.
The rationale mirrors ladder 3's "no permissive fallback": the boundary that
confines the file effectors should not be one typo in a config file away from
disappearing. (Convention, consistent across the runtime: `state_dir` holds
machine-managed runtime state like `working_root` and is not meant to be
hand-edited; `data_dir` holds operator policy like `blacklist.toml` and
`egress_allowlist.toml` and is meant to be edited.)

Global-to-runtime is the first cut. Per-team or per-agent working roots are a
plausible future — a team assigned to a sub-crate, an agent sandboxed to a
scratch dir — but nothing in the shipped teams forces it yet, and a single root
matches the launch story. Deferred, not foreclosed: the config field is the
extension point.

## Web Access

`web_fetch` reaches the network under `NetworkEgress`. Two gates stack on top
of the capability check, both evaluated in the tool actor before a socket
opens:

1. **Egress allowlist.** `<data_dir>/egress_allowlist.toml` is a list of host
   globs. It is **default-deny**: an empty or absent file blocks all egress.
   The requested host must match an allowlist entry. This is the operator's
   positive statement of where agents may reach; the blacklist's "network
   egress to non-allowlisted hosts" default becomes concrete here.
2. **Blacklist.** After the allowlist admits the host, a `WebFetch(domain:...)`
   blacklist entry can still ban it — the deterministic floor overrides the
   allowlist, same as it overrides a permissive profile.

The allowlist is watched and reloaded in place like the blacklist, with the
same **fail-closed** posture: a malformed file retains the last-good state and
surfaces a reload error to the panopticon, so a typo never silently opens
egress.

Three roles compose here, worth naming to avoid confusion: the **allowlist is
the global ceiling** (no agent reaches a host that is not on it), the profile's
`NetworkEgress` is the **per-agent enable** (an agent reaches the network only
if its profile grants it), and the blacklist is the **global floor** (a banned
host is refused even if allowlisted). An allowlist of `*` collapses the ceiling
— it is "enable all egress" written in a file, and should be read as such.

Host matching is only as trustworthy as the parse. `web_fetch` parses the URL
strictly, **rejects userinfo** (so `https://allowed.com@evil.com/` does not
read as `allowed.com`), normalizes the host (IDNA/punycode, lowercase, strip
trailing dot), and matches the allowlist against that normalized host.
IP-literal hosts are refused in ladder 4 — allowlists are host-name policy;
IP-literal support is a later refinement. Redirects are the subtle case: a
fetch to an allowlisted host that 302s to a non-allowlisted one must not
smuggle content past the ceiling, so the allowlist is re-checked on **every
redirect hop** and a hop to a non-allowlisted host fails the fetch. The
response returns as bytes plus `content-type` and `charset`, with a decoded
`text` field when the body is decodable and within the size limit (§ Limits);
ladder 4 does no HTML-to-readability extraction.

`web_search` (a query against a search provider) is **not** in this ladder. It
needs an external provider and API key, which means the adapter/keychain
pattern the model adapters already use — a meaningful surface of its own. It is
the natural next web effector and is deferred to a scope cut. `web_fetch`
(retrieve a given URL) needs no provider and delivers most of the value.

## Limits

Every effector that returns unbounded data or runs unbounded work carries a
hard limit, so a single tool call cannot exhaust memory, flood the bus tape, or
hang an agent. Ladder 4 sets conservative defaults; the exact numbers are
tunable and not load-bearing for the mental model — what is load-bearing is
that a bounded limit with a visible truncation marker exists:

- `read_file` — a maximum file size; a larger file returns a head-plus-marker,
  not the whole file.
- `write_file` / `edit` — a maximum resulting file size, and `edit` refuses
  binary files.
- `shell` — a wall-clock timeout (the process is killed on expiry) and a
  per-stream byte cap on captured stdout/stderr; over-cap output is truncated
  with an explicit `[output truncated]` marker recorded in the result.
- `web_fetch` — a maximum response size; an over-cap response is truncated with
  a marker.

Truncation is always visible in the tool result and the audit trail — a
truncated read never looks like a complete one. **Redaction is out of scope for
ladder 4:** tool output is captured verbatim, so a command that prints a secret
lands that secret in the bus tape and audit log. This is stated as a known
posture, not an oversight; operator-side redaction is a later refinement, and
it is another reason `ExecuteShell` is a high-trust grant.

## Content Trust and the Gatekeeper

Content that `read_file` and `web_fetch` return enters agent context
**unclassified**. Reeve's content-security layer — the gatekeeper, which
inspects content at promotion boundaries for prompt injection — is ladder 5 and
does not exist yet. Web-fetched content in particular is the canonical
injection surface: adversarial instructions embedded in a fetched page are
exactly what the gatekeeper is built to catch.

Ladder 4 does **not** block on the gatekeeper, but it is explicit about the
gap: every effector tool result carries a `content_source` annotation (the
resolved file path or the fetched URL) so that (a) the provenance is visible in
the audit log and inspect view today, and (b) ladder 5 has the hook it needs to
classify content at the point it entered. Until the gatekeeper lands, an
operator who enables `network_egress` for an agent is accepting that fetched
content is trusted-by-default inside that agent's context. This is stated as a
gotcha, not hidden.

The interaction cuts the other way too: shipping web access _raises the
priority_ of the gatekeeper ladder. It was already next in the roadmap; this
ladder is the reason it should stay next.

## Sandbox Posture

The working-root jail (over the file effectors), the capability profile, and
the blacklist are the boundaries this ladder ships. OS-level sandboxing — macOS
Seatbelt, Linux Landlock / seccomp to confine what a spawned shell process can
touch and reach — is **deferred to a hardening pass**, not built here. That
deferral has a sharp consequence the spec states plainly rather than burying:
**`ExecuteShell` is an unconfined local grant.** A shell process is constrained
only by its `cwd`; it is not jailed. An agent with `ExecuteShell` can:

- reach the network via `curl` / `wget` / `git remote`, bypassing the egress
  allowlist entirely;
- read and write **any absolute path** — `~/.ssh`, `/etc`, `$HOME` — bypassing
  the working-root jail that confines the file effectors;
- evade shell blacklist entries through trivial obfuscation (§ The Effector
  Tools).

So the file-effector jail and the egress allowlist are meaningful boundaries
for personas **without** `ExecuteShell`, and best-effort only for personas with
it. The mitigation until the OS-sandbox pass is compositional and must be
understood by whoever configures a team: grant `ExecuteShell` only to personas
trusted with general local capability, and do not rely on the jail or egress
allowlist to contain an agent that also has shell. Confining the shell process
— filesystem and network — is the explicit job of the deferred sandbox pass.

## Audit and Observability

Effector invocations flow through the ladder-3 audit surface unchanged: every
invocation emits an `authority.decision` entry (Allow or Refuse) with its
action descriptor, and the tool result — including the `content_source`
annotation — is recorded on the agent's bus tape and rendered in the inspect
view. No new on-disk facility is introduced. The panopticon's existing tool-
activity and pending-decisions surfaces populate with real file/shell/web
actions instead of the estate-only tools they showed before.

Model cost and thresholds are unaffected: `read_file`, `write_file`, `edit`,
`shell`, and `web_fetch` make no model call, so they do not touch the cost
meter or the `cost_per_*` thresholds. (A future `web_search` backed by a
provider would.)

## Reading Order

For implementers, read in this order:

1. This document — what ladder 4 ships, the three resolved forks, the deferred
   surfaces.
2. `specs/completed/reeve-authority.md` § The Authority Check, § Blacklist —
   the gate these effectors plug into and the action-descriptor / match
   semantics adopted here verbatim.
3. `reeve-domain-model.md` § Capability Profile — the closed category enum
   (`ReadFiles`, `WriteFiles`, `ExecuteShell`, `NetworkEgress`, and the
   deferred `GitRead` / `GitWrite`).
4. `reeve-multi-agent.md` § The Tool Subsystem — the tool actor topology every
   effector follows.

## Scope Cuts

Deferred to later ladders or out of scope:

- **Dedicated git effector** (decision A). Git runs through `shell`;
  `GitRead` / `GitWrite` are defined but not separately enforced. A git tool
  that splits read from write is a future refinement.
- **`web_search`.** Needs a search provider and key (adapter/keychain
  pattern). `web_fetch` ships; search is the next web effector.
- **Per-team / per-agent working roots** (decision B). One global root this
  ladder; the config field is the extension point.
- **Unrestricted reads outside the working root** (decision C). Reads are
  jailed. If a use case needs reading system paths, the intended shape is an
  explicit operator allowlist of read-outside paths, not a blanket relaxation.
- **Content classification** — the gatekeeper is ladder 5. Effector content
  enters unclassified with a `content_source` hook for it.
- **OS-level process sandbox** (Seatbelt / Landlock / seccomp) — deferred
  hardening pass. Until then, `shell` bypasses both the egress allowlist
  (`curl`) and the file jail (absolute paths); that is the known gap.
- **MCP / structured HTTP effectors** — the tool architecture admits them;
  none ship here.

## Gotchas and Constraints

- **`ExecuteShell` is unconfined.** With shell, an agent bypasses the egress
  allowlist (`curl`), the working-root file jail (absolute-path read/write),
  and shell blacklist entries (obfuscation). It is a high-trust local grant;
  the jail and allowlist do not contain it. Grant it deliberately.
- **Git runs hooks and helpers.** Through the shell, `git commit` fires
  `.git/hooks/*` (arbitrary shell), and git may invoke credential helpers and
  submodule fetches that reach the network. Neuter with `core.hooksPath=/dev/null`
  / `GIT_CONFIG_GLOBAL=/dev/null` if that matters; not mandated in ladder 4.
- **The jail is on resolved paths, and rejections are audited.** Symlinks are
  canonicalized (on the existing prefix, so creates work) before the
  containment check; a symlink inside the root pointing outside it does not
  escape. Path resolution happens before the authority check, but an out-of-root
  path still emits an `effector.path_rejected` audit event — it is not a silent
  drop.
- **`web_fetch` re-checks the allowlist on every redirect.** An allowlisted
  host that redirects to a non-allowlisted one fails the fetch rather than
  smuggling its content in. Userinfo is rejected and hosts are punycode/
  case-normalized before matching.
- **Egress is default-deny.** A fresh install with no `egress_allowlist.toml`
  blocks all `web_fetch`. This is intended; the operator opts hosts in.
- **Output is captured verbatim and bounded.** No redaction in ladder 4 — a
  printed secret is logged. Oversized reads, shell output, and responses are
  truncated with a visible marker.
- **Fetched content is trusted-by-default until ladder 5.** Enabling
  `network_egress` on an agent means accepting unclassified web content in its
  context until the gatekeeper ships.
- **Working root is fixed per daemon lifetime.** Moving to a different repo
  means restarting the daemon (or overriding with `--working-root`).
