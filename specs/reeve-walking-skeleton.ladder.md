## Phase 1: Workspace bootstrap

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-05-05 | 2026-05-05 |

Tags: bootstrap

Cargo workspace and crate split that everything else builds on. Six crates: `reeve-types` (envelope schema, identity and key types, agent state types — pure data, no I/O), `reeve-transport` (canonical JSON, sign/verify, maildir state machine, ledgers — depends on `reeve-types`), `reeve-adapter` (Adapter trait, Anthropic adapter — depends on `reeve-types`), `reeve-runtime` (actix supervisor, agent actors, runtime daemon — depends on transport, adapter, types), `reeve-tui` (ratatui frontend, filesystem reader/watcher — depends on types and transport for signing), `reeve-cli` (the `reeve` binary entry point that dispatches subcommands — depends on all). Single-binary distribution per the overview's architecture rationale: Rust plus strict compiler closes the round-trip loop for AI implementers; one binary simplifies installation. CI baseline (cargo test, clippy -D warnings, rustfmt --check) is established here so all subsequent phases inherit it. The architectural commitments to honor across all phases: ed25519 signing, canonical JSON serialization, filesystem-only TUI/runtime communication (no socket, no RPC), actix supervisor for actor hosting, OS keychain for private keys. These are recorded in specs/reeve-roadmap.md (Key Decisions) and are not revisitable in this ladder.

#### Delivers

- Cargo workspace with six crates: reeve-types, reeve-transport, reeve-adapter, reeve-runtime, reeve-tui, reeve-cli
- Single `reeve` binary built from reeve-cli that prints workspace version
- CI configuration running cargo test, clippy, and rustfmt across the workspace

#### Done When

