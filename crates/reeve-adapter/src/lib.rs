//! Reeve model adapters.
//!
//! Per-(route, model) translation between Reeve's internal protocol and a
//! specific provider API. Each adapter declares the capabilities the
//! (route, model) pair actually delivers. See
//! `specs/reeve-domain-model.md` § Adapter for the contract.
//!
//! # Protocol surface
//!
//! The central abstraction is the [`Adapter`] trait. Callers assemble a slice
//! of [`Message`]s, a slice of [`Tool`]s, and a [`Params`] struct, then invoke
//! [`Adapter::call`]. On success they receive a [`Response`]; on failure an
//! [`AdapterError`] with a structured reason.
//!
//! The types in this crate are Reeve's *internal* representation. Concrete
//! adapter implementations translate between these types and the wire format of
//! a specific provider.
//!
//! # Adapters
//!
//! - [`ClaudeOpus47`] — `claude-opus-4-7@anthropic-direct`: Anthropic's
//!   Messages API with rustls TLS, no system OpenSSL dependency.
//! - [`DeepSeekR1OpenRouter`] — `deepseek/deepseek-r1-0528@openrouter`:
//!   `DeepSeek` R1 0528 via `OpenRouter`'s OpenAI-compatible endpoint.
//! - [`Glm52OpenRouter`] — `z-ai/glm-5.2@openrouter`: Zhipu/Z.ai GLM-5.2 via
//!   `OpenRouter`, preferring fp8 hosts (Novita, `GMICloud`).
//!
//! The OpenRouter-routed adapters share an internal `openrouter` module for the
//! client policy, endpoint, and status→error mapping; only model ID,
//! capabilities, cost, and provider preference differ per adapter.

mod anthropic;
mod claude_opus_47;
mod deepseek_r1_openrouter;
mod glm_5_2_openrouter;
mod openai_compat;
mod openrouter;

pub use claude_opus_47::ClaudeOpus47;
pub use deepseek_r1_openrouter::DeepSeekR1OpenRouter;
pub use glm_5_2_openrouter::Glm52OpenRouter;

use std::collections::HashSet;
use std::fmt;

// ── Capabilities ──────────────────────────────────────────────────────────────

/// An individual capability that a (route, model) adapter pair may expose.
///
/// Used in [`Capabilities`]. The set is `#[non_exhaustive]`; future variants
/// may be added as providers surface new features.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum Capability {
    /// The adapter exposes provider tool/function calling.
    ToolCalling,
    /// The adapter accepts vision/image inputs.
    Vision,
    /// The adapter exposes the provider's reasoning/thinking mode.
    Reasoning,
    /// The adapter exposes structured-output / JSON mode.
    StructuredOutput,
    /// The adapter exposes parallel tool-call dispatch.
    ///
    /// Implies [`Capability::ToolCalling`] — adapters MUST NOT declare
    /// `ParallelToolCalls` without also declaring `ToolCalling`.
    ParallelToolCalls,
    /// The adapter exposes prompt caching.
    PromptCaching,
}

/// The capability set that a specific (route, model) adapter pair actually
/// delivers.
///
/// An adapter may declare fewer capabilities than the underlying model
/// supports — routes can restrict what they expose.
///
/// Construct with [`Capabilities::new()`] (empty) or
/// [`Capabilities::default()`] (also empty), then chain [`Capabilities::with`]
/// calls to add capabilities:
///
/// ```rust
/// # use reeve_adapter::{Capabilities, Capability};
/// let caps = Capabilities::new()
///     .with(Capability::ToolCalling)
///     .with(Capability::Vision);
/// assert!(caps.contains(Capability::ToolCalling));
/// assert!(!caps.contains(Capability::Reasoning));
/// ```
///
/// Implication invariants are enforced at construction. [`Capabilities::with`]
/// panics (both debug and release) if an invariant is violated. Use
/// [`Capabilities::try_with`] for fallible callers that need a `Result`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    set: HashSet<Capability>,
}

/// Check implication invariants for a capability set, returning the first
/// missing prerequisite found, or `None` if the set is well-formed.
///
/// Currently enforced: `ParallelToolCalls` ⇒ `ToolCalling`.
///
/// Using `Option<Capability>` rather than `bool` ensures that if a second
/// invariant lands, `try_with` automatically reports the correct missing
/// capability rather than hard-coding `Capability::ToolCalling`.
fn first_violation(set: &HashSet<Capability>) -> Option<Capability> {
    if set.contains(&Capability::ParallelToolCalls) && !set.contains(&Capability::ToolCalling) {
        return Some(Capability::ToolCalling);
    }
    None
}

/// Error returned when a [`Capabilities`] implication invariant is violated.
///
/// Currently: [`Capability::ParallelToolCalls`] requires
/// [`Capability::ToolCalling`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityViolation {
    /// The capability that was being added.
    pub adding: Capability,
    /// The capability that is required but absent.
    pub missing: Capability,
}

