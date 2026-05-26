# Reeve

Reeve is a runtime that supervises AI coding agents as named, addressable,
supervised actors on a developer's workstation.

Agents are first-class principals. Each one holds a signing keypair, receives
work through a Maildir inbox, and writes its conversation to an append-only
journal the TUI reads directly. Communication between the operator and agents
— and between agents — uses signed JSON envelopes over the filesystem; there
is no socket or RPC for the operator–agent channel.

## Crate map

| Crate             | Role                                               |
| ----------------- | -------------------------------------------------- |
| `reeve-types`     | Shared domain types: envelopes, identities, keys   |
| `reeve-transport` | Envelope signing and verification                  |
| `reeve-adapter`   | Model-provider adapters (Anthropic, etc.)          |
| `reeve-runtime`   | Agent actors, supervisor tree, watcher, daemon     |
| `reeve-tui`       | Terminal UI: panopticon, chat, inspect, quarantine |
| `reeve-cli`       | `reeve` binary entry point                         |

## Further reading

- [Architecture](./architecture.md) — actor topology, filesystem substrate,
  identity model
- [Operations](./operations.md) — install, start the daemon, attach the TUI,
  send a message
- `cargo doc --open` — full API reference
- `docs/decisions/` — Architecture Decision Records
