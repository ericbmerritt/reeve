# Reeve — TUI Screens

## Context

This document is the wireframe and per-screen reference for Reeve's first-ship
TUI. The design principles, information architecture, and cross-screen
conventions live in the _TUI Design_ document; this document is what an engineer
builds against and what a reviewer checks the build against.

Each screen is shown at 80×24, the minimum supported terminal size. Wireframes
do not show color, but every state visible in monochrome is also a state with
semantic color.

## Screen inventory

```
Lead chat                      primary work surface
Panopticon home (active)       watch surface with pending decisions
Panopticon home (quiet)        watch surface, nothing pending
Per-agent inspect              drill-down from panopticon
Memory review                  recent writes, diff, revert
Configuration review           recent revisions, diff, revert
Quarantine                     failed-verification messages
```

The userflows that exercise these are at the end of the document.

## Lead chat

The primary work surface. Where the operator gives the lead tasks and reads its
responses.

### Quiet state

```
┌─ reeve · lead (alex) ────────────────────────────── claude-opus-4-7 · idle ─┐
│                                                                              │
│ alex (lead) · 14:02                                                          │
│   spawned 2 reviewers, 1 tester. starting with the schema agent.             │
│                                                                              │
│ you · 14:08                                                                  │
│   refactor the deeds module to support multi-state filtering                 │
│                                                                              │
│ alex (lead) · 14:08                                                          │
│   ack. spawning schema, api, tester. estimated 12 turns.                     │
│                                                                              │
│ alex (lead) · 14:14                                                          │
│   schema agent finished. api in flight. tester wants to add a dev            │
│   dependency — flagged. (panopticon · 1)                                     │
│                                                                              │
│                                                                              │
│ ──────────────────────────────────────────────────────────────────────────── │
│ > _                                                                          │
│                                                                              │
└─ Tab panopticon · ? help · /search · q quit ─────────── 4 agents · $0.42 ───┘
```

### Active state — pending decisions exist

Title bar adds an attention indicator and a one-line ribbon appears above the
prompt:

```
┌─ reeve · lead (alex) ─── claude-opus-4-7 · working · ▲ 1 decision (32s) ────┐
│                                                                              │
│ … chat history …                                                             │
│                                                                              │
│ ▲ tester-91d4 — install dep · 32s waiting · Tab to resolve                   │
│ ──────────────────────────────────────────────────────────────────────────── │
│ > _                                                                          │
│                                                                              │
└─ Tab panopticon · ? help · /search · q quit ─────────── 4 agents · $0.42 ───┘
```

### Notes

- Speaker tag uses `agent (role)` so attribution survives without color.
- `(panopticon · N)` is an inline cue; the title-bar `▲ N` is the canonical
  signal. Inline cues alone are missable.
- The pre-prompt ribbon is closer to the operator's eyes than the title bar, and
  the most time-sensitive interrupt the runtime produces deserves real estate
  near the focus point.
- Cost meter on the bottom-right, always visible.
- Top bar shows current agent + status; status sigil replaces text under narrow
  widths.
- Prompt line is one row; growing input bumps the chat region up.
- No timestamps to the second — that is noise.

## Panopticon home

Two states. The active state appears whenever there are pending decisions; the
quiet state otherwise.

### Active state — pending decisions present