impl fmt::Display for CapabilityViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "capability {:?} requires {:?} to be present",
            self.adding, self.missing
        )
    }
}

impl std::error::Error for CapabilityViolation {}

impl Capabilities {
    /// Construct an empty capability set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            set: HashSet::new(),
        }
    }

    /// Add a capability. Returns the new set for ergonomic chaining.
    ///
    /// Panics (in both debug and release builds) if the resulting set would
    /// violate an implication invariant (e.g., declaring
    /// [`Capability::ParallelToolCalls`] without [`Capability::ToolCalling`]).
    /// For trusted call sites (e.g., adapter constructors with compile-time
    /// known capability sets) this is appropriate. New code that constructs
    /// capability sets from runtime input should prefer [`Capabilities::try_with`].
    #[must_use]
    pub fn with(self, cap: Capability) -> Self {
        match self.try_with(cap) {
            Ok(caps) => caps,
            Err(violation) => {
                panic!("Capabilities::with: implication invariant violated: {violation}")
            }
        }
    }

    /// Add a capability, returning `Err` if the resulting set would violate an
    /// implication invariant (e.g., [`Capability::ParallelToolCalls`] requires
    /// [`Capability::ToolCalling`]).
    ///
    /// Prefer this over [`Capabilities::with`] when constructing capability
    /// sets from runtime input.
    pub fn try_with(mut self, cap: Capability) -> Result<Self, CapabilityViolation> {
        let mut candidate = self.set.clone();
        candidate.insert(cap);
        if let Some(missing) = first_violation(&candidate) {
            return Err(CapabilityViolation {
                adding: cap,
                missing,
            });
        }
        self.set = candidate;
        Ok(self)
    }

    /// Returns `true` if the set respects all declared implication invariants.
    ///
    /// Currently enforced: `ParallelToolCalls` ⇒ `ToolCalling`.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        first_violation(&self.set).is_none()
    }

    /// Returns `true` if the given [`Capability`] is in this set.
    #[must_use]
    pub fn contains(&self, cap: Capability) -> bool {
        self.set.contains(&cap)
    }
}

// ── Message types ────────────────────────────────────────────────────────────

/// A single message (a "turn") in a conversation, carrying a [`Role`] and one
/// or more [`MessageContent`] blocks.
///
/// A turn may carry multiple blocks: an assistant turn can mix text and
/// tool-use blocks; a user turn can carry one or more tool-result blocks. The
/// adapter is responsible for serializing the block array into the provider's
/// wire format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// Authorship — see [`Role`] for valid values and the typical
    /// system→user→assistant turn order.
    pub role: Role,
    /// One or more content blocks for this turn — see [`MessageContent`].
    pub content: Vec<MessageContent>,
}

/// The author of a [`Message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A system-level instruction, typically the persona's system prompt.
    System,
    /// Content originating from the human operator or an inbound agent
    /// message.
    User,
    /// Content produced by the model in a prior turn.
    Assistant,
}

/// One block of content in a [`Message`].
///
/// A turn carries one or more of these in `Message.content`. This enum is
/// `#[non_exhaustive]`: future variants (images, reasoning blocks) may land
/// without a breaking change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MessageContent {
    /// A plain UTF-8 text block.
    Text(String),
    /// A tool invocation requested by the assistant in a prior turn. Sent back
    /// to the provider verbatim as part of the assistant turn so it stays
    /// paired with the matching `ToolResult` in the next user turn.
    ToolUse {
        /// Provider-assigned identifier for this invocation; echoed back in
        /// the matching [`MessageContent::ToolResult`] block.
        id: String,
        /// Tool name the model invoked.
        name: String,
        /// Arguments the model supplied, conforming to the tool's input
        /// schema.
        input: serde_json::Value,
    },
    /// The result of a tool invocation, carried in a user turn that follows
    /// an assistant turn containing the matching [`MessageContent::ToolUse`].
    ToolResult {
        /// Identifier of the [`MessageContent::ToolUse`] block this result
        /// answers.
        tool_use_id: String,
        /// Tool output as a string. Structured outputs are serialized to JSON
        /// upstream; Reeve does not impose a schema on this field.
        content: String,
        /// `true` if the tool execution failed; signals to the model that the
        /// result represents an error condition.
        is_error: bool,
    },
}

// ── Tool ─────────────────────────────────────────────────────────────────────

/// A tool the adapter may surface to the model.
///
/// The `input_schema` is a JSON Schema object describing the tool's argument
/// shape. The adapter forwards it verbatim to the provider; Reeve does not
/// validate its structure beyond requiring it to be a JSON value.
#[derive(Debug, Clone)]
pub struct Tool {
    /// Short, unique name the model uses to invoke this tool.
    pub name: String,
    /// Human-readable description surfaced to the model to guide tool
    /// selection.
    ///
    /// No length limit is enforced here; provider-side limits apply
    /// (e.g., Anthropic enforces a 1024-character limit; long descriptions
    /// are silently truncated or surface as `BadRequest`).
    pub description: String,
    /// JSON Schema describing the tool's input shape, forwarded verbatim to
    /// the provider.
    pub input_schema: serde_json::Value,
}

