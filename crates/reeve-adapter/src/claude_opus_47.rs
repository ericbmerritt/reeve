//! `ClaudeOpus47` adapter on the `anthropic-direct` route.
//!
//! Translates Reeve's internal protocol to Anthropic's Messages API
//! (`POST /v1/messages`) and back. Cost is estimated from adapter-local
//! per-token rates (not authoritative billing).
//!
//! # Security
//!
//! The API key is wrapped in [`secrecy::SecretString`] and never included in
//! error messages, log output, or `Debug` formatting. Only Anthropic's
//! documented stable `error_type` tokens are forwarded to callers.

use std::time::Instant;

use secrecy::ExposeSecret as _;

use crate::anthropic::{self, ErrorResponse, MessagesResponse};
use crate::{
    Adapter, AdapterError, AuthKind, Capabilities, Capability, CostEstimate, Message, Params,
    Response, TokenCounts, Tool,
};

// ── Route constants ────────────────────────────────────────────────────────────

/// Base URL for the `anthropic-direct` route.
pub(crate) const BASE_URL: &str = "https://api.anthropic.com";

/// API version pin for the Anthropic Messages API.
///
/// `2023-06-01` is the stable Messages API version that introduced
/// the v1 wire format used by all Claude 3.x and 4.x models. Anthropic
/// versions are dated; future versions may add fields without breaking
/// 2023-06-01 callers, but new content block types may appear that this
/// adapter cannot parse. If Anthropic deprecates 2023-06-01, the
/// adapter must update both this constant and the wire types.
pub(crate) const API_VERSION: &str = "2023-06-01";

/// HTTP header name for the API key (Anthropic-specific).
pub(crate) const API_KEY_HEADER: &str = "x-api-key";

/// HTTP header name for the API version.
pub(crate) const API_VERSION_HEADER: &str = "anthropic-version";

/// Endpoint for the Messages API.
pub(crate) const MESSAGES_ENDPOINT: &str = "/v1/messages";

// Named HTTP status constants used in the error-mapping match below.
const HTTP_UNAUTHORIZED: u16 = 401;
const HTTP_FORBIDDEN: u16 = 403;
const HTTP_TOO_MANY_REQUESTS: u16 = 429;

/// Model identifier string sent on the wire.
const MODEL_ID: &str = "claude-opus-4-7";

/// Adapter identifier returned by [`Adapter::id`].
const ADAPTER_ID: &str = "claude-opus-4-7@anthropic-direct";

// ── Cost rates ─────────────────────────────────────────────────────────────────

/// Per-token cost rates for claude-opus-4-7 on `anthropic-direct`, expressed
/// in microdollars (1 USD = `1_000_000` µ$).
///
/// Rates are adapter-local snapshots and may diverge from negotiated billing.
/// Verified against Anthropic's published pricing (claude.ai/pricing) as of
/// 2026: Opus 4 family = $15/M input, $75/M output. Cache reads at ~10% of
/// input rate ($1.50/M), rounded to 1 µ$/token for integer arithmetic.
///
/// Aggregation belongs in the runtime billing layer; never trust these for
/// invoicing.
pub(crate) struct ClaudeOpus47Rates;

impl ClaudeOpus47Rates {
    /// $15 / M input tokens → 15 µ$/token.
    pub(crate) const INPUT_MICRODOLLARS_PER_TOKEN: u64 = 15;
    /// $75 / M output tokens → 75 µ$/token.
    pub(crate) const OUTPUT_MICRODOLLARS_PER_TOKEN: u64 = 75;
    /// ~$1.50 / M cache-read tokens → 1 µ$/token (rounded from 1.5).
    pub(crate) const CACHE_READ_MICRODOLLARS_PER_TOKEN: u64 = 1;
}

/// Compute the cost estimate for a single call from its [`TokenCounts`].
pub(crate) fn compute_cost(tokens: TokenCounts) -> CostEstimate {
    let micro: u64 = u64::from(tokens.input) * ClaudeOpus47Rates::INPUT_MICRODOLLARS_PER_TOKEN
        + u64::from(tokens.output) * ClaudeOpus47Rates::OUTPUT_MICRODOLLARS_PER_TOKEN
        + u64::from(tokens.cached) * ClaudeOpus47Rates::CACHE_READ_MICRODOLLARS_PER_TOKEN;
    CostEstimate {
        microdollars: micro,
    }
}

// ── ClaudeOpus47 ──────────────────────────────────────────────────────────────