- `cargo build --workspace` succeeds on macOS and Linux without warnings
- `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, and `cargo fmt --check` all exit zero
- `reeve --version` prints the workspace version

#### Depends On

- (none)

## Phase 2: Operator identity

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-05-05 | 2026-05-05 |

Tags: security, identity

First-class identity primitives and operator enrollment, the foundation under everything signed. Per the transport-security spec, ed25519 is the default signing algorithm and there is exactly one registered operator identity per machine per runtime (all attached TUIs sign with the same operator key). Private keys live in the OS keychain only — `security-framework` on macOS, `secret-service` on Linux — and are NEVER persisted on the agent filesystem tree (domain-model invariant 5). The identity registry on disk holds public keys only, as TOML files under `~/.local/share/reeve/identities/<id>.toml`. Identity IDs are UUIDv7 (domain-model § Identifiers) and once retired are never reused (invariant 1). Each key record carries status (active | deprecated | revoked), valid_from, valid_until per transport-security § Identity and Key Model. Identity types per domain-model § Identity are Operator, Agent, External — only Operator type matters in this ladder, but the schema supports all three so later ladders need not migrate. Enrollment is interactive only; processes cannot self-register. All identity operations are recorded in the audit log (which arrives in phase 4); for this phase, log to stdout if the audit log is not yet present.

#### Delivers

- ed25519 keypair primitives and Identity / KeyRecord types in reeve-types
- Identity registry on disk under `~/.local/share/reeve/identities/` (TOML, public keys only)
- OS keychain integration via security-framework (macOS) and secret-service (Linux)
- `reeve identity enroll` interactive subcommand that generates a keypair and registers it
- `reeve identity list` subcommand that prints registered identities

#### Done When

- Given no operator identity exists, when the operator runs `reeve identity enroll`, then a keypair is generated, the public half is written to the registry as a TOML file with status `active`, and the private half is stored in the OS keychain only (never on disk)
- `reeve identity list` prints all registered identities with type, display name, identity ID, and key fingerprint
- Identity IDs are UUIDv7 and are asserted unique across all registered identities (test enforced)
- Attempting `reeve identity enroll` when an operator identity already exists fails with a clear error explaining single-operator-per-machine

#### Depends On

- workspace-bootstrap

## Phase 3: Signed envelope

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-05-05 | 2026-05-05 |

Tags: security, transport, crypto

The signed envelope is the message format every Reeve participant speaks. Schema is fixed by transport-security § Signed Message Envelope and domain-model § Message Envelope: schema_version, message_id, sender_id, sender_key_id, recipient_id, created_at, nonce, payload_hash, body, signature. Canonical JSON serialization (RFC 8785-style): deterministic key ordering (alphabetical), normalized number formatting, no insignificant whitespace. The signature covers the canonical bytes EXCLUDING the signature field; verification operates on the exact canonical byte representation, never on a reserialized form (transport-security § Signed Message Envelope is explicit about this). `payload_hash` is a content hash of the body that lets the runtime carry envelope metadata without inlining the body — the hash is verified against the body before delivery. Schema versioning is a first-class field; v1 is what we are shipping. Unknown fields are rejected per the spec's forward-compatibility rules. The default algorithm is ed25519 (transport-security spec); other algorithms are not adopted by default. This phase produces standalone primitives plus debug CLIs (`reeve envelope sign`, `reeve envelope verify`) used by tests in phase 4 and by the TUI in phase 8.

#### Delivers

- Envelope struct in reeve-types with all fields per transport-security § Signed Message Envelope
- Canonical JSON serializer (deterministic key ordering, normalized numbers, no whitespace) in reeve-transport
- `sign_envelope` and `verify_envelope` functions in reeve-transport using ed25519
- `reeve envelope sign --to <agent> --body <text>` and `reeve envelope verify <file>` debug subcommands

#### Done When

- Round-trip property test: signing then verifying with the matching public key succeeds for any generated envelope
- Tamper test: any single-byte modification of the canonical bytes makes verification fail
- Canonicalization is stable: same input produces the same bytes across runs and across processes (golden test)
- Envelope schema_version is included; an envelope with an unknown schema_version is rejected
- An envelope with an unknown top-level field is rejected

#### Depends On

- operator-identity

## Phase 4: Maildir transport

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-05-05 | 2026-05-05 |

Tags: transport, security

The maildir-based transport that the rest of Reeve composes on. Per-agent inbox layout: `agents/<name>/inbox/{tmp,new,cur,quarantine}` (transport-security § Delivery Model). The maildir state machine is critical: files only move to `cur/` AFTER durable insertion succeeds; logical states (verifying, verified, delivering) are tracked in runtime metadata, not by moving the file (transport-security § Message State Machine). At-least-once pickup with idempotent delivery: a runtime crash mid-delivery leaves the file in `new/` for reprocessing, and the delivery ledger ensures duplicate pickups do not become duplicate agent interpretations (transport-security § At-Least-Once Pickup). TWO LEDGERS, distinct: replay ledger keyed on `sender_id + message_id + nonce` prevents replay within retention; delivery ledger keyed on `recipient_id + message_id` ensures idempotent delivery (transport-security § Replay Ledger and Delivery Ledger; conflating them is explicitly called out as a bug). Filesystem safety per transport-security § Filesystem Safety: no symlink follow, no traversal, no hardlink trust, bounded sizes, atomic moves only within the same filesystem. Filename is non-authoritative; identity is taken only from the verified envelope (invariant 12). Failures move to `quarantine/` with reason recorded; `cur/` rotation is included in this phase — post-integration messages older than retention move to an archive subdir to bound directory size and filesystem churn (per roadmap key decisions). Audit log is JSONL append-only at `~/.local/share/reeve/audit/log.jsonl`; transport events recorded here are `transport.delivered`, `transport.quarantine`, `transport.replay-rejected`, plus identity enrollment events from phase 2 if the audit log has been deferred to here. The runtime is the only reader of `inbox/new/`; agents NEVER read inboxes directly (invariant 6).

#### Delivers

- Per-agent inbox layout (`agents/<name>/inbox/{tmp,new,cur,quarantine}`) provisioned for any registered agent
- `notify`-based watcher actor that consumes `new/` and runs the verification pipeline
- Verification pipeline: parse, schema check, clock skew check, key registry lookup, signature verify, replay ledger check, recipient match
- Replay ledger and delivery ledger as distinct durable JSONL files with retention pruning
- Audit log writer (JSONL append-only) at `~/.local/share/reeve/audit/log.jsonl`
- `cur/` rotation: post-integration messages older than retention move to an archive subdir

#### Done When

- Given a valid signed envelope written to `agents/<name>/inbox/new/`, when the watcher picks it up, then it is moved to `cur/`, the delivery ledger has an entry by `recipient_id + message_id`, and the audit log has a `transport.delivered` event
- Given an envelope with a tampered body, then it is moved to `quarantine/` and the audit log has `transport.quarantine` with reason `signature_invalid`
- Given a duplicate `message_id` from the same `sender_id` within retention, then it is rejected to `quarantine/` with reason `replay`
- Given an envelope whose recipient does not match the inbox path, then it is rejected with reason `recipient_mismatch`
- Replay ledger and delivery ledger are distinct on-disk artifacts with distinct schemas (asserted by integration test)
- On runtime restart, in-flight messages in `new/` are reprocessed and not re-delivered (delivery ledger deduplicates)
- Filenames are non-authoritative: an envelope whose filename does not match its envelope contents is delivered or rejected based on the envelope, not the filename

#### Depends On

- signed-envelope

## Phase 5: Claude adapter

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-05-05 | 2026-05-05 |

Tags: adapter, model

The model adapter framework and first concrete adapter. Per domain-model § Adapter, an adapter is the (route, model) translation: it takes Reeve's internal protocol and produces requests in the wire format the route expects for that specific model, then translates responses back. This phase delivers the trait, the `anthropic-direct` route, and the `claude-opus-4-7` adapter. The Adapter trait exposes `call(messages, tools, params) -> Response { content, tool_calls, finish_reason, tokens, cost, latency }` and declares the capabilities the (route, model) pair actually delivers (domain-model § Adapter; routes can expose less than the model supports). Cost calculation is from token counts multiplied by adapter-declared per-token rates. Credentials come from the OS keychain (entry name `reeve-anthropic-api-key`) — not environment variables, not files, per the security model. Failover and retry beyond simple network errors are NOT in scope here (later ladders, when adapters multiply); this phase surfaces failures with structured reasons and lets the caller decide. Routing services like OpenRouter are routes, not transparent layers (domain-model gotcha); we do not abstract over them. This phase is parallelizable with phases 2, 3, 4 — it depends only on the workspace from phase 1 and is wired into the runtime in phase 6.

#### Delivers

- Adapter trait in reeve-adapter
- anthropic-direct route configuration
- claude-opus-4-7 adapter implementing the trait, with declared capabilities
- Credential reading from OS keychain entry `reeve-anthropic-api-key`
- Cost calculation from token counts and declared per-token rates
- `reeve adapter test --prompt <text>` standalone subcommand

#### Done When

- Given a valid Anthropic API key in the keychain, when the operator runs `reeve adapter test --prompt "hello"`, then the response, token counts (input/output/cached), latency, and cost are printed
- Adapter declares its capability set (tool calling, vision, reasoning, structured output, parallel tool calls, prompt caching) and an integration test asserts the declared capabilities match the route's actual behavior
- Cost calculation is unit-tested against known token counts and per-token rates
- On adapter failure (network, auth, rate limit, model error), the error is surfaced with a structured reason and the call records what it can

#### Depends On

- workspace-bootstrap

## Phase 6: Runtime daemon

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-05-06 | 2026-05-06 |

Tags: runtime

The long-lived background process that hosts the supervisor tree and the runtime's owned state. Per domain-model § Runtime, there is exactly one runtime per machine per operator at any time — enforced via a lockfile at `~/.local/state/reeve/runtime.lock` (a second `reeve daemon start` while one runs fails with a clear error). PID file at `~/.local/state/reeve/runtime.pid`. The actix supervisor tree is established here; in this phase no agents are spawned yet, but the supervisor and its restart-on-failure semantics are tested with an injected-panic actor. `runtime/heartbeat` file is touched every 1 second — this is the TUI's liveness check (phase 8 reads its mtime; older than 2x interval means stale). The audit log writer from phase 4 is wired in as an actor here. The filesystem watcher from phase 4 is wired in as an actor and consumes the verification pipeline. Owned state per domain-model § Runtime: agent registry, identity registry, replay ledger, delivery ledger, audit log writer, filesystem watchers, adapter registry, route registry, model client pool. In this phase the agent registry is empty; phase 7 populates it. Crash-restart of individual actors does not exit the process (let-it-crash per overview architecture); a panicking actor is restarted by its supervisor without quiescing the runtime.

#### Delivers

- `reeve daemon start | stop | status` lifecycle subcommands
- PID file at `~/.local/state/reeve/runtime.pid` and lockfile preventing dual instances
- Heartbeat file at `~/.local/state/reeve/runtime/heartbeat` touched every second
- actix supervisor tree (no agents yet) with restart-on-failure
- Wiring: audit log writer actor, filesystem watcher actor consuming the phase 4 pipeline

#### Done When

- `reeve daemon start` returns and the daemon runs in the background
- `reeve daemon status` prints alive plus heartbeat-fresh when running, and `no runtime` when stopped or stale (heartbeat older than 2x interval)
- A second `reeve daemon start` while one is running fails with a clear `already running, PID N` error
- `reeve daemon stop` flushes the audit log, releases the lock, and exits cleanly
- An injected panic in any non-supervisor actor restarts only that actor; the daemon process keeps running (verified by test)

#### Depends On

- maildir-transport
- claude-adapter

## Phase 7: Lead agent

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-05-06 | 2026-05-06 |

Tags: runtime, agent

The first running agent — a single lead spawned automatically when the daemon starts. Per domain-model § Agent, an agent is an actix actor with its own mailbox, its own per-agent state, and an immutable configuration snapshot taken at spawn (persona name and version, skill names and versions, capability profile name and version, classifier policy name and version, memory generation, resolved model/route/adapter ID). The persona TOML is loaded from `~/.local/share/reeve/personas/lead/config.toml`; the team TOML from `~/.local/share/reeve/teams/default.toml`. The team config maps the role label `lead` to the lead persona — the daemon spawns one running agent per (persona, count) combination on startup. Conversation thread is durable, append-only, JSON Lines at `agents/lead/log/conversation.jsonl` (domain-model § Conversation Thread); entry types are inbound, outbound, model_call, tool_invocation, authority_decision, system. Status file at `agents/lead/status` and cost meter file at `agents/lead/cost` are atomic-rename updates (write-tmp-then-rename) so concurrent readers (the TUI in phase 8) never see partial writes. Model resolution at spawn (domain-model § Model Resolution): walk the persona's preference list, find adapters that serve each preferred model on routes with working credentials, filter by required capabilities, select the first that satisfies all constraints. The resolved (model, route, adapter ID, version) triple is recorded in the agent's spawn snapshot. CRITICAL: capability profile fields are PARSED from configs but NOT enforced in this ladder (enforcement is ladder 3 / `reeve-authority`). Memory subscriptions, skills, classifier policy are similarly parsed but not used. A conversation entry is individually atomic: a runtime crash mid-write produces either a complete prior line or no new line, never a partial line — verified by injected-crash test.

Curator simplicity: in this phase, the curator is a degenerate appending loop. Incoming messages append to the conversation thread (chat-style); the agent calls the adapter with the full thread; the response appends back. No structured working context, no mechanical compression, no satellites, no separate bus tape, no small-model fallback. The full architecture — curator, brainstem, cognition-as-function, contributing satellites (memory composer first), bus tape, structured working context — lives in `specs/reeve-actor-interior.md` (which uses "actor" for what this ladder calls "agent" — same concept; see `specs/reeve-positioning.md` § Conceptual Model). Later ladders grow the simple v1 toward the full architecture. Persona is loaded as static configuration here; the live-actor persona of `specs/reeve-persona-actor.md` is a later-ladder concern. Versioned on-disk artifacts per `specs/reeve-disk-substrate.md` are also future work; the lead persona TOML loaded here is not yet in a versioned directory.

#### Delivers

- Persona TOML loader and Team TOML loader
- Default lead persona config and default team config installed if absent
- Agent actix actor with mailbox, conversation thread, status file, and cost meter file
- Atomic-rename helpers for status and cost files
- Model resolution at spawn, recorded in the agent's spawn snapshot
- Automatic spawn of the lead on daemon startup per the default team config

#### Done When

- Given the daemon starts, then the lead agent is spawned automatically per the default team config and `agents/lead/status` reads `idle`
- Given a signed envelope arrives in `agents/lead/inbox/cur/` (delivered by phase 4), when the lead receives it, then status transitions `idle -> working`, the adapter is called, the response and the model_call are appended to `conversation.jsonl`, `cost` is updated atomically, and status returns to `idle`
- The agent's spawn snapshot (persona version, capability profile reference, resolved adapter ID and version) is recorded in `agents/lead/agent.toml` and is asserted unchanged across the agent's lifetime
- Conversation entries are individually atomic: an injected crash mid-write produces either a complete prior line or no new line (never a partial line)
- Status and cost files always read as well-formed (no partial-write window) under concurrent reader pressure (test-enforced)

#### Depends On

- runtime-daemon

## Phase 8: TUI

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| ✅ complete     | 2026-05-06 | 2026-05-07 |

Tags: tui

The terminal UI as a separate process whose entire interaction with the runtime goes through the filesystem — no socket, no RPC, no REST. This is the architectural commitment recorded in specs/reeve-roadmap.md (Key Decisions): the TUI is a privileged file reader plus a watcher plus a signed-envelope writer; the runtime maintains canonical state on disk; everything composes through the same primitives the maildir transport already establishes. Implementation: ratatui frontend in reeve-tui; filesystem reader for `agents/lead/status`, `agents/lead/log/conversation.jsonl`, `agents/lead/cost`; `notify`-based watcher across the agent dir with ~250ms render debounce per tui-design § Update cadence; lead chat screen per tui-screens § Lead chat (read that spec for layout — title bar with sigils, color, and microcopy, conversation pane, input box). Sigils plus color contract per tui-design § Cross-screen conventions; NO_COLOR=1 fallback verified; 80x24 minimum verified. Submission path for messages: TUI signs an envelope with the operator key fetched from the OS keychain, writes it to `agents/lead/inbox/tmp/<id>.json`, then atomic-renames it into `agents/lead/inbox/new/<id>.json` — exactly the path any external sender would take, exercising the universal-transport claim from day one. Liveness check at startup: read `runtime/heartbeat` mtime; if older than 2x interval, render `no runtime found, run reeve daemon start` and exit. Detach and reattach: `q` exits the TUI process; the runtime keeps running and the lead's state stays on disk; re-launching the TUI reads canonical state and resumes — no stored TUI-side state to reconcile.

#### Delivers

- ratatui frontend in reeve-tui
- Filesystem reader for status, conversation thread, and cost
- `notify`-based watcher with ~250ms render debounce
- Lead chat screen with sigils, color contract, and NO_COLOR fallback
- Submission path: sign with operator key, atomic-rename envelope into `agents/lead/inbox/new/`
- Heartbeat staleness check on startup
- `reeve attach` subcommand

#### Done When

- Given a running daemon, when the operator runs `reeve attach`, then the TUI opens, reads the conversation thread, and renders the lead chat screen
- Given the operator types a message and submits, when the daemon delivers it, then the response appears in the conversation pane within ~250ms of `conversation.jsonl` being appended
- Given the operator quits the TUI with `q`, when they re-launch the TUI, then the same conversation history renders and the agent is still alive (status reads `idle` or `working`, never absent)
- Given no daemon is running, when the operator opens the TUI, then it shows `no runtime found, run reeve daemon start` (heartbeat older than 2x interval)
- With `NO_COLOR=1` and `stty rows 24 cols 80`, the chat screen is fully readable via sigils and microcopy
- Two TUIs attached at the same time both render the same canonical state and both can submit messages without coordination (verified by smoke test)

#### Depends On

- lead-agent

## Phase 9: First-run polish

| Status         | Started    | Completed  |
| -------------- | ---------- | ---------- |
| 🟡 in-progress  | 2026-05-07 |            |

Tags: polish, ux

Bootstrap the operator's first-run experience and tie loose ends so the demo holds together. `reeve` (no subcommand) is the entry point: it detects daemon state and operator identity, walks the user through enrollment if needed, starts the daemon if it is not running, and launches the TUI — one command from clean install to chat with lead. Default persona/team configs are installed at first run if absent (shipped as embedded resources or written from defaults). Adapter and transport failures must surface as system events in the conversation thread — never silent — so the operator can see what happened without reading logs. README plus a short architectural one-pager so a new contributor (or a forge agent in a later ladder) has orientation. Smoke-test script in `scripts/smoke-test.sh` runs the whole flow from clean install to a model response and exits 0; this becomes the regression check for ladder 1's promise. This phase is thin by design: the substantive work is upstream; this is the first-run UX and the harness that lets us trust the demo.

#### Delivers

- `reeve` (no subcommand) entry point that detects state and walks the user through whatever is missing
- Default persona and team configs installed at first run if absent
- System-event surfacing for adapter and transport failures in the conversation thread
- README and architectural one-pager
- `scripts/smoke-test.sh` that runs the clean-install demo end-to-end

#### Done When

- Given a clean machine (no Reeve state), when the operator runs `reeve`, then enrollment runs, the daemon starts, the lead spawns with a default persona, and the TUI opens — all in one flow
- Given an existing install with a running daemon, when the operator runs `reeve`, then the TUI attaches without re-enrollment or re-start
- Adapter or transport failures appear in the conversation pane as system events with a reason; never silent
- `scripts/smoke-test.sh` runs end-to-end from clean install to a model response and exits 0

#### Depends On

- tui

## Notes

### Non-goals for this ladder

Deferred to later ladders, intentionally:

- Capability profile, blacklist, and classifier *enforcement* — fields are parsed; enforcement arrives in `reeve-authority` (ladder 3) and `reeve-gatekeeper` (ladder 4).
- Cost ceiling enforcement — costs are recorded per call; ceiling refusal arrives in `reeve-authority` (ladder 3).
- Failover across adapters — single route, single adapter; failover arrives when adapters multiply.
- Memory subsystem — no project / persona / operator memory in this ladder. Arrives in `reeve-memory` (ladder 5).
- Skills — no skill bundles or activation. Arrives in `reeve-skills-versioning` (ladder 6).
- Configuration revision flow — configs are loaded once at daemon start. Revision tracking and the configuration revision review screen arrive in `reeve-skills-versioning` (ladder 6).
- Audit log ring buffer — file is written; the in-memory ring buffer that feeds the panopticon arrives with the panopticon in `reeve-multi-agent` (ladder 2).
- Per-agent git worktrees — open question in domain-model § Open Questions; defer until multiple agents exist in `reeve-multi-agent` (ladder 2).
- Subordinate spawning, panopticon, per-agent inspect, quarantine review screen — all arrive in `reeve-multi-agent` (ladder 2).

### Architectural commitments

Fixed for this ladder, per `specs/reeve-roadmap.md` Key Decisions:

- ed25519 signing from day one
- Canonical JSON for envelopes (RFC 8785-style)
- Filesystem-only TUI / runtime communication: no socket, no RPC, no REST
- actix supervisor tree for actor hosting
- OS keychain for private keys; never on disk in the agent filesystem tree
- Maildir state machine: files only move to `cur/` after durable insertion
- Two distinct ledgers: replay (sender + message ID + nonce) and delivery (recipient + message ID)
- `cur/` rotation from day one to bound directory size

### Reading order for implementing agents

Before picking up any phase, skim `specs/reeve-overview.md` for product context, `specs/reeve-roadmap.md` for sequence and key decisions, and the sibling spec(s) most relevant to the phase. The transport-security and domain-model specs are load-bearing for phases 2, 3, 4, 6, 7. The TUI design and TUI screens specs are load-bearing for phase 8.