// ── Temperature ──────────────────────────────────────────────────────────────

/// Sampling temperature, validated to `[0.0, 2.0]` at construction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Temperature(f32);

impl Temperature {
    /// Construct a `Temperature`, validating the value is in `[0.0, 2.0]`.
    ///
    /// Returns `Err(TemperatureOutOfRange)` if `value` is NaN, below 0.0,
    /// or above 2.0.
    pub fn new(value: f32) -> Result<Self, TemperatureOutOfRange> {
        if value.is_nan() || !(0.0..=2.0).contains(&value) {
            Err(TemperatureOutOfRange { value })
        } else {
            Ok(Self(value))
        }
    }

    /// The validated temperature value, guaranteed to be in `[0.0, 2.0]`
    /// and not NaN by construction.
    #[must_use]
    pub fn value(self) -> f32 {
        self.0
    }
}

/// Error returned when [`Temperature::new`] is called with a value
/// outside `[0.0, 2.0]` or with NaN.
#[derive(Debug, Clone)]
pub struct TemperatureOutOfRange {
    /// The rejected value, made available so callers can format their
    /// own error messages or log the input that failed validation.
    pub value: f32,
}

impl fmt::Display for TemperatureOutOfRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "temperature {} out of range [0.0, 2.0]", self.value)
    }
}

impl std::error::Error for TemperatureOutOfRange {}

// ── Params ───────────────────────────────────────────────────────────────────

/// Call-time parameters for a single [`Adapter::call`] invocation.
///
/// Only the minimum set needed for this ladder. Additional parameters will be
/// added via `#[non_exhaustive]` extension in later tasks.
#[derive(Debug, Clone, Default)]
pub struct Params {
    /// Maximum tokens the provider may emit in a single response.
    ///
    /// **No useful default exists**; the derived `Default` impl yields
    /// `0`, which providers reject as a `BadRequest`. Callers should
    /// always set this explicitly (e.g., 1024 for normal completions,
    /// 4096+ for code generation, up to the model's context window).
    pub max_tokens: u32,
    /// Sampling temperature in `[0.0, 2.0]`. `None` means use the provider
    /// default.
    pub temperature: Option<Temperature>,
    /// Optional system prompt — provider-level instructions setting
    /// persona, tool-use style, formatting preferences, etc.
    ///
    /// When present, the adapter should prepend a [`Role::System`] message
    /// (or use the provider's native system-prompt field) before forwarding
    /// the caller messages.
    ///
    /// SECURITY: system prompts may contain operator-specific context
    /// (internal service names, deployment-specific tool descriptions,
    /// occasionally embedded credentials in URL parameters). Adapter
    /// implementations MUST NOT log `Params` at debug or trace level
    /// without redacting this field. Treat the contents as
    /// operator-confidential, not provider-public.
    pub system_prompt: Option<String>,
}

// ── CostEstimate ─────────────────────────────────────────────────────────────

/// Best-effort cost estimate for this call, computed by the adapter
/// from [`TokenCounts`] and the adapter-local per-token rates.
///
/// Stored as `u64` microdollars (1 USD = `1_000_000` units) to permit
/// lossless aggregation by the runtime billing layer. The adapter is NOT
/// the authoritative billing source: rate tables are adapter-local snapshots
/// and may diverge from negotiated pricing. Aggregate cost across calls in
/// the runtime billing layer, not in adapter-returned [`Response`]s.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CostEstimate {
    /// Cost in microdollars (USD × `1_000_000`). Lossless under sum.
    pub microdollars: u64,
}

impl CostEstimate {
    /// Approximate display value in US dollars. Lossy; use only for display.
    /// For aggregation, sum [`CostEstimate::microdollars`] directly.
    #[must_use]
    pub fn usd(self) -> f64 {
        // Split into low and high 32-bit words to avoid a `u64 as f64` cast.
        // For values ≤ u32::MAX (~$4 300), the hi path contributes 0.
        // Precision loss past 2^53 microdollars (~$9 trillion) is acceptable
        // for this display-only helper.
        let lo = f64::from(u32::try_from(self.microdollars & 0xFFFF_FFFF).unwrap_or(u32::MAX));
        let hi = f64::from(u32::try_from(self.microdollars >> 32).unwrap_or(0_u32));
        (hi * 4_294_967_296.0 + lo) / 1_000_000.0
    }
}

// ── Response ─────────────────────────────────────────────────────────────────