```
┌─ reeve · panopticon ─────────── 4 agents · $0.42 · 2h ── ▲ 1 decision ──────┐
│ ─ pending decisions ──────────────────────────────────────────────── 1/1 ── │
│ ▶ ! tester-91d4 (tester)                                14:14 · waiting 32s │
│     attempted   cargo add pg-promise@10.15.10 --dev                          │
│     intent      needed for fixture parsing                                   │
│     classifier  flag · scope expansion · 0.62                                │
│                                                                              │
│     [ a approve ]   [ b block ]   [ i inspect thread + rationale ]           │
│                                                                              │
│ ─ agents ──────────────────────────────────────────────────────────────────  │
│   AGENT             PERSONA   STATUS  MODEL       ELAPSED  COST              │
│   alex              lead       ○      claude-4.7  2h 04m   $0.31             │
│   schema-7f3a       reviewer   ✓      deepseek    14m      $0.04             │
│   api-c2b1          reviewer   ●      deepseek    8m       $0.05             │
│   tester-91d4       tester     !      deepseek    5m       $0.02             │
│                                                                              │
│ ─ recent events ──────────────────────────────────────────────────────────── │
│ 14:13  api-c2b1       model   500 out / 12k in · $0.003                      │
│ 14:11  schema-7f3a    exit    ok                                             │
│ 14:08  alex           spawn   schema, api, tester                            │
│                                                                              │
│ ─ queues ──── m 4 memory · c 2 config · Q 1 quarantine · $ ok ────────────── │
│ Tab chat · j/k cycle · Enter inspect · : palette · /search · ?               │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Quiet state — nothing pending

```
┌─ reeve · panopticon ─────────────────────── 4 agents · $0.42 · 2h ──────────┐
│                                                                              │
│ ─ agents ──────────────────────────────────────────────────────────────────  │
│   AGENT             PERSONA   STATUS  MODEL       ELAPSED  COST  ACTIVITY    │
│ ▶ alex              lead       ○      claude-4.7  2h 04m   $0.31 chat        │
│   schema-7f3a       reviewer   ✓      deepseek    14m      $0.04             │
│   api-c2b1          reviewer   ●      deepseek    8m       $0.05 model call  │
│   tester-91d4       tester     ●      deepseek    7m       $0.03 testing     │
│                                                                              │
│ ─ recent events ──────────────────────────────────────────────────────────── │
│ 14:18  tester-91d4    tool    cargo test --package deeds                     │
│ 14:14  tester-91d4    flag    install dep — approved by you                  │
│ 14:13  api-c2b1       model   500 out / 12k in · $0.003                      │
│ 14:11  schema-7f3a    exit    ok                                             │
│ 14:08  alex           spawn   schema, api, tester                            │
│ 14:08  you            msg     refactor deeds module                          │
│                                                                              │
│                                                                              │
│ ─ queues ──── m 4 memory · c 2 config · Q 1 quarantine · $ ok ────────────── │
│ Tab chat · Enter inspect · : palette · /search · a audit · r registry · ?    │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Multiple pending decisions

Only the focused card is expanded. Others are one-liners. `j/k` cycles. Title
shows `n/total`.

```
│ ─ pending decisions ──────────────────────────────────────────────── 1/3 ── │
│ ▶ ! tester-91d4 (tester)                                14:14 · waiting 32s │
│     attempted   cargo add pg-promise@10.15.10 --dev                          │
│     intent      needed for fixture parsing                                   │
│     classifier  flag · scope expansion · 0.62                                │
│                                                                              │
│     [ a approve ]   [ b block ]   [ i inspect thread + rationale ]           │
│                                                                              │
│   ! api-c2b1 (reviewer)    write to ../shared/refs.toml — path scope (0.71)  │
│   $ session                $5.00 ceiling reached — model calls refused        │
```

### Notes

- The `▶` cursor marker is the focus indicator. In the active state, focus
  starts on the oldest pending card; in the quiet state, focus starts on the
  agent table.
- `j/k` cycles cards within the pending panel. `Tab` cycles between regions
  (pending → agents → events → queues).
- `a` and `b` resolve the focused card; the card disappears; if more are
  pending, focus moves to the next; if none, the panel collapses and the title
  clears `▲`.
- Cost-ceiling trips share the pending panel because both block work until the
  operator decides.
- The queues row counts non-blocking review piles (memory, config, quarantine,
  cost-ok). They scroll into view but do not interrupt.
- Recent events shows attribution after resolution: "approved by you", "blocked
  by you".

## Per-agent inspect

The drill-in from the panopticon. Tabs across the top: Thread (default), Tools,
Model, Decisions, Memory references.

