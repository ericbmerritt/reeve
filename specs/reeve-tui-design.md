# Reeve — TUI Design

## Context

The overview commits Reeve to two screens — a chat with the lead and a
panopticon — and to a set of operator surfaces that extend from them: review
queues for memory writes, configuration revisions, and quarantined messages;
pending-decision surfaces for classifier flags and cost-ceiling trips; per-agent
drill-in for threads, tools, model calls, authority decisions, and memory
references.

This document covers the TUI design that ties those surfaces together: the
principles, the information architecture, the cross-screen conventions, and what
the TUI explicitly does _not_ do. The wireframes and per-screen notes are in the
_TUI Screens_ document.

## Design principles

1. **Keyboard-first.** Every action reachable from the keyboard. Bindings
   consistent with TUI canon — `q` quit, `?` help, `/` search, `hjkl`/arrows for
   movement, `Enter` select, `Esc` back, `Tab` next pane, `1-9` jump to tab.

2. **Information hierarchy.** Most important information gets the most visible
   position. Pending decisions outrank live agent activity, which outranks
   historical events, which outranks counters, which outranks navigation hints.

3. **States exist.** Empty state tells why and offers a way forward. Loading
   indicates progress and is cancellable. Error says what happened and what to
   do next. No mystery states.

4. **Density is a feature.** Reeve operators read densely — ten years of
   exposure to vim, htop, lazygit, k9s. Whitespace is intentional, not
   decorative.

5. **Color is information.** Semantic only. Red for errors. Yellow for pending
   operator attention. Green for success. Output remains parseable under
   `NO_COLOR=1`. Sigils alongside color so meaning survives monochrome.

6. **80×24 minimum.** No hard-coded widths that break at 80 columns. Test path:
   `stty rows 24 cols 80 && reeve`.

7. **Consistency over cleverness.** Match existing TUI canon — vim, htop,
   lazygit, k9s, gh, fzf. Do not invent new interaction patterns when
   established ones exist.

8. **Microcopy.** Terse, specific, developer-appropriate. No marketing tone. No
   exclamation marks. Say what happened, not how the tool feels about it.

## Information architecture — five modes

The TUI is a multi-resource explorer. Following the k9s pattern: one
application, many resource types, a `:` command palette to switch contexts, plus
single-letter shortcuts for the common ones.

```
1. ESTATE     Live agent rows + recent events. Watch surface.
              Reached: panopticon home (default).

2. ATTENTION  Pending operator decisions: flags, cost trips,
              quarantine, recent writes, recent revisions.
              Reached: pending-decisions panel on home;
              dedicated review screens via `m`, `c`, `Q`, `$`, `F`.

3. INSPECT    Drill into one resource. Per-agent (default), per-team,
              per-persona, per-memory-entry, per-message.
              Tabs inside.
              Reached: `Enter` on a row.

4. REGISTRY   Browse what is installed. Personas, skills, teams,
              profiles, blacklists, policies, identities, adapters,
              routes. Read-mostly tables.
              Reached: `r` opens a registry chooser, or
              `:personas`, `:skills`, etc., directly.

5. AUDIT      Query the audit log. Hot ring buffer up front,
              historical scan behind it. Free-form filter.
              Reached: `a` from any screen.
```

The five modes are the answer to _"how does the operator find the thing they
want?"_ Estate is the home. Attention is the interrupt. Inspect is the deep
dive. Registry is the browse. Audit is the look-back.

## Cross-screen conventions

### Status sigils

Sigils carry semantic meaning even under `NO_COLOR=1`:

```
○  idle              ●  working           !   flag pending
✓  exit ok           ✗  exit error        ⏵   spawning
▲  attention         $  cost ceiling      ?   awaiting input
[ ] focused action
```

### Color contract

Color is additive, never sole-bearer of meaning:

```
red     error / failed verification / exit error
yellow  pending operator attention / working / !flag / ▲ indicator
green   ok / approved / committed / exit ok
blue    operator-action target / approval surface
dim     historical / inactive / resolved items
```

Test path: run `NO_COLOR=1 reeve` and confirm every state remains readable from
sigils and microcopy.

### Global keybindings

Consistent across all screens:

```
q          quit
?          help (overlay)
/          search (filter active surface)
Tab        next pane / cycle focus
Esc / h    back / dismiss
Enter      select / inspect / drill in
j/k or ↓/↑ move within current pane
1-9        jump to tab or screen
:          command palette (resource switching, saved queries)
```

Per-screen action keys are listed in the screen's footer. No keybinding does two
different things across screens. `u` is always undo / revert. `a` is always
approve. `b` is always block. `d` is always discard. `o` is always
operator-action (e.g., convert quarantined to operator-tier message).

### Title bar