/// The structured return value from a successful [`Adapter::call`].
///
/// This struct is `#[non_exhaustive]`: future tasks may add fields (e.g.
/// reasoning traces, cache metadata) without breaking callers.
///
/// Use [`Response::new_text`] to construct a `Response` from outside this crate —
/// the `#[non_exhaustive]` attribute prevents struct-literal construction by
/// external callers.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Response {
    /// Text or other content blocks the model produced.
    pub content: Vec<MessageContent>,
    /// Tool invocations the model requested; empty if no tools were called.
    pub tool_calls: Vec<ToolCall>,
    /// Why the model stopped generating.
    pub finish_reason: FinishReason,
    /// Token usage for this call.
    pub tokens: TokenCounts,
    /// Best-effort cost estimate for this call.
    pub cost: CostEstimate,
    /// Round-trip latency from when the request was sent to when the
    /// response was fully received and decoded. For non-streaming calls,
    /// this is the request-send to response-decode interval. Streaming
    /// adapters may interpret this as time-to-first-token or
    /// time-to-completion depending on what they treat as "fully received"
    /// — the contract is per-adapter and should be documented in concrete
    /// adapter implementations.
    pub latency: std::time::Duration,
}

impl Response {
    /// Construct a text-only `Response` from outside this crate.
    ///
    /// Sets `tool_calls` to empty, `finish_reason` to [`FinishReason::EndTurn`],
    /// and `latency` to zero. Useful for mock adapters and simple adapter
    /// implementations that do not use tool calling.
    ///
    /// For responses that include tool calls, construct `Response` directly
    /// using struct literal syntax inside the `reeve-adapter` crate (the
    /// `#[non_exhaustive]` attribute only prevents external struct literals).
    pub fn new_text(content: Vec<MessageContent>, tokens: TokenCounts, cost: CostEstimate) -> Self {
        Self {
            content,
            tool_calls: Vec::new(),
            finish_reason: FinishReason::EndTurn,
            tokens,
            cost,
            latency: std::time::Duration::ZERO,
        }
    }

    /// Construct a `Response` carrying tool calls (`finish_reason` set to
    /// [`FinishReason::ToolUse`]).
    ///
    /// Used by mock adapters that drive the agent's tool execution loop in
    /// tests. `latency` is set to zero.
    pub fn new_tool_use(
        content: Vec<MessageContent>,
        tool_calls: Vec<ToolCall>,
        tokens: TokenCounts,
        cost: CostEstimate,
    ) -> Self {
        Self {
            content,
            tool_calls,
            finish_reason: FinishReason::ToolUse,
            tokens,
            cost,
            latency: std::time::Duration::ZERO,
        }
    }
}

/// A tool invocation the model requested.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Opaque identifier assigned by the provider; echoed back in a
    /// tool-result message when the caller returns the tool's output.
    pub id: String,
    /// Name of the tool the model chose to invoke.
    pub name: String,
    /// The model's arguments for this invocation, as a JSON value. The shape
    /// conforms to the corresponding [`Tool::input_schema`].
    pub arguments: serde_json::Value,
}

/// Why the model stopped generating tokens.
///
/// This enum is `#[non_exhaustive]`: future provider-specific reasons may be
/// added without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FinishReason {
    /// The model reached a natural stopping point (end-of-turn).
    EndTurn,
    /// The response was truncated because it hit [`Params::max_tokens`].
    MaxTokens,
    /// The model emitted one or more tool calls and is waiting for results.
    ToolUse,
    /// The model stopped at a caller-supplied stop sequence.
    StopSequence,
    /// The provider reported a stop reason not captured by the other
    /// variants. Callers should treat this as a normal stop (no retry,
    /// no special handling) but should log the original reason for later
    /// triage.
    Other,
}

/// Token usage reported by the provider for a single call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenCounts {
    /// Tokens in the request (prompt).
    pub input: u32,
    /// Tokens in the response (completion).
    pub output: u32,
    /// Tokens served from the provider's prompt cache, already counted within
    /// `input`. Non-zero only when the adapter declares
    /// [`Capability::PromptCaching`].
    pub cached: u32,
}

// ── AuthKind ──────────────────────────────────────────────────────────────────

/// Categorized authentication/authorization failure shape.
///
/// Distinguishes invalid-credential from forbidden so the runtime can route
/// to operator-fix vs request-replan.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthKind {
    /// HTTP 401 — credential is invalid, expired, or absent.
    InvalidCredential,
    /// HTTP 403 — credential is valid but not authorized for this resource
    /// (e.g., model not enabled for this account).
    Forbidden,
    /// Other auth failure shape the adapter could not categorize.
    ///
    /// SECURITY: implementors MUST NOT include credential material
    /// (API keys, authorization headers, key prefixes, account
    /// identifiers) in this string. Free-form text is for operator
    /// triage only. The string may be logged at INFO or higher.
    Other(String),
}

// ── AdapterError ─────────────────────────────────────────────────────────────

