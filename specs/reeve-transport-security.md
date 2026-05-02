# Reeve — Transport Security Model

## Context

Reeve is a local coding tool that runs AI agents on a developer's workstation.
Each agent is a named, persistent actor with a stable filesystem address. Any
local process — a shell script, another agent, an internal tool — can send a
message to a running agent by writing to its inbox directory. This
addressability is the core feature of the system.

This document describes how Reeve authenticates message senders to prevent
impersonation and unauthorized access. It covers local workstation transport
only. This is one of two security documents; the companion document covers
content-layer injection via the gatekeeper model.

## Security Invariants

The transport model rests on the following invariants:

1. Inbox addressability does not imply authority.
2. Message authority is derived only from runtime-verified sender identity.
3. A valid signature authenticates the sender; it does not by itself authorize
   the requested action.
4. The agent never receives unsigned, unverifiable, or revoked messages in
   context.
5. External senders may provide data but may not directly issue instructions.
6. Message claims about origin, authority, or intent are non-authoritative.
7. Revocation takes effect before delivery, not after agent interpretation.
8. The runtime, not the agent, owns identity verification and trust-tier
   assignment.
9. A failed or untrusted message is never redelivered under its original claimed
   identity.

## Trust Boundary and Assumptions

Reeve's transport model protects agents from unauthenticated local senders and
forged message provenance. It assumes the Reeve runtime, registry storage, and
enrolled private keys are not compromised.

The model does not protect against a fully compromised developer account,
hostile kernel, compromised runtime binary, or stolen private keys. Reeve runs
as the developer's local user. Same-user processes have equivalent filesystem
rights and may attempt denial of service, file tampering, or registry
interference. Signing prevents impersonation. Restrictive permissions and
defensive file handling reduce damage. They do not make the local user boundary
unconditionally strong.

## Delivery Model

Inbound message delivery uses Maildir semantics. Each agent inbox is a directory
with four subdirectories:

```
agents/<name>/
  inbox/
    tmp/         # sender staging area
    new/         # completed messages, awaiting runtime pickup
    cur/         # verified messages durably delivered to agent context
    quarantine/  # failed verification or trust-tier block
```

A sender writes a signed message file into `tmp/`, then renames it atomically
into `new/`. Atomic rename prevents the runtime from observing partial writes
and avoids sender/runtime coordination for completed messages. The runtime
watches `new/` via inotify (Linux) or kqueue (macOS), picks up arriving
messages, verifies them, attempts delivery, and moves them to `cur/` only after
durable context insertion succeeds. Failed messages are moved to `quarantine/`.
The agent never reads the inbox directory directly. The runtime is the only
delivery path into agent context.

## Message State Machine

A message progresses through explicit states:

```
new -> verifying -> verified -> delivering -> delivered (cur/)
                            \                            \
                             -> quarantine                -> quarantine (insertion failure)
```

`cur/` means verified and durably delivered to agent context. A message is not
moved to `cur/` until context insertion completes durably.

The filesystem directory is not the sole source of message state. Until durable
context insertion succeeds, the message file remains in `new/`. Logical
processing state — `verifying`, `verified`, `delivering` — is tracked in runtime
metadata, not by moving the file. Only after successful durable context
insertion does the runtime move the file to `cur/`. This allows a runtime crash
at any pre-delivery state to be recovered by re-reading `new/`.

## Replay Ledger and Delivery Ledger

Two separate ledgers track message state for security and consistency. They are
related but not the same (entity definitions in _Domain Model_ § Replay Ledger
and § Delivery Ledger):

- **Replay ledger** — `sender_id` + `message_id` and nonce records accepted
  within the replay retention window. Prevents external replay. A message
  rejected for any reason still updates the replay ledger so it cannot be
  retried under the same identifiers.
- **Delivery ledger** — `recipient_id` + `message_id` records that have been
  durably inserted into agent context. Prevents duplicate agent interpretation
  across crash recovery.

Conflating these two leads to bugs in which a verified-but-not-delivered message
is rejected after a crash. They are tracked separately.