/// The `claude-opus-4-7@anthropic-direct` adapter.
///
/// Construct with [`ClaudeOpus47::new`] for production use. The key is stored
/// as a [`secrecy::SecretString`] and is never exposed through `Debug` output
/// or error messages.
pub struct ClaudeOpus47 {
    client: reqwest::Client,
    api_key: secrecy::SecretString,
    base_url: String,
    capabilities: Capabilities,
}

impl ClaudeOpus47 {
    /// Build the capability set declared by this adapter.
    ///
    /// Capabilities reflect what the adapter actually exposes through the
    /// `anthropic-direct` route — not the raw model feature list. Per spec,
    /// routes may expose less than the model supports.
    fn declared_capabilities() -> Capabilities {
        // TODO(phase-5+): Add Capability::Vision when MessageContent gains
        // an Image variant and parse_response handles image content blocks.
        // Add Capability::Reasoning when MessagesResponseContent gains a
        // Thinking variant; today, "thinking" content blocks cause
        // AdapterError::Decode (see AT16). Declaring either capability
        // would violate declare-vs-deliver honesty.
        Capabilities::new()
            .with(Capability::ToolCalling)
            .with(Capability::StructuredOutput)
            // Must follow Capability::ToolCalling above (implication invariant).
            .with(Capability::ParallelToolCalls)
            .with(Capability::PromptCaching)
    }

    /// Construct a new adapter with the given API key.
    ///
    /// Uses a default [`reqwest::Client`] and the production Anthropic base
    /// URL. Task 17 will wire keychain credential retrieval here.
    pub fn new(api_key: secrecy::SecretString) -> Self {
        Self::with_base_url(api_key, BASE_URL.to_owned())
    }

    /// Construct an adapter with an explicit base URL.
    ///
    /// **Internal — production callers MUST use [`ClaudeOpus47::new`].**
    /// `new` calls this constructor with the hardcoded production HTTPS
    /// endpoint; tests call it directly with a wiremock server URL.
    /// `with_base_url` accepts arbitrary URL schemes (including `http://`)
    /// because tests target HTTP-only mock servers; it does NOT validate
    /// the scheme. Routing operator-supplied URLs through here would
    /// expose the API key to plaintext network channels — never accept
    /// a base URL from outside this crate.
    #[expect(
        clippy::expect_used,
        reason = "TLS backend init failure at construction time is a system-level error \
                  unrecoverable by the adapter; falling back to Client::new() would silently \
                  void the no-redirect security invariant, so panicking is correct here"
    )]
    pub(crate) fn with_base_url(api_key: secrecy::SecretString, base_url: String) -> Self {
        // Policy::none() prevents reqwest from following redirects, which
        // would re-send x-api-key to the redirect destination.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect(
                "reqwest client construction must succeed (rustls-tls init failure is \
                 unrecoverable; do not fall back to Client::new() which would void the \
                 no-redirect policy)",
            );
        let capabilities = Self::declared_capabilities();
        Self {
            client,
            api_key,
            base_url,
            capabilities,
        }
    }

    /// Parse the `Retry-After` header value as an integer number of seconds.
    ///
    /// Returns `None` if the header is absent or unparseable.
    fn parse_retry_after(response: &reqwest::Response) -> Option<u64> {
        response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
    }

    /// Extract Anthropic's stable `error_type` token from a JSON error body.
    ///
    /// SECURITY: only the `error.type` field is forwarded; the human-readable
    /// `error.message` is discarded to prevent leaking account context or
    /// operator-specific details that Anthropic may echo back.
    async fn extract_error_type(response: reqwest::Response) -> String {
        match response.json::<ErrorResponse>().await {
            Ok(body) => body.error.error_type,
            Err(_) => "unknown_error".to_owned(),
        }
    }
}