/// A structured error returned by [`Adapter::call`].
///
/// Each variant carries enough information for the runtime to decide whether
/// to retry, fail-over, or surface the error to the operator. This enum is
/// `#[non_exhaustive]` so future variants can be added without breaking
/// existing match arms.
#[non_exhaustive]
#[derive(Debug)]
pub enum AdapterError {
    /// A network-level failure: connection refused, DNS resolution failure,
    /// timeout, or TLS handshake error. The runtime may retry with back-off.
    Network {
        /// The underlying network error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Authentication or authorization failure (HTTP 401, 403). The
    /// credential should be re-checked before retrying.
    Auth {
        /// Categorized auth failure shape — distinguishes
        /// invalid-credential from forbidden so the runtime can
        /// route to operator-fix vs request-replan.
        kind: AuthKind,
    },
    /// The provider applied a rate limit (HTTP 429). The caller may retry
    /// after the indicated back-off, if present.
    RateLimit {
        /// Number of seconds to wait before retrying, when provided by the
        /// provider in a `Retry-After` header or response body.
        retry_after_secs: Option<u64>,
    },
    /// The provider rejected the request as malformed (4xx other than 401,
    /// 403, 429). Retrying the same request is unlikely to succeed.
    ///
    /// SECURITY: implementors MUST NOT include credential material
    /// (API keys, authorization headers, key prefixes, account identifiers)
    /// in `message`. Provider error bodies often echo back authentication
    /// context; callers should redact before constructing this variant. The
    /// message may be logged at INFO or higher.
    BadRequest {
        /// Human-readable explanation from the provider or the adapter.
        message: String,
    },
    /// The provider encountered an internal error (5xx). The runtime may
    /// retry or fail-over to another adapter.
    ///
    /// SECURITY: implementors MUST NOT include credential material
    /// (API keys, authorization headers, key prefixes, account identifiers)
    /// in `message`. Provider error bodies often echo back authentication
    /// context; callers should redact before constructing this variant. The
    /// message may be logged at INFO or higher.
    Provider {
        /// HTTP status code returned by the provider.
        status: u16,
        /// Human-readable explanation from the provider or the adapter.
        message: String,
    },
    /// The adapter could not decode the provider's response. This indicates
    /// a protocol mismatch or an unexpected provider-side change.
    Decode {
        /// The underlying deserialization or parsing error.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The credential required to authenticate with the provider could not be
    /// loaded from the OS keychain. This error is delegated to the keychain
    /// subsystem (Task 17); the adapter reports it here so callers have a
    /// uniform error surface.
    ///
    /// SECURITY: implementors MUST NOT include credential material in
    /// `message`. This includes:
    /// - Key bytes or key prefixes (full or partial — even 4 bytes can
    ///   help an attacker fingerprint accounts)
    /// - Account identifiers, keychain item labels that bind to specific
    ///   accounts, service-name + account-name combinations
    /// - Raw OS keychain error strings — macOS Security framework and
    ///   Linux secret-service routinely echo account labels in error
    ///   messages. Construct messages from `KeychainError` variants by
    ///   name (e.g., "item not found", "item exists"); never pass
    ///   `os_error.to_string()` through.
    ///
    /// The message is for operator triage and may be logged at INFO or
    /// higher; treat it like a public string. A future `RedactedString`
    /// newtype may make this contract type-enforced rather than
    /// convention-enforced; until then, this is the convention contract.
    CredentialUnavailable {
        /// Human-readable explanation of why the credential could not be
        /// loaded.
        message: String,
    },
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network { source } => {
                write!(f, "network error: {source}")
            }
            Self::Auth {
                kind: AuthKind::InvalidCredential,
            } => {
                write!(f, "authentication failure: invalid credential (HTTP 401)")
            }
            Self::Auth {
                kind: AuthKind::Forbidden,
            } => {
                write!(f, "authentication failure: forbidden (HTTP 403)")
            }
            Self::Auth {
                kind: AuthKind::Other(msg),
            } => {
                write!(f, "authentication failure: {msg}")
            }
            Self::RateLimit {
                retry_after_secs: Some(secs),
            } => {
                write!(f, "rate limited; retry after {secs}s")
            }
            Self::RateLimit {
                retry_after_secs: None,
            } => {
                write!(f, "rate limited; no retry-after provided")
            }
            Self::BadRequest { message } => {
                write!(f, "bad request: {message}")
            }
            Self::Provider { status, message } => {
                write!(f, "provider error (HTTP {status}): {message}")
            }
            Self::Decode { source } => {
                write!(f, "response decode error: {source}")
            }
            Self::CredentialUnavailable { message } => {
                write!(f, "credential unavailable: {message}")
            }
        }
    }
}

impl std::error::Error for AdapterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Network { source } | Self::Decode { source } => Some(source.as_ref()),
            Self::Auth { .. }
            | Self::RateLimit { .. }
            | Self::BadRequest { .. }
            | Self::Provider { .. }
            | Self::CredentialUnavailable { .. } => None,
        }
    }
}

// ── Adapter trait ─────────────────────────────────────────────────────────────

