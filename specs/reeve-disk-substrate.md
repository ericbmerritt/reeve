# Reeve — Versioned Disk Substrate

## Context

Reeve is built on a Unix-like substrate. Every actor has a filesystem address.
Every state mutation is on disk. Every change is recorded in a bus tape. The
system is debuggable with `cat`, `ls`, `tail`, and `grep`, without specialized
tools.

This document specifies how versioning works in that substrate, and what
discipline holds the substrate clean over time.

## The Discipline

State lives on disk. History lives in the bus tape. The panopticon is the lens
that reads both. There is no metrics layer.

This is a real architectural commitment. Prometheus-style metrics would be a
fourth source of truth competing with the disk and the tape, and over time the
sources would drift. The system is already self-describing through its
substrate. Adding a parallel observation channel would create the kind of
accumulated complexity that mature systems acquire and never remove.

If a question cannot be answered from the disk and the tape, the answer is to
enrich the disk or the tape. Not to add a fourth source.

Time-series questions are answerable from the tape. Cost over time is filtering
tape entries for cost meter events and summing. Threshold history is filtering
for `set-threshold` messages. The panopticon renders these as charts when
needed. The mechanism is the tape, not a metrics service.

## Versioning as Substrate Property

Versioning is not a feature attached to certain artifacts. It is a property of
the disk format. Every mutable artifact carries a version. Prior versions are
retained. Nothing is overwritten in place.

The on-disk representation makes this natural to read and natural to write.

## The Layout

Every versioned artifact lives in a directory. The directory contains numbered
version files, a `current` pointer file, and a `versions.toml` manifest.

```
personas/coder/
  v0001.toml
  v0002.toml
  v0003.toml
  current        # contains: v0003
  versions.toml  # ordered history with metadata
```

The `current` pointer names the active version. Reading the current version is
one indirection through the pointer. Reading any historical version is a direct
path lookup. The disk is self-describing. `ls` shows the full version history.

The manifest `versions.toml` records each version's number, timestamp, the
sender that authored the change, the bus tape entry that triggered it, and a
brief description. The manifest is an index into the bus tape. It lets the
panopticon render history without scanning the tape directly. If the manifest
and the tape ever disagree, the tape wins and the manifest is rebuilt.

## Atomic Transitions

New versions are written by creating a new versioned file and atomically
updating the pointer. The pattern is write-and-rename, the same primitive used
for inboxes and other state mutations across Reeve.

Readers always see a consistent state. Either the old version or the new one.
Never a torn read. No transactions or locks are required. The atomicity comes
from the filesystem.

## What Gets Versioned

Personas. Each persona is a directory of versioned TOML files. New versions are
written when the forge promotes defaults, when memory is curated, when skills
change, when subsystem defaults are updated.

Skills. Each skill is versioned. Personas reference skills by version, not by
name alone. Updating a skill produces a new version. Existing personas continue
using their referenced version until explicitly updated.

Memory entries. Each memory has a version history. Editing a memory creates a
new version. The active version is what the composer can inject.

Subsystem defaults at instance scope. Each actor's subsystems directory holds
versioned subsystem state. Every authoritative tuning produces a new version
with the pointer updated atomically. An actor can be rolled back to a prior
tuning state by repointing.

Teams. Team configurations are versioned. A team published as a unit references
specific versions of personas, skills, and seed memory.

## What Does Not Get Versioned

The bus tape itself. The tape is append-only by nature. Versioning applies to
mutable artifacts, not to the immutable record of changes to those artifacts.

Actor run state that is genuinely transient. Status files, current cost meter
values, in-flight tool call records. These are working state, not curated
artifacts. They are overwritten in place because their history is captured by
the tape.

The distinction is between artifacts that humans curate and artifacts that the
runtime updates as a side effect of activity. The first category is versioned.
The second is not.

## Rollback

Rollback is structural. There is no rollback feature. Repoint the `current` file
to a prior version. The action is a normal authoritative write through the
dispatcher, signed by the operator or the forge, recorded on the bus tape.

The next consumer of the artifact reads the new pointer. For a persona, the next
actor spawned from it inherits the older defaults. For a subsystem default, the
actor picks up the older state on its next dispatcher cycle. The substrate makes
rollback a one-line operation because the substrate retains everything.

## Diffs

Diffs are real diffs. Comparing versions is
`diff personas/coder/v0006.toml personas/coder/v0007.toml`. Standard tools work
without modification. The panopticon can render diffs when that is more
readable, but the rendering is optional. The substrate is debuggable with shell
tools alone.

## Disk Usage

Retaining every version of every artifact has a cost. For the kinds of artifacts
in question, which are text configuration, memory entries, and skill
definitions, the cost is small and grows slowly. A persona promoted ten times in
a year is ten small TOML files.

If retention ever becomes a real concern, the right answer is a separate
archival pass. Compress old versions periodically, keep recent versions hot.
Defer this until it matters. Start by retaining everything.

Garbage collection of unreferenced versions is not a thing. The `current`
pointer is the only thing that determines what is active. Prior versions are
part of the artifact's history regardless of whether anything currently uses
them. There is no notion of unreferenced in this substrate, and that absence is
intentional. It avoids a class of bugs around when deletion is safe.

## Properties

Several useful properties fall out of this substrate.

The disk is the truth. State is not in memory waiting to be flushed. State is on
disk continuously. Crash recovery is restart and read.

History is durable. Every change to every artifact is permanently recorded. The
project's evolution over months or years is reconstructible from the same files.

Inspection is shell-native. Operators do not need a special tool to understand
the system. `ls`, `cat`, `tail`, and `diff` are sufficient for most questions.
The panopticon is convenient. The substrate is self-sufficient.

Rollback is repointing. No migration, no deploy.

The system stays clean over time. Because there is no second source of truth, no
parallel observation channel, no separate metrics service, the substrate does
not accumulate the kind of complexity that erodes long-lived systems.

These are the properties to defend.
