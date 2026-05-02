# Reeve — Content Security: The Gatekeeper Model

## Context

Reeve is a local coding tool that runs AI agents on a developer's workstation.
Agents read files, execute tools, fetch content, and receive messages from peer
agents and external processes. They have real authority: they can write files,
run shell commands, create commits, and message other agents.

This document describes how Reeve addresses prompt injection — adversarial
content embedded in legitimate inputs that attempts to hijack an agent's
behavior. It is one of two security documents; the companion document covers
transport-layer authentication via sender signing.

## Problem

Transport authentication proves who sent a message. It does not protect against
adversarial content embedded in legitimate inputs. An authorized agent reads a
source file containing instructions crafted to hijack its behavior. A web page
fetched by a tool call contains text designed to override the agent's task. Tool
output claims special authority. In each case the sender is legitimate but the
content is adversarial.

In a single-agent system this is a nuisance. In a multi-agent system with real
authority — file writes, commits, inter-agent messaging, shell execution — a
successfully injected agent becomes an attack vector against the entire estate.

## Security Goal

Reeve should prevent untrusted or adversarial content from silently acquiring
instruction authority over an agent. Content may inform the agent's work. It
must not be allowed to redirect the agent, expand its authority, override its
task, exfiltrate secrets, induce unsafe tool use, or message other agents
outside the declared task scope.

## Non-Goals

The gatekeeper does not prove that content is true, benign, complete, or
semantically correct. It does not replace transport authentication, capability
enforcement, sandboxing, tool allowlists, or human review of high-authority
actions. It only classifies whether content appears to be attempting to act as
instruction outside its allowed jurisdiction.

## Approach: Pre-Delivery Classification

A small local classifier inspects content before it enters a working agent's
context window. The classifier produces a structured risk signal. The Reeve
runtime combines that signal with the content's source trust tier (established
by the transport layer) and the working agent's capability profile to decide
whether content is passed, flagged, or blocked.

The gatekeeper is not a second agent and not the enforcement boundary. It is a
low-authority classifier. The Reeve runtime enforces policy.

## Gatekeeper Authority

The gatekeeper has no tools, no filesystem write access, no network access, no
agent messaging capability, no memory, and no persistence. It does not modify
content. Its only output is a constrained disposition record consumed by the
Reeve runtime. This makes the gatekeeper a sensor, not an actor.

## Task Scope as Jurisdiction

The gatekeeper's judgment is bounded by the working agent's declared task scope:
what the agent was asked to do and what it is currently doing. Content that
falls within scope is low-signal. Content that attempts to expand scope, claim
new authority, or redirect behavior raises the risk classification.

Gatekeeper effectiveness depends on concrete task scopes. Broad or vague scopes
reduce its ability to distinguish legitimate task-relevant content from
scope-expanding instruction. Reeve encourages narrow, explicit, current task
scopes.

## Classification Contract

The gatekeeper emits a structured Classification (full schema in _Domain Model_
§ Classification). The runtime rejects malformed output. At minimum a
classification carries:

- risk level
- category labels indicating the kind of concern (model-directed instruction,
  authority claim, scope expansion, role override, hidden or encoded
  instruction, and similar)
- confidence
- bounded rationale
- content hash

The runtime treats invalid or missing output as a failure signal, handled
according to the agent's authority — fail-closed for high-authority agents, flag
for low-authority agents.

The gatekeeper does not return a disposition. It returns a classification.

## Runtime Disposition Policy

The runtime decides disposition by applying the active Classifier Policy
(defined in _Domain Model_ § Classifier Policy). The policy maps the
classification and three runtime-supplied inputs to a disposition:

- **Source trust tier** from the transport layer: operator, agent, external,
  untrusted.
- **Agent capability profile** from the authority model: what the receiving
  agent is permitted to do.
- **Content type** — the surface from which content originates, such as a
  repository file, web page, tool output, or peer-agent message. Different
  surfaces carry different baseline suspicion.

The policy produces one of three dispositions:

**Pass** — content enters the working agent's context normally.

**Flag** — content enters the working agent's context with a trust annotation.
The event is logged and surfaced in the panopticon. Flag is an observability and
steering state, not a containment state. Flagged content is still delivered to
the working agent.

**Block** — content does not enter the working agent's context. The agent
receives a sanitized notification that content was withheld. The full content,
classifier output, and source metadata are logged for operator review.

Block is the only disposition that prevents exposure. Specific bias is encoded
in the active Classifier Policy: a common pattern is to bias toward block for
agents with high-authority capabilities (`write_files`, `git_write`,
`execute_shell`, `network_egress`) and permit flag for read-only agents.

## Performance

The gatekeeper is applied at context-promotion boundaries, not on every
filesystem read. Cheap lexical scans, content-type policy, chunking of large
content, and caching of classification results by content hash are all available
to the runtime to keep classification off the hot path. The gatekeeper must not
make ordinary repository navigation feel sluggish.

## Separation of Planes

The classifier's output never enters the working agent's context as instruction.
The two models are on separate trust planes. The classifier sees the content;
the working agent sees only what the runtime delivered, with at most a trust
annotation. For blocked content, the working agent receives only a sanitized
notification. For flagged content, the original content is delivered with a
trust annotation and audit record — this is observability, not containment. The
classifier's rationale, categories, and confidence are runtime metadata; they
are not delivered to the working agent.

## Logging

Disposition events are logged with structured fields: timestamp, agent name,
source, content type, classification ID, disposition policy version, runtime
disposition. The full quarantined content is retained for operator review but is
not casually replayed into other agent contexts.

Classifier model revisions are attribution-only. Past dispositions stand at
their original classifier version; updating the classifier does not trigger
reclassification of historical content. This bounds the scope of any classifier
change to new classifications and prevents audit-log churn or retroactive
disposition shifts.

## Relationship to Transport Security

Transport signing and the gatekeeper are complementary and non-overlapping.
Transport signing handles the provenance question: who sent this, and are they
authorized? The gatekeeper handles the content question: regardless of who sent
it, does this content appear to be attempting to act as instruction outside the
agent's jurisdiction? Both layers are necessary. Neither substitutes for the
other. Authentication changes the suspicion threshold; it does not grant content
authority.

## Limitations

The gatekeeper reduces prompt-injection risk; it does not eliminate it. It
produces false positives and false negatives. Its effectiveness depends on
narrow task scopes, accurate source metadata, conservative defaults, and
deterministic runtime enforcement.

Flagged content is still delivered to the working agent and should not be
treated as contained. Block is the only disposition that prevents exposure. The
gatekeeper is one layer in defense-in-depth: transport authentication, the
persona's capability profile, the blacklist of irreversible actions, and human
review of high-authority artifacts (commits, configuration revisions, memory
writes) all remain in effect.

A general-purpose chat model is the wrong choice for the classifier role. The
initial implementation should use a small local prompt-injection classifier —
narrow, fast, output-constrained — rather than a creative instruction-following
model that could itself be redirected.

## Relevant Expertise

This model draws on adversarial prompt injection research, small local model
inference and deployment, classifier design for instruction detection, and
integration of inference pipelines into Rust runtimes. Experience with
red-teaming LLM systems and content moderation pipeline design is directly
applicable.
