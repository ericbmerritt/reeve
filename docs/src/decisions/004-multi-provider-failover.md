# ADR 004 — Multi-provider failover for a single model

## Context

A model is not tied to one provider. Cheap open-weight models in particular —
GLM-5.2 was the case that surfaced this — are served concurrently by many
providers: the vendor's own API (Z.ai), aggregators (OpenRouter routes ~20
hosts: Novita, GMICloud, DeepInfra, Atlas Cloud, …), and specialist providers
(e.g. energy-priced hosts like Neuralwatt). The _same_ model differs across
providers in price, latency, quantization (fp8 vs fp4, which affects output
quality), context ceiling, and — most importantly for reliability —
independent outages and rate limits.

The operator will want to run one model across multiple provider routes with
**failover**: prefer a primary route (cheapest, or fastest, or fp8) and fall
to a secondary route when the primary errors or rate-limits. This is a
near-term want, not hypothetical.

What exists today:

- Adapters are per-`(route, model)` pairs with IDs like
  `glm-5.2@openrouter` and `glm-5.2@neuralwatt`. Each is a thin translation to
  one provider's endpoint; the OpenAI-compatible ones share `openai_compat`.
- A persona selects a model via `model_preferences` — an ordered list resolved
  **at spawn time** by `resolve_model`, matching on **model ID, not route**.
  It picks the first preferred model that has a registered adapter.
- `AdapterError` already anticipates failover: the `Network`, `Provider` (5xx),
  and `RateLimit` variants are documented as errors on which "the runtime may
  retry or fail-over to another adapter."

What is missing: `model_preferences` cannot express "same model, prefer route A
over route B" (it is route-blind), and nothing performs **per-call** failover
when a route errors. The project overview also currently marks Reeve as "not a
model router." So the capability is anticipated by the error taxonomy but not
built, and one deliberately-deferred piece of a stated non-goal.

## Decision

**Defer runtime multi-provider failover.** It is not built as part of adding
any single model or provider. Adapters stay per-`(route, model)` and a persona
picks one route explicitly.

When it is built (anticipated soon), the intended shape is a **failover
adapter**:

- It wraps an _ordered list_ of concrete `(route, model)` adapters for the
  **same** model and itself implements the `Adapter` trait.
- On a **retryable** `AdapterError` from route N — `Network`, `Provider` (5xx),
  `RateLimit` — it advances to route N+1. **Non-retryable** errors — `Auth`,
  `BadRequest` — surface immediately (they indicate a caller/config fault the
  next route would also reject).
- It composes with the existing registry with no trait or resolution changes:
  it is just another `Adapter` with its own `id()` (e.g. a composite id),
  registered in the adapter slice and resolvable through `model_preferences`
  exactly like a single-route adapter.
- **Observability stays truthful.** Each attempt records its own model call and
  cost against the route actually used, so the audit log and cost meter reflect
  which provider served the turn — not the nominal primary.

Explicitly still out of scope even when failover lands, and remaining "not a
router" non-goals: cross-_model_ failover (e.g. Claude → GLM), load-balancing
or traffic-shaping across providers, and cost-optimizing route selection.
Failover here means _reliability_ for one model, not a routing/optimization
layer.

## Consequences

- Adding a model on a new provider stays a small, mechanical adapter addition
  (the `deepseek/deepseek-r1-0528@openrouter` adapter is the template) with no
  routing entanglement.
- Until failover lands, provider redundancy is **manual**: an operator changes
  a persona's `model_preferences` route by hand if a provider degrades.
- The failover adapter is purely **additive** — no change to the `Adapter`
  trait, the registry, or `model_preferences` semantics — so this deferral
  costs nothing to reverse, and the `AdapterError` retryable/non-retryable
  split is the seam it will build on.
- `model_preferences` being route-blind and resolved at spawn is a known
  limitation. The failover adapter sidesteps it (route ordering lives inside
  the wrapper); a broader route-aware preference model is only needed if route
  selection must vary outside the failover case.