Every screen carries a title bar:

```
Left:   reeve · <screen> · <focus>
Right:  context (model, status, elapsed, cost)
        ▲ <count> on the right when pending decisions exist
```

The `▲` attention indicator is sticky across screens so the operator never has
to remember to check.

### States

```
empty
  panopticon  ─ "no agents running. give the lead a task or
                 `reeve team start`."
  memory      ─ "no recent writes."
  quarantine  ─ "nothing quarantined."
  audit       ─ "no events match. broaden the filter."

loading
  startup     ─ "connecting to runtime…  Ctrl+C to abort"
  spawning    ─ status sigil ⏵ + activity "spawning…" in row
  model call  ─ activity "model call · 2.1s" with elapsed counter

error
  no runtime  ─ "no runtime found. start one with `reeve`."
  crashed     ─ row turns ✗, inspect shows reason; thread retained
  detach lost ─ banner: "runtime stopped responding. retrying…
                 q to exit."
```

## Update cadence

The TUI subscribes to the runtime over a local socket. Push updates are
debounced at the renderer to ~250 ms human-perceptible cadence to avoid jitter
on the agent-row sigils. If the socket lags, the TUI falls back to a 1–2 Hz
poll. Status sigils animate at the debounced cadence; cost meters and activity
strings update with the same beat.

The cadence is not a tunable. Faster makes the panopticon feel busy and degrades
the operator's ability to read state at a glance; slower makes it feel stale and
erodes trust in what the screen says.

## Boundaries — what the TUI does _not_ do

```
not a chat client
  Beyond the lead and per-agent chat surfaces themselves, the
  panopticon does not let you compose messages to agents. Admin
  actions (approve, block, revert) are not "messages" — they are
  runtime decisions surfaced in the TUI.

not a config editor
  Registries are read-mostly. Edits land through forge agents or
  by editing files. The TUI shows revisions and lets you revert.
  No TOML editor.

not a model dashboard
  Model-call telemetry surfaces here, but Reeve is not a Grafana
  for LLM ops. The audit log is exportable for deeper analytics.

not a multi-runtime console
  One runtime per machine per operator. No fleet view across
  machines. If that becomes useful, it is a separate product.

not a CI surface
  Reeve produces code; what it produces could become a CI system.
  The TUI is not a CI dashboard.
```

## Open design questions

```
1. First-run experience.
   `reeve` outside a repo spawns the default team and the lead
   orients the operator. The lead's first message has not been
   designed; it determines whether the user understands delegation
   in the first 90 seconds.

2. Subordinate chat (`reeve attach <subordinate>`).
   Same chat layout as the lead with friction on input. Not
   yet wireframed.

3. Connection-loss banner + reconnect.
   What does the TUI show when the runtime stops responding?
   When it comes back? Two states; both easy to misdesign.

4. Multi-TUI concurrency.
   Two TUIs attached, both typing into the lead. The overview
   commits to multiple TUIs sharing one runtime. Behavior under
   concurrent input is implicit; worth validating with one real
   session.

5. Help and onboarding (`?`).
   Lazygit's modal panel listing the current screen's keys is the
   right pattern. Trivial to add once screens stabilize.

6. Sound / bell on flag.
   Some operators want an audible cue. Most do not. Default off,
   opt-in via runtime config. If on, fire only on flag arrival,
   never on resolution.

7. Cost meter behavior during flag wait.
   Should the agent's cost meter pause while a flag is pending
   operator decision? Probably not — the operator's decision is
   part of the cost — but worth confirming once one real
   ceiling trip happens during a wait.
```

## Deferred — design later, ship without

```
registry browsers     Read-mostly tables for personas / skills /
                      teams / profiles / blacklists / policies /
                      identities / adapters / routes. The k9s-style
                      template handles all ten the same way.

cost view             Sortable table of spend by scope with
                      sparklines. Subset of what the panopticon
                      already shows.

audit query           Free-form filter over the JSON Lines log.
                      Ship a substring filter on the panopticon's
                      recent-events tail first; build the dedicated
                      mode when operators ask to look back further
                      than the buffer.

per-team inspect      Same template as per-agent inspect, with
                      team-level counters.

per-persona inspect   Same template as per-agent inspect, with
                      version history and aggregate stats across
                      instances.

per-memory-entry      Same template, with reference-count chart
inspect               and version diff stack.

analytics             Decision and classification distributions
                      over time. Saved-query views on top of audit.
```

## Verdict path

The first ship covers: chat, panopticon home (active + quiet states), per-agent
inspect, memory review, configuration revision review, quarantine. That is the
operator's hot loop end to end. Everything else in this document is either a
downstream template (registries, per-team, per-persona) or a follow-up iteration
once the build has produced real events to react to.
