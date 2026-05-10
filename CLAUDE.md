# CLAUDE.md

Reeve is a Rust runtime that supervises AI coding agents as named, addressable,
supervised actors on a developer's workstation. This file is also accessible as
AGENTS.md (symlink).

## Reading order

1. `specs/reeve-overview.md` — what Reeve is and why
2. `specs/reeve-roadmap.md` — build sequence and load-bearing decisions
3. Sibling specs as relevant
4. The current ladder's `.md` and `.ladder.md` in `specs/`

## Conventions

- **Specs are canonical.** Design lives there, not here. Don't restate spec
  content. If a design question is not answered, surface it; don't invent.
- **VCS is jj-colocated.** Use jj idioms. Git read commands are fine; don't run
  state-modifying git commands unless asked.
- **Markdown is prettier-formatted at 80 columns** with `--prose-wrap always`.
- **Architectural commitments** in `reeve-roadmap.md` § Key Decisions are not
  revisited mid-implementation.

## Design defaults

These are settled project-level rules distilled from reviewer cycles. The
team-execution pipeline loads them at start so the panel doesn't re-litigate
them and the executor starts with them pre-loaded. Each rule carries a
`Why:` rationale and `Source:` cycle citations so future readers can judge
whether the rule still holds (and retire it if not).

- **Comments justify their bytes by what the code can't say.** Comments that
  paraphrase the next line, restate the function name, or narrate code
  structure are deleted before commit. Comments that explain non-obvious
  cross-module behavior, external-system interaction, or intentional design
  tradeoffs stay. When in doubt: ask whether a careful reader who has *only
  the code in front of them* (not the surrounding crate, not the external
  system's docs) can recover the comment's information. If yes, delete. If
  the comment names a coupling the code at this location can't show, keep
  it.
  **Why:** Drifted/restating comments lie when code changes; cross-module
  WHY comments are precisely what a single file can't tell the reader.
  **Source:** Phase 2, ~20 instances across t3 cycles c2–c17;
  cross-codebase reinforcement from jjr.
- **Tests using `rx.await` wrap in `tokio::time::timeout`.** Bare `.await`
  on a oneshot/mpsc receiver in a test hangs forever if the producer drops;
  CI fails by timeout instead of by clean assertion. Wrap in
  `tokio::time::timeout(Duration::from_millis(N), rx).await` with a bounded
  duration; on `Err(Elapsed)` the test fails with a useful diagnostic.
  **Why:** Diagnose failures by assertion, not by CI timeout.
  **Source:** [t3/c10 priya.p1, p2]; reinforced across Phase 3 t5 cycles.
- **Tests of quarantine paths assert audit events.** Any test exercising a
  quarantine code path includes an `audit_lines!(kind=transport.quarantine,
  reason=<expected>)` assertion. A test that hits the path without observing
  the audit proves only that the function returned, not that the system
  recorded the security event.
  **Why:** Quarantine is a security boundary; the audit trail is what
  oncall and operators read.
  **Source:** [t3/c5 priya.p1], [t3/c6 yelena.y1, priya.p1].
- **Search adjacent files in the same crate before writing helpers.**
  Before writing any helper function, validation routine, mock actor,
  capturing collector, fixture writer, or shared utility, grep the
  surrounding crate (not just the same file or the deps) for similar
  patterns. Cross-file duplication within a single crate is the most common
  DRY-violation in this codebase. If you find yourself writing the same 4+
  line setup or helper for the third time, factor it into the crate's
  `test_support.rs` (or equivalent) before committing — once a duplicate
  ships, future code copies-and-adapts.
  **Why:** Iris (dry-eye) flags this deterministically; the cost is one
  grep before writing.
  **Source:** [t3/c6 iris.i1], [t3/c7 iris.iris-01], [t3/c8 iris.dry-001],
  [t3/c13 iris.d1]; cross-codebase reinforcement from jjr.
- **Comparable inputs at the same boundary receive comparable
  normalization.** When multiple free-form string inputs (or any inputs
  sharing a class — caller-supplied strings, byte-capped fields, trimmable
  user text) cross the same boundary, every input in that class receives
  the same normalization and the same validation. If `system_prompt` is
  `.trim()`-ed at boundary, every other free-form string at that boundary
  is too unless the asymmetry is documented and justified in the type or
  doc comment.
  **Why:** Boundary asymmetry is a coherence bug that compiles and passes
  happy-path tests; reviewers catch it but each instance costs a cycle.
  **Source:** [t4/c9 magnus.m1], [t5/c7 priya.p1], [t5/c8 yelena.y1].
- **One-shot reply channels are `Option<Recipient<Reply>>::take()`.** For
  spawn-coordinator-style request/response actors that return a single
  reply on success or an error path: type `reply_to` as
  `Option<Recipient<Reply>>` and use `.take()` semantics. The first caller
  delivers; subsequent callers (timer, error path, drop) see `None` and
  skip. Do not type as `Recipient<Reply>` and rely on clone +
  cancellation discipline — that is a leak (or double-fire) waiting to
  happen. Generalization: this is one instance of a broader pattern
  (sentinel state uses `Option<T>::take()/replace()`, not a typed
  sentinel) that two unrelated codebases converged on.
  **Why:** Idempotency over cancellation is the right shape for one-shot
  replies in actix; `.clone()` semantics doesn't mean what callers expect.
  **Source:** [t5 cycles 1-3 SpawnRelay Trigger-1 thrash, human escalation
  t5c3]; cross-codebase reinforcement from jjr (`__Pending` sentinel
  resolution).
- **Derived display attributes disclose their derivation in the name.**
  When one user-facing identity is computed from another (e.g.
  `display_name` from `agent_name_str` with a suffix), the variable name
  and the field name make the derivation visible. Use
  `derived_display_name`, not `display_name`; `pretty_<source>`, not
  `name`. The reader of the code should not have to trace the assignment
  to know which field is the source of truth.
  **Why:** Aliasing source field names onto derived values masks the
  derivation; reviewers consistently catch this and the fix is always
  renaming.
  **Source:** Multiple Phase 3 CONSENSUS findings on persona/display_name
  aliasing — [t5/c1 SpawnResponse illegal state], [t5/c5 persona=
  display_name], [t5/c8 4-reviewer CONSENSUS on display_name derivation].

- **All `ProcessInbound` messages are issued through a single private
  `Watcher::deliver` function.** No code outside that function calls
  `do_send(ProcessInbound { ... })` directly. The function owns the full
  delivery sequence: check `DeliveryLedger`, record delivery, append audit
  event, dispatch. New dispatch paths go through it — they do not get their
  own ledger check.
  **Why:** Two independent dispatch paths (`dispatch_verified_envelope` and
  `scan_cur_and_dispatch`) had inconsistent `DeliveryLedger` checks. The
  second path bypassed the ledger entirely, causing duplicate dispatches that
  required `SeenIds` in `Agent` as a band-aid — a second in-memory dedup
  source with different restart semantics than the durable ledger.
  **Source:** Design review 2026-05-10; see
  `docs/decisions/001-single-processInbound-dispatch.md`.

## Documentation

Specs are build-phase artifacts. Once a ladder is complete they are retired;
design rationale and operational knowledge live in two durable places instead:

- **`docs/decisions/`** — Architecture Decision Records. One file per
  significant design choice (`001-tool-actor-trust-boundary.md`,
  `002-actor-system-internals.md`, …). Three sections: Context, Decision,
  Consequences. Never edited after the fact; superseded ADRs get a
  `Superseded by:` line at the top. When a reviewer conversation settles a
  non-obvious architectural question, it belongs here.
- **`//!` rustdoc** — what each crate and module does, its public surface,
  and links to the book for narrative context. `cargo doc --no-deps` must
  produce zero warnings. `just docs` builds the mdBook at `docs/` and opens
  it.

Do not put design rationale in `CLAUDE.md` or `AGENTS.md`. Put it in an ADR.
Do not put operational instructions in spec files. Put them in the book.

## What to ask before doing

- Architectural deviations from the specs.
- Decisions the specs mark as open (see `reeve-domain-model.md` § Open
  Questions).
- Anything that would expand a phase beyond its `done_when` criteria.