/// The core abstraction for a (route, model) pair.
///
/// An `Adapter` translates Reeve's internal protocol into the wire format
/// expected by a specific provider and route, and translates responses back.
/// Per-pair quirks live in the concrete implementation.
///
/// Implementations must be `Send + Sync` so they can be held behind a
/// `Box<dyn Adapter>` in the runtime's adapter registry and shared across
/// Tokio tasks.
///
/// # Dyn-safety
///
/// The trait is dyn-safe: `Box<dyn Adapter>` is valid and is how the runtime's
/// adapter registry stores resolved adapters.
///
/// # Example
///
/// ```rust,ignore
/// let adapter: Box<dyn Adapter> = registry.resolve(&agent_snapshot)?;
/// let response = adapter.call(&messages, &tools, &params).await?;
/// ```
#[async_trait::async_trait]
pub trait Adapter: Send + Sync {
    /// Returns the adapter's unique identifier (e.g.,
    /// `"claude-opus-4-7@anthropic-direct"`).
    ///
    /// **Stability contract**: implementors MUST return the same string
    /// for the lifetime of the adapter instance. Two adapter instances
    /// returning the same `id()` are interchangeable from the runtime
    /// registry's perspective.
    // TODO(phase-5/6): replace with a typed AdapterId { model: String,
    // route: String } when the registry lands. The current "model@route"
    // string convention is unenforced and the @ delimiter is ambiguous
    // about field order.
    fn id(&self) -> &str;

    /// Returns the adapter's declared capability set.
    ///
    /// **Lifetime contract**: implementors MUST return the same value
    /// for every call on a given adapter instance. The capability set
    /// is fixed at adapter-instance construction; if a credential
    /// refresh or model upgrade changes capabilities, that requires
    /// re-registering the adapter.
    fn capabilities(&self) -> Capabilities;

    /// Make a single call to the underlying model.
    ///
    /// # Arguments
    ///
    /// * `messages` — the conversation history in Reeve's internal format, in
    ///   chronological order. The adapter translates these into the provider's
    ///   wire format.
    /// * `tools` — tools the caller wishes to surface to the model. May be
    ///   empty. The adapter should only forward tools when
    ///   [`Capability::ToolCalling`] is declared.
    /// * `params` — call-time parameters such as `max_tokens` and
    ///   `temperature`.
    ///
    /// # Errors
    ///
    /// Returns an [`AdapterError`] with a structured reason. The runtime uses
    /// the variant to decide whether to retry, fail-over, or surface the error
    /// to the operator.
    async fn call(
        &self,
        messages: &[Message],
        tools: &[Tool],
        params: &Params,
    ) -> Result<Response, AdapterError>;
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── A1: AdapterError Display ─────────────────────────────────────────────