#[async_trait::async_trait]
impl Adapter for ClaudeOpus47 {
    fn id(&self) -> &str {
        ADAPTER_ID
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn call(
        &self,
        messages: &[Message],
        tools: &[Tool],
        params: &Params,
    ) -> Result<Response, AdapterError> {
        // 1. Translate Reeve types → Anthropic wire format.
        let wire_req = anthropic::build_request(messages, tools, params, MODEL_ID)
            .map_err(AdapterError::from)?;

        // 2. Build and dispatch the HTTP request, measuring round-trip latency.
        let url = format!("{}{}", self.base_url, MESSAGES_ENDPOINT);
        let start = Instant::now();
        let http_response = self
            .client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(API_KEY_HEADER, self.api_key.expose_secret())
            .header(API_VERSION_HEADER, API_VERSION)
            .json(&wire_req)
            .send()
            .await
            .map_err(|e| AdapterError::Network {
                source: Box::new(e),
            })?;
        let latency = start.elapsed();

        // 3. Map HTTP status → AdapterError for all non-200 responses.
        let status = http_response.status();
        if !status.is_success() {
            let status_u16 = status.as_u16();
            return match status_u16 {
                HTTP_UNAUTHORIZED => Err(AdapterError::Auth {
                    kind: AuthKind::InvalidCredential,
                }),
                HTTP_FORBIDDEN => Err(AdapterError::Auth {
                    kind: AuthKind::Forbidden,
                }),
                HTTP_TOO_MANY_REQUESTS => {
                    let retry_after_secs = Self::parse_retry_after(&http_response);
                    Err(AdapterError::RateLimit { retry_after_secs })
                }
                400..=499 => {
                    let error_type = Self::extract_error_type(http_response).await;
                    Err(AdapterError::BadRequest {
                        message: error_type,
                    })
                }
                _ => {
                    let error_type = Self::extract_error_type(http_response).await;
                    Err(AdapterError::Provider {
                        status: status_u16,
                        message: error_type,
                    })
                }
            };
        }

        // 4. Decode the response body.
        let body: MessagesResponse =
            http_response
                .json()
                .await
                .map_err(|e| AdapterError::Decode {
                    source: Box::new(e),
                })?;

        // 5. Translate wire format → Reeve types.
        let (content, tool_calls, finish_reason, tokens) = anthropic::parse_response(body);

        // 6. Compute cost from token counts.
        let cost = compute_cost(tokens);

        Ok(Response {
            content,
            tool_calls,
            finish_reason,
            tokens,
            cost,
            latency,
        })
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Capability, Message, MessageContent, Params, Role, TokenCounts};
    use secrecy::SecretString;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TEST_KEY: &str = "sk-ant-TEST_SECRET_DO_NOT_LEAK_12345";

    fn make_adapter(base_url: &str) -> ClaudeOpus47 {
        ClaudeOpus47::with_base_url(SecretString::from(TEST_KEY), base_url.to_owned())
    }

    fn user_message(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![MessageContent::Text(text.to_owned())],
        }
    }

    fn base_params() -> Params {
        Params {
            max_tokens: 128,
            ..Params::default()
        }
    }

