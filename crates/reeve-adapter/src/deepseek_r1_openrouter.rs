//! `DeepSeekR1OpenRouter` adapter on the `openrouter` route.
//!
//! Translates Reeve's internal protocol to `OpenRouter`'s OpenAI-compatible
//! chat completions API (`POST /v1/chat/completions`) and back.
//!
//! # Security
//!
//! The API key is wrapped in [`secrecy::SecretString`] and never included in
//! error messages, log output, or `Debug` formatting. Only `OpenRouter`'s
//! documented stable `error.type` tokens are forwarded to callers.

use std::time::Instant;

use secrecy::ExposeSecret as _;

use crate::openai_compat::{self, ChatResponse};
use crate::{
    Adapter, AdapterError, AuthKind, Capabilities, Capability, CostEstimate, Message, Params,
    Response, TokenCounts, Tool,
};

// ── Route constants ────────────────────────────────────────────────────────────

/// Base URL for the `OpenRouter` route.
pub(crate) const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Chat completions endpoint (OpenAI-compatible).
pub(crate) const COMPLETIONS_ENDPOINT: &str = "/chat/completions";

const HTTP_UNAUTHORIZED: u16 = 401;
const HTTP_FORBIDDEN: u16 = 403;
const HTTP_TOO_MANY_REQUESTS: u16 = 429;

/// Model string sent to `OpenRouter` on the wire.
const MODEL_ID: &str = "deepseek/deepseek-r1-0528";

/// Adapter identifier returned by [`Adapter::id`].
const ADAPTER_ID: &str = "deepseek/deepseek-r1-0528@openrouter";

// ── Cost rates ─────────────────────────────────────────────────────────────────

/// Per-token cost rates for `deepseek/deepseek-r1-0528` on `OpenRouter`, in
/// microdollars (1 USD = `1_000_000` µ$).
///
/// Rates are adapter-local snapshots; `OpenRouter` pricing may change.
/// Approximate as of 2026-05: ~$0.55/M input, ~$2.19/M output.
pub(crate) struct DeepSeekR1Rates;

impl DeepSeekR1Rates {
    /// $0.55 / M input tokens → 1 µ$/token (rounded from 0.55).
    pub(crate) const INPUT_MICRODOLLARS_PER_TOKEN: u64 = 1;
    /// $2.19 / M output tokens → 2 µ$/token (rounded from 2.19).
    pub(crate) const OUTPUT_MICRODOLLARS_PER_TOKEN: u64 = 2;
}

/// Compute the cost estimate for a single call from its [`TokenCounts`].
pub(crate) fn compute_cost(tokens: TokenCounts) -> CostEstimate {
    let micro: u64 = u64::from(tokens.input) * DeepSeekR1Rates::INPUT_MICRODOLLARS_PER_TOKEN
        + u64::from(tokens.output) * DeepSeekR1Rates::OUTPUT_MICRODOLLARS_PER_TOKEN;
    CostEstimate {
        microdollars: micro,
    }
}

// ── DeepSeekR1OpenRouter ──────────────────────────────────────────────────────

/// The `deepseek/deepseek-r1-0528@openrouter` adapter.
///
/// Routes through `OpenRouter`'s OpenAI-compatible chat completions endpoint.
/// Construct with [`DeepSeekR1OpenRouter::new`] for production use.
pub struct DeepSeekR1OpenRouter {
    client: reqwest::Client,
    api_key: secrecy::SecretString,
    base_url: String,
    capabilities: Capabilities,
}

impl DeepSeekR1OpenRouter {
    fn declared_capabilities() -> Capabilities {
        Capabilities::new()
            .with(Capability::ToolCalling)
            .with(Capability::StructuredOutput)
            .with(Capability::ParallelToolCalls)
    }

    /// Construct a new adapter with the given `OpenRouter` API key.
    pub fn new(api_key: secrecy::SecretString) -> Self {
        Self::with_base_url(api_key, BASE_URL.to_owned())
    }

    /// Construct an adapter with an explicit base URL.
    ///
    /// **Internal — production callers MUST use [`DeepSeekR1OpenRouter::new`].**
    /// Tests call this with a wiremock server URL. Never accept a base URL
    /// from outside this crate; `with_base_url` does not validate the scheme.
    #[expect(
        clippy::expect_used,
        reason = "TLS backend init failure at construction time is unrecoverable; \
                  falling back to Client::new() would void the no-redirect invariant"
    )]
    pub(crate) fn with_base_url(api_key: secrecy::SecretString, base_url: String) -> Self {
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

    fn parse_retry_after(response: &reqwest::Response) -> Option<u64> {
        response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
    }
}

#[async_trait::async_trait]
impl Adapter for DeepSeekR1OpenRouter {
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
        let wire_req = openai_compat::build_request(messages, tools, params, MODEL_ID);