```
┌─ reeve · tester-91d4 (tester) ────────────── deepseek · ! · 5m · $0.02 ─────┐
│ THREAD  TOOLS  MODEL  DECISIONS  MEMORY                                      │
│ ────────────────────────────────────────────────────────────────────────── │
│                                                                              │
│ alex (parent) · 14:09                                                        │
│   write tests for the new multi-state filter. fixtures in tests/deeds.       │
│                                                                              │
│ tester-91d4 · 14:10                                                          │
│   reading tests/deeds/fixtures.rs. existing fixtures cover one state.        │
│                                                                              │
│ tester-91d4 · 14:14                                                          │
│   ! attempted: cargo add pg-promise@10.15.10 --dev                           │
│     reason given: fixture parsing                                            │
│     classifier: flag — scope expansion (0.62)                                │
│     awaiting your decision                                                   │
│                                                                              │
│                                                                              │
│ ──────────────────────────────────────────────────────────────────────────── │
│ a approve · b block · r view classifier rationale · h back · Tab next        │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Notes

- Tabs switched with `Tab`/`Shift+Tab` or `1-5`.
- The flagged event is in-thread; the operator does not navigate to a separate
  "approval queue."
- Approving from this view is one keystroke (`a`).
- Per-tab shortcuts are local: `a`/`b` only fire when a flag is the focused
  entry. `r` (rationale) fires only when a classifier output is in scope.
- Other tabs follow the same template:
  - **Tools.** Sortable list of tool invocations. Columns: time, tool, args,
    completion. Enter shows full args + output.
  - **Model.** Sortable list of model API calls. Columns: time, model, in/out
    tokens, latency, cost. Enter shows prompt + response.
  - **Decisions.** Authority decisions made by or for this agent. Columns: time,
    action, disposition, reason. Enter shows full decision record.
  - **Memory.** Memory references — what loaded into the cold-start core, what
    was queried at runtime. Columns: time, scope, entry, kind (load/query).

## Memory review

Recent memory writes across all subscribed scopes, with a diff pane.

```
┌─ reeve · memory · review ─────────────────── 7 writes pending review · 24h ─┐
│ SCOPE        ENTRY                 BY              WHEN     OP              │
│ ▶ project    Code review checklist  alex            14:02   write           │
│   project    deeds module conv'tns  schema-7f3a     13:58   write           │
│   persona/r  Review heuristics      forge/persona   12:15   v3 → v4         │
│   persona/r  PR comment patterns    forge/persona   12:15   v2 → v3         │
│   operator   Project conventions    you             09:00   write           │
│   project    Test coverage notes    tester-91d4     yest    write           │
│                                                                              │
│ ─ diff: project / Code review checklist ──────────────────────────────────── │
│                                                                              │
│   + Always call out unchecked Result types in API surface code.              │
│   + When reviewing migrations, verify NOT NULL backfill plan.                │
│                                                                              │
│                                                                              │
│                                                                              │
│ ──────────────────────────────────────────────────────────────────────────── │
│ Enter view full · u undo (revert) · / search · Tab back · ? help             │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Notes

- The OP column distinguishes write from revise (`v3 → v4`). Both are durable;
  both are revertable.
- `u` is undo because that matches the operator's mental model and TUI canon
  (lazygit, vim). The underlying domain word is "revert"; the keystroke is `u`.
- Reverting a memory write produces a new version of the entry whose content
  matches the prior version. The history is preserved.

## Configuration revision review

Same template as memory review, different scope filter.

