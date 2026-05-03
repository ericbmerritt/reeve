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

## What to ask before doing

- Architectural deviations from the specs.
- Decisions the specs mark as open (see `reeve-domain-model.md` § Open
  Questions).
- Anything that would expand a phase beyond its `done_when` criteria.