## At-Least-Once Pickup with Idempotent Delivery

If the runtime crashes mid-delivery, messages remain in `new/` and are processed
on restart, giving at-least-once pickup semantics. Because pickup is
at-least-once, delivery into agent context must be idempotent by `message_id`.

The runtime either records context insertion and the delivery ledger entry
transactionally, or uses a two-phase delivery state that allows interrupted
insertions to resume without duplicate interpretation. A duplicate pickup must
not become a duplicate agent interpretation, and a crash must not cause a
verified message to be marked delivered before it is durably available to the
agent.

## Filesystem Safety

Inbox directories are owned by the Reeve runtime user. `tmp/` and `new/` are
sender-writable; `cur/`, `quarantine/`, and registry storage are runtime-owned
and not expected to be modified by senders. These permissions prevent accidental
modification and reduce attack surface, but they do not protect against a
malicious process running as the same OS user with equivalent filesystem rights.
The honest containment boundary is the developer's user account; the
cryptographic boundary is the signed envelope.

The runtime opens files defensively: no symlink following, no directory
traversal, no hardlink trust, bounded file size, bounded filename length, and
atomic moves only within the same filesystem. Message filenames are treated as
non-authoritative; identity and recipient are taken only from the verified
signed envelope, never from the filename.

## Signed Message Envelope

The Message Envelope is formally defined in _Domain Model_ § Message Envelope.
Every message is a file containing a signed envelope:

```
schema_version
message_id
sender_id
sender_key_id
recipient_id
created_at
nonce
payload_hash
body
signature
```

The signed payload is canonicalized before signing. The runtime verifies the
exact canonical byte representation, not a reserialized or partially parsed
form. Unknown fields are either rejected or included in the canonical form
according to schema version rules. The signature covers the complete envelope
excluding the signature field itself. The specific canonical encoding (canonical
JSON, CBOR, or other) is an open implementation question; see _Domain Model_ §
Open Questions § Canonical Serialization.

`payload_hash` provides verification and indexing for the message body without
requiring the runtime to retain inline body text in registry metadata. The hash
is verified to match the body before delivery.

The default signing algorithm is Ed25519. Implementations may use other
primitives where constraints require it, but Reeve does not adopt JWT, RSA, or
other algorithms by default.

The runtime rejects or quarantines messages with: invalid signatures, revoked or
unknown keys, duplicate `message_id`, duplicate nonces per sender within the
retention window, recipient mismatch with the inbox path from which the message
was picked up, arrival outside the accepted clock skew window, allowed-target
violation, message-kind violation, or capability-scope violation.

## Replay Protection

The runtime retains accepted `message_id` and nonce records in the replay ledger
for at least the maximum accepted message age plus the clock skew allowance.
Messages older than the accepted age window are rejected unconditionally. This
bounds replay ledger storage to a predictable size.

## Identity and Key Model

Identity and Key Record are formally defined in _Domain Model_ § Security Layer.
The schemas relevant to the transport protocol are inlined here for
self-sufficiency.

Every participant holds a keypair. Each key record carries:

```
key_id
identity_id
public_key
status: active | deprecated | revoked
valid_from
valid_until
```

An identity may have one active key and any number of deprecated keys.
Deprecated keys are accepted only for envelopes whose `created_at` falls within
the key's validity window and whose arrival is within the allowed delivery
delay. This is enforced by the runtime against envelope timestamps, not by
anything intrinsic to the key material. A deprecated key cannot authenticate
newly created messages after `valid_until`. Revoked keys verify nothing and are
rejected immediately.

## Internal Senders

Agents within the supervised runtime are provisioned session keypairs at spawn
time. Agent identity is persistent; session keys are short-lived and bound to
the current actor incarnation. Session keys are held in memory only and are
invalidated when the actor exits or the runtime loses its lease. On restart, the
runtime mints a new session key and updates the registry. Two runtimes cannot
simultaneously hold valid session keys for the same agent name.