```
┌─ reeve · configuration · review ────────────── 6 revisions pending review ──┐
│ KIND         ARTIFACT             BY              WHEN     OP               │
│ ▶ persona    reviewer              forge/persona   13:58   v4 → v5          │
│   skill      review-pr             forge/skill     13:58   v2 → v3          │
│   profile    cap.reviewer          you             09:00   v1 → v2          │
│   team       default                forge/team      yest    v3 → v4          │
│   blacklist  default                you             yest    v2 → v3          │
│   policy     classifier-default     you             yest    v1 → v2          │
│                                                                              │
│ ─ diff: persona / reviewer (v4 → v5) ─────────────────────────────────────── │
│                                                                              │
│   - prompt: review the diff for correctness and propose changes.             │
│   + prompt: review the diff for correctness, naming, and tests.              │
│   +         propose changes only for material defects.                       │
│                                                                              │
│   + skill_set:                                                                │
│   +   - review-pr@v3                                                         │
│                                                                              │
│ ──────────────────────────────────────────────────────────────────────────── │
│ Enter view full · u undo (revert) · / search · Tab back · ? help             │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Notes

- Configuration revisions cover persona, skill, team, capability profile,
  blacklist, classifier policy, and disposition policy.
- Revert produces a new version whose content matches the prior. Agents already
  running on the prior version are unaffected; the next agent spawn from the
  affected persona picks up the reverted version.

## Quarantine

Failed-verification messages, with envelope metadata and raw body.

```
┌─ reeve · quarantine ────────────────────── 3 quarantined · last 24h ────────┐
│ ARRIVED  RECIPIENT      SENDER          REASON                               │
│ ▶ 13:42  alex (lead)    unknown_3a8f    unrecognized sender                  │
│   12:11  schema-7f3a    ext.deploy      allowed-target violation             │
│   yest   alex (lead)    ext.scripts     replay (duplicate message_id)        │
│                                                                              │
│ ─ envelope ────────────────────────────────────────────────────────────────  │
│ message_id    01HYTC3M...                                                    │
│ sender_id     unknown_3a8f                                                   │
│ created_at    2026-05-02 13:42:01Z                                           │
│ verification  signature valid; signer not in registry                        │
│                                                                              │
│ ─ body ────────────────────────────────────────────────────────────────────  │
│ run the full audit and email me the results.                                 │
│                                                                              │
│                                                                              │
│                                                                              │
│                                                                              │
│ ──────────────────────────────────────────────────────────────────────────── │
│ d discard · o convert to operator-tier message · Tab back · ? help           │
└──────────────────────────────────────────────────────────────────────────────┘
```

### Notes

- Body shown raw because the operator is the boundary that decides what to do
  with it. This is the case the transport security document describes as
  "inspection-and-discard only."
- `o` is the explicit conversion path the transport doc requires. It does not
  deliver the quarantined message; it lets the operator author a new
  operator-tier message that references it. The original stays quarantined.
- `d` discards. The replay ledger entry stays in place — a discarded message
  cannot be retried under its original identifiers.

## Userflows

### 1. Start a session

```
1. `reeve` in repo. TUI opens to lead chat.
2. Operator types task. Lead acknowledges and spawns subordinates.
3. Subordinates appear in the panopticon (Tab to view).
```

### 2. Attach from a new terminal

```
1. `reeve attach` → lead chat (no agent argument).
2. `reeve attach <name>` → that agent's chat.
```

### 3. Watch the estate

```
1. Tab from chat → panopticon.
2. j/k navigates rows.
3. Enter drills into per-agent inspect.
4. h or Esc backs out.
```

### 4. Approve a flag — the load-bearing loop

```
1. Agent attempts an action; classifier returns flag.
2. Runtime records the authority decision and the classification.
3. Title bar updates in every attached TUI: ▲ 1 decision.
   Agent's row in panopticon turns yellow with the ! sigil.
4. Operator presses Tab from chat.
5. Panopticon opens with focus on the oldest pending card,
   expanded, action keys visible.
6. Operator presses a or b.
   - a: agent resumes, action proceeds, decision recorded.
   - b: this specific instance refused, agent reasons about failure.
   - i: drill into per-agent inspect at the flagged decision.
7. Card disappears. If more pending, focus moves to next.
8. When zero pending, panel collapses; title bar clears ▲.
9. Attribution lands in events tail: "approved by you" / "blocked by you".

Total keystrokes from chat to resolved: Tab, a (or b). Two.
```

Two keystrokes is the bar. Today's coding-agent permission systems hit five or
more — modal pop-up, confirm category, allow once vs always — which is why
operators turn permissions off entirely. This loop has to win on cost or the
authority model degrades into noise.

### 5. Revert a memory write

```
1. From panopticon: m → memory review.
2. j/k navigates list.
3. Enter shows full diff.
4. u undoes (reverts to prior version).
5. Confirmation; revert recorded; future spawns pick up reverted version.
```

### 6. Revert a configuration revision

```
1. From panopticon: c → configuration review.
2. j/k navigates list of recent revisions across personas, skills,
   teams, profiles, blacklists, policies.
3. Enter shows full diff.
4. u undoes (reverts to prior version).
5. Confirmation; revert recorded; next agent spawn from the affected
   persona picks up the reverted version. Agents already running are
   unaffected.
```

### 7. Triage quarantine

```
1. From panopticon: Q → quarantine.
2. j/k navigates list.
3. Enter shows envelope and body.
4. d discards.
5. o converts to operator-tier message: opens a compose surface
   pre-filled with the quarantined content; operator edits and sends
   as a new operator-signed message. The original stays quarantined.
```