    /// Minimal error type for testing `AdapterError` Display impls.
    struct StringError(&'static str);
    impl fmt::Display for StringError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }
    impl fmt::Debug for StringError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }
    impl std::error::Error for StringError {}

    fn assert_display_informative(label: &str, err: &AdapterError) {
        let s = err.to_string();
        assert!(!s.is_empty(), "Display for {label} produced empty string");
        assert!(
            s.contains(':') || s.contains("retry"),
            "Display for {label} appears uninformative: {s:?}"
        );
    }

    /// A1: Each `AdapterError` variant produces a non-empty, informative
    /// Display string.
    #[test]
    fn adapter_error_display_is_informative() {
        assert_display_informative(
            "network",
            &AdapterError::Network {
                source: Box::new(StringError("connection refused")),
            },
        );
        assert_display_informative(
            "auth invalid credential",
            &AdapterError::Auth {
                kind: AuthKind::InvalidCredential,
            },
        );
        assert_display_informative(
            "auth forbidden",
            &AdapterError::Auth {
                kind: AuthKind::Forbidden,
            },
        );
        assert_display_informative(
            "auth other",
            &AdapterError::Auth {
                kind: AuthKind::Other("unexpected response".into()),
            },
        );
        assert_display_informative(
            "rate limit retry",
            &AdapterError::RateLimit {
                retry_after_secs: Some(30),
            },
        );
        assert_display_informative(
            "rate limit no retry",
            &AdapterError::RateLimit {
                retry_after_secs: None,
            },
        );
        assert_display_informative(
            "bad request",
            &AdapterError::BadRequest {
                message: "missing field".into(),
            },
        );
        assert_display_informative(
            "provider",
            &AdapterError::Provider {
                status: 503,
                message: "overloaded".into(),
            },
        );
        assert_display_informative(
            "decode",
            &AdapterError::Decode {
                source: Box::new(StringError("unexpected token")),
            },
        );
        assert_display_informative(
            "credential unavailable",
            &AdapterError::CredentialUnavailable {
                message: "keychain entry not found".into(),
            },
        );
    }

    // ── A2: Capabilities ─────────────────────────────────────────────────────

    /// A2: `Capabilities::new()` is empty; `with` correctly adds capabilities
    /// and the implication invariant is enforced.
    #[test]
    fn capabilities_new_is_empty_and_with_works() {
        let caps = Capabilities::new();
        assert!(!caps.contains(Capability::ToolCalling));
        assert!(!caps.contains(Capability::Vision));
        assert!(!caps.contains(Capability::Reasoning));
        assert!(!caps.contains(Capability::StructuredOutput));
        assert!(!caps.contains(Capability::ParallelToolCalls));
        assert!(!caps.contains(Capability::PromptCaching));
        assert!(caps.is_well_formed());

        let caps = Capabilities::new()
            .with(Capability::ToolCalling)
            .with(Capability::ParallelToolCalls);
        assert!(caps.contains(Capability::ToolCalling));
        assert!(caps.contains(Capability::ParallelToolCalls));
        assert!(caps.is_well_formed());

        let bad = {
            let mut c = Capabilities::new();
            c.set.insert(Capability::ParallelToolCalls);
            c
        };
        assert!(!bad.is_well_formed());
    }

    /// A2b: `try_with` returns `Err` when `ParallelToolCalls` is added
    /// without `ToolCalling` present.
    #[test]
    fn capabilities_try_with_returns_err_on_violation() {
        let result = Capabilities::new().try_with(Capability::ParallelToolCalls);
        assert!(
            result.is_err(),
            "try_with(ParallelToolCalls) without ToolCalling should be Err"
        );
        let err = result.unwrap_err();
        assert_eq!(err.adding, Capability::ParallelToolCalls);
        assert_eq!(err.missing, Capability::ToolCalling);
        assert!(!err.to_string().is_empty());
    }

    /// A2c: `with` panics (both debug and release) when implication invariant
    /// is violated.
    #[test]
    #[should_panic(expected = "implication invariant violated")]
    fn capabilities_with_panics_on_violation() {
        let _ = Capabilities::new().with(Capability::ParallelToolCalls);
    }

    // ── A3: Type construction ─────────────────────────────────────────────────

    /// A3: `Message`, `Tool`, `Params`, `Response`, `ToolCall`, `TokenCounts`,
    /// and `FinishReason` can all be constructed and compared as expected.
    #[test]
    fn type_construction_compiles_and_round_trips() {
        let msg = Message {
            role: Role::User,
            content: vec![MessageContent::Text("hello".into())],
        };
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, vec![MessageContent::Text("hello".into())]);

        let tool = Tool {
            name: "search".into(),
            description: "Search the web.".into(),
            input_schema: serde_json::json!({ "type": "object" }),
        };
        assert_eq!(tool.name, "search");

        let params = Params {
            max_tokens: 1024,
            temperature: Some(Temperature::new(0.7).unwrap()),
            system_prompt: Some("You are helpful.".into()),
        };
        assert_eq!(params.max_tokens, 1024);

        let tokens = TokenCounts {
            input: 10,
            output: 20,
            cached: 5,
        };
        assert_eq!(tokens.input + tokens.output, 30);

        let tool_call = ToolCall {
            id: "call_abc".into(),
            name: "search".into(),
            arguments: serde_json::json!({ "query": "rust async" }),
        };
        assert_eq!(tool_call.name, "search");

        let response = Response {
            content: vec![MessageContent::Text("world".into())],
            tool_calls: vec![tool_call],
            finish_reason: FinishReason::EndTurn,
            tokens,
            cost: CostEstimate { microdollars: 420 },
            latency: std::time::Duration::from_millis(350),
        };
        assert_eq!(response.finish_reason, FinishReason::EndTurn);
        assert_eq!(response.content.len(), 1);
        assert_eq!(response.tool_calls.len(), 1);
        assert!((response.cost.usd() - 0.000_420).abs() < 1e-9);
        assert_eq!(response.latency, std::time::Duration::from_millis(350));
    }

    // ── A4 + A5: MockAdapter ─────────────────────────────────────────────────

    /// Minimal in-memory adapter used by trait contract tests.
    ///
    /// Returns a fixed `Response` regardless of inputs; not suitable for
    /// integration testing actual adapter behavior.
    struct MockAdapter;

    #[async_trait::async_trait]
    impl Adapter for MockAdapter {
        fn id(&self) -> &'static str {
            "mock@test"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::new()
        }

        async fn call(
            &self,
            _messages: &[Message],
            _tools: &[Tool],
            _params: &Params,
        ) -> Result<Response, AdapterError> {
            Ok(Response {
                content: vec![],
                tool_calls: vec![],
                finish_reason: FinishReason::EndTurn,
                tokens: TokenCounts::default(),
                cost: CostEstimate::default(),
                latency: std::time::Duration::ZERO,
            })
        }
    }

    /// A4: `MockAdapter::id()` and `MockAdapter::capabilities()` return correct
    /// values.
    #[tokio::test]
    async fn mock_adapter_id_and_capabilities() {
        let adapter = MockAdapter;
        assert_eq!(adapter.id(), "mock@test");
        assert_eq!(adapter.capabilities(), Capabilities::new());
    }

    /// A4b: `call()` returns `Ok` with an empty but valid `Response`.
    #[tokio::test]
    async fn mock_adapter_call_returns_ok() {
        let adapter = MockAdapter;
        let result = adapter.call(&[], &[], &Params::default()).await;
        let response = result.expect("MockAdapter.call should return Ok");
        assert!(response.content.is_empty());
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.finish_reason, FinishReason::EndTurn);
        assert_eq!(response.tokens, TokenCounts::default());
    }

    /// A5: The trait is dyn-safe — `Box<dyn Adapter>` can be constructed.
    #[test]
    fn adapter_is_dyn_safe() {
        let _: Box<dyn Adapter> = Box::new(MockAdapter);
    }

    /// A5b: Dyn dispatch through `call()` actually works end-to-end.
    #[tokio::test]
    async fn adapter_dyn_dispatch_call_works() {
        let adapter: Box<dyn Adapter> = Box::new(MockAdapter);
        let response = adapter.call(&[], &[], &Params::default()).await.unwrap();
        assert!(matches!(response.finish_reason, FinishReason::EndTurn));
    }

    // ── A6: error::Error::source() chain ────────────────────────────────────

    /// `A6_error_source_chain`: `source()` is `Some` for box-wrapped variants
    /// and `None` for leaf variants (all 7 `AdapterError` variants covered).
    #[test]
    fn error_source_chain() {
        struct Leaf;
        impl fmt::Display for Leaf {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("leaf")
            }
        }
        impl fmt::Debug for Leaf {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("Leaf")
            }
        }
        impl std::error::Error for Leaf {}

        let network = AdapterError::Network {
            source: Box::new(Leaf),
        };
        assert!(
            std::error::Error::source(&network).is_some(),
            "Network::source should be Some"
        );

        let decode = AdapterError::Decode {
            source: Box::new(Leaf),
        };
        assert!(
            std::error::Error::source(&decode).is_some(),
            "Decode::source should be Some"
        );

        let auth = AdapterError::Auth {
            kind: AuthKind::InvalidCredential,
        };
        assert!(
            std::error::Error::source(&auth).is_none(),
            "Auth::source should be None"
        );

        let rate = AdapterError::RateLimit {
            retry_after_secs: Some(5),
        };
        assert!(
            std::error::Error::source(&rate).is_none(),
            "RateLimit::source should be None"
        );

        let bad = AdapterError::BadRequest {
            message: "oops".into(),
        };
        assert!(
            std::error::Error::source(&bad).is_none(),
            "BadRequest::source should be None"
        );

        let provider = AdapterError::Provider {
            status: 503,
            message: "x".into(),
        };
        assert!(
            std::error::Error::source(&provider).is_none(),
            "Provider::source should be None"
        );

        let cred = AdapterError::CredentialUnavailable {
            message: "x".into(),
        };
        assert!(
            std::error::Error::source(&cred).is_none(),
            "CredentialUnavailable::source should be None"
        );
    }

    // ── C2: Temperature validation ───────────────────────────────────────────

    /// C2: `Temperature::new` validates bounds correctly.
    #[test]
    fn temperature_validation() {
        assert!(Temperature::new(0.0).is_ok(), "0.0 should be valid");
        assert!(Temperature::new(2.0).is_ok(), "2.0 should be valid");
        assert!(Temperature::new(1.0).is_ok(), "1.0 should be valid");
        assert!(
            Temperature::new(-0.1).is_err(),
            "-0.1 should be out of range"
        );
        assert!(Temperature::new(2.1).is_err(), "2.1 should be out of range");
        assert!(
            Temperature::new(f32::NAN).is_err(),
            "NaN should be rejected"
        );

        let t = Temperature::new(1.5).unwrap();
        assert!((t.value() - 1.5).abs() < f32::EPSILON);

        let err = Temperature::new(-1.0).unwrap_err();
        assert!(err.to_string().contains("out of range"));
    }

    // ── D2: CostEstimate::usd() hi-path precision ────────────────────────────

    /// D2: `CostEstimate::usd()` is accurate on both the lo path (≤ `u32::MAX`)
    /// and the hi path (> `u32::MAX`, exercising the hi-word arithmetic).
    #[test]
    fn cost_estimate_usd_hi_path_preserves_precision() {
        // Lo path: 420 microdollars → $0.000420
        let small = CostEstimate { microdollars: 420 };
        assert!(
            (small.usd() - 0.000_420).abs() < 1e-9,
            "lo path: got {}",
            small.usd()
        );

        // Hi path: 5_000_000_000_000 microdollars ($5M) — exercises the
        // hi-word arithmetic; validates lossless u64→f64 path at this scale.
        let big = CostEstimate {
            microdollars: 5_000_000_000_000,
        };
        assert!(
            (big.usd() - 5_000_000.0).abs() < 1.0,
            "hi path: got {}",
            big.usd()
        );
    }
}