## External Senders

A process outside the runtime — a shell script, an internal tool — requests an
external identity from the runtime. External identities may only be created
through an operator-tier action or an interactive local approval flow. A process
cannot self-register.

At enrollment, the external identity record (full schema in _Domain Model_ §
Identity) includes:

```
identity_id
display_name
public_key
created_by
created_at
expires_at         # optional
allowed_targets    # which agents may be addressed
allowed_message_kinds
capability_scope
revoked_at         # populated on revocation
```

The runtime returns the private key to the requester only over the local
authenticated enrollment channel. Callers are expected to store it in OS
credential storage or another explicitly configured secret store. Environment
variables, command-line arguments, and world-readable files are not supported
storage mechanisms.

External keypairs are revocable at any time. Revocation is immediate: the
registry entry is marked revoked and subsequent messages signed with the
associated key are quarantined.

Remote identities — processes not running on the local workstation — are out of
scope for this document.

## Session Authentication

Human operators authenticate via the same mechanism. An operator registers an
identity keypair with the runtime once through an interactive enrollment flow.
Subsequently, `reeve attach` and related commands prove identity via
challenge-response against the registered public key. OS keychain on macOS and
XDG credential storage on Linux hold the private key between sessions.

## Trust Tiers on Delivery

Every verified message is assigned a trust tier based on sender identity:

- **Operator** — registered human operator identity
- **Agent** — registered agent session key
- **External** — registered external identity
- **Untrusted** — unsigned, unrecognized, revoked, or failed verification

Untrusted messages are moved to quarantine. They are logged and surfaced to the
operator. The agent never sees them.

Trust tier governs what the agent may do in response. Operator-tier messages may
invoke the full capability profile. Agent-tier messages operate within the
delegated scope defined by team configuration. External-tier messages are
treated as data: they may provide observations, attach context, or request
operator review. They may not directly cause tool use, filesystem mutation,
command execution, delegation, or capability expansion unless an operator
converts them into an explicit instruction.

## Failure Modes and Recovery

Quarantined messages are retained with the full envelope, verification failure
reason, and arrival metadata. The operator may inspect or discard quarantined
messages. The filesystem interface is inspection-and-discard only. Conversion of
quarantined content into a new operator-tier message must happen through the
runtime so the new message is explicitly attributed to the operator and recorded
in the audit log. A failed or untrusted message is never redelivered under its
original claimed identity.

Runtime crash after verification but before durable context insertion leaves the
message recoverable as not-yet-delivered: the file remains in `new/` and is
reprocessed on restart, with delivery deduplicated against the delivery ledger
by `message_id`. Runtime crash after durable context insertion records the
message in the delivery ledger; duplicate pickup is discarded by ledger check.
No message reaches agent context without completing the full verification path.

## Audit Log

The audit log (defined in _Domain Model_ § Audit Log) records security-relevant
transport events durably: identity enrollment, key rotation, revocation,
verification failure, quarantine, operator conversion of quarantined content,
successful delivery, and rejected replay attempts.

Audit records include timestamp, sender identity where known, recipient,
`message_id`, `key_id`, decision, and reason. Audit records are runtime-owned
metadata derived from runtime decisions, not from message claims. The audit log
supports operator trust and post-incident inspection.

## What This Does Not Solve

Transport signing proves provenance under the assumption that sender private
keys and runtime provisioning remain uncompromised. It does not prevent a
legitimately authorized agent from processing adversarial content embedded in a
file, a web page, or tool output. That is the content-layer injection problem,
addressed by the gatekeeper model.

## Relevant Expertise

This model draws on applied public key cryptography (Ed25519 signing), canonical
serialization for signing, Maildir delivery semantics, filesystem event
notification (inotify/kqueue), secure key provisioning and revocation, audit
logging design, and trust hierarchy design. Implementation is in Rust; relevant
crates include `ring` or `ed25519-dalek`, `notify`, and OS keychain integration
via `security-framework` (macOS) and `secret-service` (Linux).
