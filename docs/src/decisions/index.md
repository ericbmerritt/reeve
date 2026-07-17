# Architecture Decision Records

One file per significant design choice. ADRs are never edited after the
fact; superseded ADRs get a `Superseded by:` line at the top.

- [ADR 001 — Single ProcessInbound dispatch](./001-single-processInbound-dispatch.md)
- [ADR 002 — Tool-actor trust boundary](./002-tool-actor-trust-boundary.md)
- [ADR 003 — Actor-system internals](./003-actor-system-internals.md)
- [ADR 004 — Multi-provider failover for a single model](./004-multi-provider-failover.md)
- [ADR 005 — `actix::Supervisor` restart is gated on mailbox connectivity, not `stop()` vs `terminate()`](./005-supervisor-restart-is-mailbox-gated.md)
- [ADR 006 — System actors are not agents: `SystemRegistry` and `IdentityType::System`](./006-non-agent-addressing.md)
