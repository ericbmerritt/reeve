# Reeve — Runtime Checkpointing and Restoration

## Context

Reeve is designed as a restartable local agent runtime whose authoritative
state exists on the filesystem. Rather than treating agents as ephemeral chat
sessions, Reeve treats them as durable actors with persistent mailboxes,
journals, memory, supervision state, and identity.

This document specifies a checkpointing and restoration model for Reeve that
enables:

- Full runtime restoration
- Actor-level restoration and forking
- Immutable historical snapshots
- Durable restart semantics
- Portable operation without requiring specialized filesystems such as ZFS or
  btrfs

The goal is not to rewind the external world. The goal is to restore Reeve's
own continuity and internal causal state.

## Core Thesis

Reeve should not attempt to fully event-source the universe.

Instead:

> Reeve operates on ordinary filesystem state while maintaining immutable
> checkpoint snapshots of Reeve-owned runtime data.

Rollback is therefore implemented as:

1. Restore checkpointed runtime state
2. Restart Reeve from that restored state

Not:

1. Replay all events from origin
2. Reconstruct the universe from an event stream

This preserves the Unix-native design of Reeve while still enabling
restoration, replay, branching, and experimentation.

## Scope of Checkpointing

Checkpointing covers Reeve-owned runtime state only.

It does not attempt to undo or rewind:

- Git repository history
- Shell command side effects
- Database writes
- External APIs
- Network calls
- Package installations
- Tool execution side effects
- Operating system mutations
- Remote systems

This boundary is intentional. Reeve restores its own interpretation of the
world, not the world itself.

## Reeve-Owned Runtime State

The following categories are considered checkpointable runtime state:

```
.reeve/
  actors/
  mail/
  journals/
  memory/
  registry/
  approvals/
  events/
  cost/
```

These directories represent:

- Actor state
- Delivery state
- Runtime journals
- Memory integration
- Supervision topology
- Approval state
- Runtime event history
- Accounting and usage tracking

All Reeve-owned mutable state must exist under known runtime roots. This is
the load-bearing discipline: checkpoint completeness depends on every
subsystem putting its state where the checkpoint scanner expects to find it.

## Architectural Model

### Live State

Reeve operates directly on ordinary filesystem structures.

```
.reeve/live/
```

The runtime continuously mutates this live state during operation.

### Immutable Snapshot Store

Checkpointed state is stored separately in an immutable content-addressed
snapshot store.

```
.reeve/store/
  objects/
  manifests/
```

Objects are immutable content-addressed blobs. Manifests describe filesystem
trees at a checkpoint boundary.

## Snapshot Model

A checkpoint is represented as:

```
checkpoint_id
  path -> content_hash
  path -> file_kind
  path -> permissions
  path -> deleted?
```

Example:

```
actors/forge/state.json       -> sha256:aaa
mail/alice/inbox/new/msg-1    -> sha256:bbb
memory/index.sqlite           -> sha256:ccc
registry/actors.json          -> sha256:ddd
```

Only changed objects are written between checkpoints. Unchanged files are
shared structurally between snapshots. This provides copy-on-write-like
behavior without requiring kernel-level filesystem support.

## Checkpoint Semantics

A checkpoint is a coherent restoration boundary.

Checkpoint creation protocol:

1. Pause actor scheduling
2. Drain pending writes
3. Flush critical state
4. Build snapshot manifest
5. Persist referenced objects
6. Atomically publish checkpoint manifest
7. Resume runtime

The checkpoint manifest becomes the authoritative reconstruction boundary.

## Restoration Model

Restoration is intentionally restart-oriented.

1. Stop Reeve
2. Restore filesystem state from checkpoint
3. Start Reeve
4. Reeve resumes from restored runtime state

The runtime is therefore restartable from checkpointed filesystem state. This
avoids requiring full runtime replay.

## Event Log Relationship

Reeve may still maintain an append-only event log. However:

> The event log is not the sole source of truth.

The event log exists primarily for:

- Audit
- Provenance
- Diagnostics
- Replay support
- Timeline inspection
- Human debugging

Filesystem state remains authoritative for live runtime continuity. This
preserves the Unix-native design.

## Hierarchical Checkpoint Scopes

Checkpointing supports multiple scopes.

### Runtime Scope

Captures all Reeve runtime state.

```
scope: runtime
```

Use cases:

- Full daemon restoration
- Runtime rollback
- Runtime branching
- Experimental forks

### Actor Scope

Captures a single actor's continuity state.

```
scope: actor:<id>
```

An actor scope includes:

```
actors/<id>/
mail/<id>/
journals/<id>/
memory/actors/<id>/
approvals/<id>/
cost/<id>/
```

This represents the actor's:

- Local memory
- Mailbox state
- Journal continuity
- Approval context
- Runtime accounting
- Internal execution state

### Mailbox Scope

Captures mailbox delivery state.

```
scope: mailbox:<id>
```

### Memory Scope

Captures a memory namespace.

```
scope: memory:<name>
```

## Consistency Groups

Individual files may be stored independently. However, restoration should
normally occur against named consistency groups.

Example:

```
actor_state:
  actors/<id>/
  journals/<id>/
mailbox:
  mail/<id>/
  delivery-ledger/<id>/
memory:
  memory/
  memory-ledger/
```

This prevents restoring causally inconsistent partial state.

## Partial Restore Semantics

Partial restoration introduces causal complexity.

Example:

- Actor A sends a message to Actor B
- Actor B processes the message
- Actor A is rolled back