        let url = format!("{}{}", self.base_url, COMPLETIONS_ENDPOINT);
        let start = Instant::now();
        let http_response = self
            .client
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", self.api_key.expose_secret()),
            )
            .json(&wire_req)
            .send()
            .await
            .map_err(|e| AdapterError::Network {
                source: Box::new(e),
            })?;
        let latency = start.elapsed();

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
                    let error_type = openai_compat::extract_error_type(http_response).await;
                    Err(AdapterError::BadRequest {
                        message: error_type,
                    })
                }
                _ => {
                    let error_type = openai_compat::extract_error_type(http_response).await;
                    Err(AdapterError::Provider {
                        status: status_u16,
                        message: error_type,
                    })
                }
            };
        }

        let body: ChatResponse = http_response
            .json()
            .await
            .map_err(|e| AdapterError::Decode {
                source: Box::new(e),
            })?;

        let (content, tool_calls, finish_reason, tokens) = openai_compat::parse_response(body)?;

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

    const TEST_KEY: &str = "sk-or-test-SECRET_DO_NOT_LEAK_12345";

    fn make_adapter(base_url: &str) -> DeepSeekR1OpenRouter {
        DeepSeekR1OpenRouter::with_base_url(SecretString::from(TEST_KEY), base_url.to_owned())
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

    fn ok_body() -> serde_json::Value {
        serde_json::json!({
            "id": "chatcmpl-abc",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello from DeepSeek!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        })
    }

    fn error_body(error_type: &str) -> serde_json::Value {
        serde_json::json!({
            "error": {
                "type": error_type,
                "message": "redacted"
            }
        })
    }

    async fn mount_mock(server: &MockServer, status: u16, body: serde_json::Value) {
        Mock::given(method("POST"))
            .and(path(COMPLETIONS_ENDPOINT))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(server)
            .await;
    }

    // OR9: compute_cost for known token counts
    #[test]
    fn or9_compute_cost_known_tokens() {
        let tokens = TokenCounts {
            input: 1000,
            output: 500,
            cached: 0,
        };
        // 1000 * 1 + 500 * 2 = 1000 + 1000 = 2000
        let cost = compute_cost(tokens);
        assert_eq!(cost.microdollars, 2000);
    }

    #[test]
    fn or9b_compute_cost_zero_tokens() {
        let cost = compute_cost(TokenCounts::default());
        assert_eq!(cost.microdollars, 0);
    }

    // OR10: capabilities are well-formed
    #[test]
    fn or10_capabilities_are_well_formed() {
        let adapter = DeepSeekR1OpenRouter::new(SecretString::from("dummy"));
        let caps = adapter.capabilities();
        assert!(caps.is_well_formed());
        assert!(caps.contains(Capability::ToolCalling));
        assert!(caps.contains(Capability::StructuredOutput));
        assert!(caps.contains(Capability::ParallelToolCalls));
        assert!(!caps.contains(Capability::Vision));
        assert!(!caps.contains(Capability::Reasoning));
        assert!(!caps.contains(Capability::PromptCaching));
    }

    // OR11: 401 maps to auth invalid credential
    #[tokio::test]
    async fn or11_401_maps_to_auth_invalid_credential() {
        let server = MockServer::start().await;
        mount_mock(&server, 401, error_body("invalid_api_key")).await;

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
    async fn or11_403_maps_to_auth_forbidden() {
        let server = MockServer::start().await;
        mount_mock(&server, 403, error_body("permission_denied")).await;

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
    async fn or11_429_maps_to_rate_limit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(COMPLETIONS_ENDPOINT))
            .respond_with(
                ResponseTemplate::new(429)
                    .append_header("retry-after", "60")
                    .set_body_json(error_body("rate_limit_exceeded")),
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
                    retry_after_secs: Some(60)
                }
            ),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn or11_500_maps_to_provider_error() {
        let server = MockServer::start().await;
        mount_mock(&server, 500, error_body("server_error")).await;

        let adapter = make_adapter(&server.uri());
        let err = adapter
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect_err("should fail");

        assert!(
            matches!(err, AdapterError::Provider { status: 500, .. }),
            "got: {err}"
        );
    }

    // OR12: round-trip 200 response
    #[tokio::test]
    async fn or12_round_trip_200_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(COMPLETIONS_ENDPOINT))
            .and(header(
                reqwest::header::AUTHORIZATION.as_str(),
                format!("Bearer {TEST_KEY}"),
            ))
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
            matches!(&response.content[0], MessageContent::Text(t) if t == "Hello from DeepSeek!")
        );
        assert!(response.tool_calls.is_empty());
        assert_eq!(response.finish_reason, crate::FinishReason::EndTurn);
        assert_eq!(response.tokens.input, 10);
        assert_eq!(response.tokens.output, 5);
        // 10*1 + 5*2 = 20 µ$
        assert_eq!(response.cost.microdollars, 20);
    }

    // OR13: SECURITY — API key not in errors
    #[tokio::test]
    async fn or13_no_api_key_in_error_display() {
        let server = MockServer::start().await;
        mount_mock(&server, 401, error_body("invalid_api_key")).await;

        let adapter = make_adapter(&server.uri());
        let err = adapter
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect_err("should fail");

        let display = format!("{err}");
        let debug = format!("{err:?}");
        assert!(
            !display.contains(TEST_KEY),
            "key leaked in Display: {display}"
        );
        assert!(!debug.contains(TEST_KEY), "key leaked in Debug: {debug}");
        assert!(!display.contains("sk-or"), "key prefix leaked: {display}");
        assert!(!debug.contains("sk-or"), "key prefix leaked: {debug}");
    }

    // OR14: SECURITY — redirects not followed
    #[tokio::test]
    async fn or14_redirect_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(COMPLETIONS_ENDPOINT))
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
            .expect_err("302 should be surfaced as an error");

        assert!(
            matches!(
                err,
                AdapterError::Provider { .. } | AdapterError::Network { .. }
            ),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn or14b_redirect_sends_exactly_one_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(COMPLETIONS_ENDPOINT))
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

    // Additional: adapter id is stable
    #[test]
    fn adapter_id_is_stable() {
        let adapter = DeepSeekR1OpenRouter::new(SecretString::from("dummy"));
        assert_eq!(adapter.id(), "deepseek/deepseek-r1-0528@openrouter");
    }
}