    /// Minimal realistic Anthropic 200 response body.
    fn ok_body() -> serde_json::Value {
        serde_json::json!({
            "id": "msg_abc",
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": "Hello from Claude!" }],
            "model": "claude-opus-4-7-20251101",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }
        })
    }

    /// Anthropic error body with a given type.
    fn error_body(error_type: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "error",
            "error": { "type": error_type, "message": "redacted" }
        })
    }

    /// Mount a mock on `MESSAGES_ENDPOINT` that responds with `status` and
    /// `body`. Covers the common case in most tests; tests with extra matchers
    /// (e.g. header assertions) mount their own mock directly.
    async fn mount_mock(server: &MockServer, status: u16, body: serde_json::Value) {
        Mock::given(method("POST"))
            .and(path(MESSAGES_ENDPOINT))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
    }

    // ── AT9: compute_cost ─────────────────────────────────────────────────────

    #[test]
    fn at9_compute_cost_known_tokens() {
        let tokens = TokenCounts {
            input: 1000,
            output: 500,
            cached: 100,
        };
        // 1000 * 15 + 500 * 75 + 100 * 1 = 15_000 + 37_500 + 100 = 52_600
        let cost = compute_cost(tokens);
        assert_eq!(cost.microdollars, 52_600);
    }

    #[test]
    fn at9b_compute_cost_zero_tokens() {
        let cost = compute_cost(TokenCounts::default());
        assert_eq!(cost.microdollars, 0);
    }

    // ── AT10: capabilities are well-formed ────────────────────────────────────

    #[test]
    fn at10_capabilities_are_well_formed() {
        let adapter = ClaudeOpus47::new(SecretString::from("dummy"));
        let caps = adapter.capabilities();
        assert!(caps.is_well_formed(), "capability set must be well-formed");
        assert!(caps.contains(Capability::ToolCalling));
        // Vision is intentionally absent: the adapter cannot translate image
        // content in either direction today. See declared_capabilities() TODO.
        assert!(
            !caps.contains(Capability::Vision),
            "Vision must NOT be declared until image content is supported"
        );
        // Reasoning is intentionally absent: "thinking" content blocks cause
        // AdapterError::Decode today because MessagesResponseContent has no
        // Thinking variant. See AT16 which pins that decode failure.
        assert!(
            !caps.contains(Capability::Reasoning),
            "Reasoning must NOT be declared until thinking content blocks are supported"
        );
        assert!(caps.contains(Capability::StructuredOutput));
        assert!(caps.contains(Capability::ParallelToolCalls));
        assert!(caps.contains(Capability::PromptCaching));
    }

    // ── AT11: status code → error mapping (wiremock) ──────────────────────────

    #[tokio::test]
    async fn at11_401_maps_to_auth_invalid_credential() {
        let server = MockServer::start().await;
        mount_mock(&server, 401, error_body("authentication_error")).await;

        let adapter = make_adapter(&server.uri());
        let err = adapter
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect_err("should fail");

        assert!(
            matches!(
                err,
                AdapterError::Auth {
                    kind: AuthKind::InvalidCredential
                }
            ),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn at11_403_maps_to_auth_forbidden() {
        let server = MockServer::start().await;
        mount_mock(&server, 403, error_body("permission_error")).await;

        let adapter = make_adapter(&server.uri());
        let err = adapter
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect_err("should fail");

        assert!(
            matches!(
                err,
                AdapterError::Auth {
                    kind: AuthKind::Forbidden
                }
            ),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn at11_429_maps_to_rate_limit_with_retry_after() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(MESSAGES_ENDPOINT))
            .respond_with(
                ResponseTemplate::new(429)
                    .append_header("retry-after", "30")
                    .set_body_json(error_body("rate_limit_error")),
            )
            .mount(&server)
            .await;

        let adapter = make_adapter(&server.uri());
        let err = adapter
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect_err("should fail");

        assert!(
            matches!(
                err,
                AdapterError::RateLimit {
                    retry_after_secs: Some(30)
                }
            ),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn at11_500_maps_to_provider_error() {
        let server = MockServer::start().await;
        mount_mock(&server, 500, error_body("api_error")).await;

        let adapter = make_adapter(&server.uri());
        let err = adapter
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect_err("should fail");

        assert!(
            matches!(err, AdapterError::Provider { status: 500, .. }),
            "got: {err}"
        );
        if let AdapterError::Provider { message, .. } = &err {
            assert_eq!(message, "api_error");
        }
    }

    #[tokio::test]
    async fn at11_400_maps_to_bad_request() {
        let server = MockServer::start().await;
        mount_mock(&server, 400, error_body("invalid_request_error")).await;

        let adapter = make_adapter(&server.uri());
        let err = adapter
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect_err("should fail");

        assert!(matches!(err, AdapterError::BadRequest { .. }), "got: {err}");
        if let AdapterError::BadRequest { message } = &err {
            assert_eq!(message, "invalid_request_error");
        }
    }

    /// `AT11_generic_4xx`: a 422 (or any non-special 4xx) maps to
    /// `AdapterError::BadRequest`. Pins the catch-all behavior.
    #[tokio::test]
    async fn at11_generic_4xx_maps_to_bad_request() {
        let server = MockServer::start().await;
        mount_mock(&server, 422, error_body("unprocessable_entity_error")).await;

        let adapter = make_adapter(&server.uri());
        let err = adapter
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect_err("should fail");

        assert!(
            matches!(err, AdapterError::BadRequest { .. }),
            "expected BadRequest for 422, got: {err}"
        );
        if let AdapterError::BadRequest { message } = &err {
            assert_eq!(message, "unprocessable_entity_error");
        }
    }

    // ── AT12: round-trip via wiremock ─────────────────────────────────────────

    #[tokio::test]
    async fn at12_round_trip_200_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(MESSAGES_ENDPOINT))
            .and(header(API_KEY_HEADER, TEST_KEY))
            .and(header(API_VERSION_HEADER, API_VERSION))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .mount(&server)
            .await;

        let adapter = make_adapter(&server.uri());
        let response = adapter
            .call(&[user_message("hello")], &[], &base_params())
            .await
            .expect("should succeed");

        assert_eq!(response.content.len(), 1);
        assert!(
            matches!(&response.content[0], MessageContent::Text(t) if t == "Hello from Claude!")
        );
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.finish_reason, crate::FinishReason::EndTurn);
        assert_eq!(response.tokens.input, 10);
        assert_eq!(response.tokens.output, 5);
        // 10*15 + 5*75 + 0*1 = 150 + 375 = 525 µ$
        assert_eq!(response.cost.microdollars, 525);
    }

    // ── AT13: SECURITY — no API key in errors ─────────────────────────────────

    #[tokio::test]
    async fn at13_no_api_key_in_error_display() {
        let server = MockServer::start().await;
        mount_mock(&server, 401, error_body("authentication_error")).await;

        let adapter = make_adapter(&server.uri());
        let err = adapter
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect_err("should fail");

        let display = format!("{err}");
        let debug = format!("{err:?}");

        assert!(
            !display.contains(TEST_KEY),
            "API key leaked in Display: {display}"
        );
        assert!(
            !debug.contains(TEST_KEY),
            "API key leaked in Debug: {debug}"
        );
        // Also verify that the key sentinel does not appear via any of its
        // component parts that are unique enough to fingerprint an account.
        assert!(
            !display.contains("sk-ant"),
            "key prefix leaked in Display: {display}"
        );
        assert!(
            !debug.contains("sk-ant"),
            "key prefix leaked in Debug: {debug}"
        );
    }

    // ── AT14: SECURITY — redirects are not followed ───────────────────────────

    /// AT14: the adapter MUST NOT follow redirects. A 302 from the server is
    /// surfaced as an error; the redirect target is never contacted.
    ///
    /// This ensures that a misconfigured `base_url` or a rogue server cannot
    /// harvest the `x-api-key` header by returning a cross-host redirect
    /// (BREACH-class credential leak via reqwest's default redirect policy).
    #[tokio::test]
    async fn at14_redirect_not_followed() {
        let server = MockServer::start().await;
        // Return a 302 pointing at an attacker-controlled host.
        Mock::given(method("POST"))
            .and(path(MESSAGES_ENDPOINT))
            .respond_with(
                ResponseTemplate::new(302)
                    .append_header("location", "http://attacker.example.com/leak"),
            )
            .mount(&server)
            .await;

        let adapter = make_adapter(&server.uri());
        let err = adapter
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect_err("302 should be surfaced as an error, not silently followed");

        // The 302 falls into the catch-all (non-success, non-4xx specific) branch.
        // It is neither Auth/RateLimit/BadRequest — it is Provider or Network.
        // The key assertion is that the call failed without contacting attacker.example.com;
        // wiremock only knows about our server, so any request to attacker.example.com
        // would either fail with a network error or produce an unexpected result.
        assert!(
            matches!(
                err,
                AdapterError::Provider { .. } | AdapterError::Network { .. }
            ),
            "302 should surface as Provider or Network, got: {err}"
        );
    }

    /// `AT14b`: the adapter emits exactly ONE request even when the server returns
    /// a redirect. This pins the no-follow-redirect security property at the
    /// request-count level — a regression that removed `Policy::none()` would
    /// emit a second request to the redirect target and this assertion would
    /// catch it.
    #[tokio::test]
    async fn at14b_redirect_sends_exactly_one_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(MESSAGES_ENDPOINT))
            .respond_with(
                ResponseTemplate::new(302)
                    .append_header("location", "http://attacker.example.com/leak"),
            )
            .mount(&server)
            .await;

        let adapter = make_adapter(&server.uri());
        let _ = adapter
            .call(&[user_message("hi")], &[], &base_params())
            .await;

        let received = server.received_requests().await.unwrap();
        assert_eq!(
            received.len(),
            1,
            "redirect must NOT trigger a second request; got {} requests",
            received.len()
        );
    }

    // ── AT16: unknown content type → Decode error ─────────────────────────────

    /// AT16: a 200 response whose content array contains only an unknown block
    /// type (e.g., `"thinking"`) that serde cannot deserialise into
    /// `MessagesResponseContent` results in `AdapterError::Decode`.
    ///
    /// This pins the behavior for wire types that arrive before the adapter's
    /// type list is updated.
    #[tokio::test]
    async fn at16_unknown_content_type_yields_decode_error() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "id": "msg_think",
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "thinking",
                "thinking": "I should reason about this carefully..."
            }],
            "model": "claude-opus-4-7-20251101",
            "stop_reason": "end_turn",
            "stop_sequence": null,
            "usage": {
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0
            }
        });
        mount_mock(&server, 200, body).await;

        let adapter = make_adapter(&server.uri());
        let err = adapter
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect_err("unknown content type should be a decode error");

        assert!(
            matches!(err, AdapterError::Decode { .. }),
            "expected Decode for unknown content type, got: {err}"
        );
    }

    // ── Additional: adapter id ────────────────────────────────────────────────

    #[test]
    fn adapter_id_is_stable() {
        let adapter = ClaudeOpus47::new(SecretString::from("dummy"));
        assert_eq!(adapter.id(), "claude-opus-4-7@anthropic-direct");
    }
}