Actor B may now contain memory of events Actor A no longer remembers. This is
acceptable if restoration semantics are explicit.

## Restore Modes

### Local Restore

```
restore actor:<id>
```

Only restores the target actor. External consequences remain facts. This is
fast but causally imperfect.

### Causal Restore

```
restore actor:<id> --causal
```

Attempts to restore dependent runtime state. This is more correct but
substantially more complex, and partial-restore causality is the failure mode
this mode is most likely to produce in practice. Treat as a research target
rather than a ship-day feature.

### Fork Restore

```
fork actor:<id> from checkpoint
```

Creates a new actor from prior state. The original actor remains unchanged.
This is the safest and most useful actor-level restoration mode.

## Forking

Forking is a first-class capability.

Example:

```
reeve fork --scope actor:forge-a \
  --checkpoint chk_123 \
  --as forge-b
```

This allows:

- Alternate execution strategies
- Safe experimentation
- Retry without destructive mutation
- Divergent agent reasoning paths
- Parallel exploration

Forking is often more useful than destructive rollback.

A caveat the operator must understand: external side effects do not fork.
If a forked agent calls `cargo install foo`, both the parent and the fork
observe `foo` installed afterward. Forks isolate Reeve-owned state; they do
not isolate the world.

## Snapshot Provider Abstraction

Reeve should not implement its own filesystem. Instead, it exposes a narrow
snapshot abstraction.

Example:

```rust
trait SnapshotProvider {
    fn checkpoint(&self, scope: Scope) -> SnapshotId;
    fn restore(&self, snapshot: SnapshotId) -> Result<()>;
    fn fork(&self, snapshot: SnapshotId) -> Result<RuntimeId>;
}
```

This allows multiple implementations.

## Portable Snapshot Backends

The default implementation should be portable.

Candidate approaches:

- Content-addressed object store
- Incremental snapshotting
- Immutable manifests
- Deduplicated blobs

Potential implementation substrates:

- `titor`
- `rustic_core`
- Custom lightweight CAS layer

The runtime should not semantically depend on any particular backend.

## Optional Native Filesystem Acceleration

Advanced filesystems may optionally accelerate checkpointing.

Examples:

- btrfs
- ZFS
- APFS

These may provide:

- Cheap snapshots
- Copy-on-write clones
- Fast forks
- Efficient rollback

However:

> Native snapshot filesystems are optional accelerators, not architectural
> requirements.

Reeve must remain portable.

## Design Principles

### Reeve Is Restartable

Reeve should always be capable of reconstructing itself from persisted
runtime state.

### Filesystem State Is Real State

Live runtime state exists as ordinary filesystem structures.

### Snapshots Are Immutable

Checkpointed state must never mutate.

### Runtime State Is Local

Reeve restores only Reeve-owned continuity.

### External Side Effects Are Facts

Tool execution and external mutations are not reversible.

### Forking Is Preferable To Erasure

Branching continuity is safer and more auditable than destructive rollback.

## Sequencing

Checkpointing depends on the subsystems it checkpoints. The state surface
listed under `## Reeve-Owned Runtime State` includes categories that do not
yet exist:

- `memory/` lands with the `reeve-memory` ladder.
- `approvals/` lands with the `reeve-authority` ladder.
- `events/` is partial today (audit log) and gains shape as the gatekeeper
  and memory ladders generate richer event sources.

Building the snapshot store and provider before those subsystems exist would
mean re-deriving the consistency-group shapes each time a ladder lands.

The discipline that survives early — and that every ladder must respect — is
the rule that **all Reeve-owned mutable state lives under documented runtime
roots**. If the runtime holds that invariant from day one, the snapshot
provider implementation is a contained engineering project later. If the
runtime drifts (state scattered across roots, opaque per-subsystem caches,
mutable in-memory authoritative state with no on-disk reflection), then
checkpointing becomes an archaeology dig.

The expected build order:

1. Hold the state-root discipline through the existing ladders.
2. Land `reeve-authority` and `reeve-memory` per the roadmap.
3. Implement the snapshot provider and the `runtime` and `actor` scopes once
   memory exists — fork-an-actor is the load-bearing demo and it has limited
   meaning without durable mental state to fork.
4. Defer `--causal` restore. Local restore plus fork covers the realistic
   operator need; causal restore is a research target.

## Open Questions

- **Pause-and-flush cadence.** The checkpoint protocol assumes a quiesce
  point. Phase-6 actors are message-driven and tend to drain quickly between
  ticks; later subsystems (memory composer, gatekeeper) may run longer-lived
  background work that resists clean quiesce. The protocol may need a
  per-subsystem "checkpointable" handshake rather than a global pause.
- **Identity continuity across forks.** A forked agent inherits state but
  not identity — its `identity_id` must be fresh so envelopes remain
  unique. The actor scope's `actors/<id>/state.json` may carry the old id;
  fork must rewrite it. Unresolved: whether forks carry a `forked_from`
  attribution in the identity record.
- **Snapshot store retention.** The doc says objects are immutable;
  unaddressed is when (or whether) old objects get garbage-collected. A
  retention policy is operator-facing surface and may need a separate spec.
- **Tape interaction.** Reeve's bus tape (per the disk-substrate spec) is
  already a content-addressed append-only log of every state mutation.
  Whether the checkpoint store reuses the tape's object encoding, or sits
  beside it as a parallel store, is an integration question worth answering
  before implementation.
