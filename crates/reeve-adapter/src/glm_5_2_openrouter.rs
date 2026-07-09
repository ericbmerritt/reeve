//! `Glm52OpenRouter` adapter on the `openrouter` route.
//!
//! Serves Zhipu/Z.ai's GLM-5.2 through `OpenRouter`'s OpenAI-compatible chat
//! completions API. Shares the route machinery in [`crate::openrouter`] with
//! the `DeepSeek` adapter; the GLM-specific pieces are the wire model ID, the
//! declared capabilities, the cost rates, and a pinned provider-routing
//! preference.
//!
//! # Provider routing
//!
//! GLM-5.2 is served by ~20 hosts on `OpenRouter` at different price, latency,
//! and quantization (fp8 vs fp4) points. This adapter pins an fp8-favouring
//! order (Novita, then `GMICloud`) with fallbacks allowed, so those hosts are
//! preferred but a single provider outage does not fail the call. See ADR 004
//! for the deferred cross-provider *failover* design; this is provider
//! *preference* within one `OpenRouter` request, not runtime failover.
//!
//! # Security
//!
//! The API key is wrapped in [`secrecy::SecretString`] and never included in
//! error messages, log output, or `Debug` formatting (see [`crate::openrouter`]).

use crate::{
    openai_compat, openrouter, Adapter, AdapterError, Capabilities, Capability, CostEstimate,
    Message, Params, Response, TokenCounts, Tool,
};

// ── Route constants ────────────────────────────────────────────────────────────

/// Model string sent to `OpenRouter` on the wire.
const MODEL_ID: &str = "z-ai/glm-5.2";

/// Adapter identifier returned by [`Adapter::id`].
const ADAPTER_ID: &str = "z-ai/glm-5.2@openrouter";

/// Preferred `OpenRouter` provider order for GLM-5.2 (fp8 hosts first). Combined
/// with `allow_fallbacks: true`, so these are tried first but any other host
/// serving the model can still take the request.
const PROVIDER_ORDER: &[&str] = &["novita", "gmicloud"];

// ── Cost rates ─────────────────────────────────────────────────────────────────

/// Per-token cost rates for `z-ai/glm-5.2` on `OpenRouter`, in microdollars
/// (1 USD = `1_000_000` µ$).
///
/// Rates are adapter-local snapshots and coarse (the µ$/token integer model has
/// $1/M granularity). Approximate as of 2026-07 for the fp8 hosts this adapter
/// prefers: ~$0.5/M input, ~$2/M output. This is a best-effort estimate, not an
/// authoritative bill — the actual host (and its quantization/price) is chosen
/// by `OpenRouter` per request.
struct Glm52Rates;

impl Glm52Rates {
    /// ~$0.5 / M input tokens → 1 µ$/token (rounded up from 0.5).
    const INPUT_MICRODOLLARS_PER_TOKEN: u64 = 1;
    /// ~$2 / M output tokens → 2 µ$/token.
    const OUTPUT_MICRODOLLARS_PER_TOKEN: u64 = 2;
}

/// Compute the cost estimate for a single call from its [`TokenCounts`].
fn compute_cost(tokens: TokenCounts) -> CostEstimate {
    let micro: u64 = u64::from(tokens.input) * Glm52Rates::INPUT_MICRODOLLARS_PER_TOKEN
        + u64::from(tokens.output) * Glm52Rates::OUTPUT_MICRODOLLARS_PER_TOKEN;
    CostEstimate {
        microdollars: micro,
    }
}

// ── Glm52OpenRouter ─────────────────────────────────────────────────────────────

/// The `z-ai/glm-5.2@openrouter` adapter.
///
/// Routes through `OpenRouter`'s OpenAI-compatible chat completions endpoint,
/// preferring fp8 hosts. Construct with [`Glm52OpenRouter::new`] for production
/// use.
pub struct Glm52OpenRouter {
    client: reqwest::Client,
    api_key: secrecy::SecretString,
    base_url: String,
    capabilities: Capabilities,
}

impl Glm52OpenRouter {
    fn declared_capabilities() -> Capabilities {
        Capabilities::new()
            .with(Capability::ToolCalling)
            .with(Capability::ParallelToolCalls)
            .with(Capability::StructuredOutput)
            .with(Capability::Reasoning)
    }

    /// Construct a new adapter with the given `OpenRouter` API key.
    pub fn new(api_key: secrecy::SecretString) -> Self {
        Self::with_base_url(api_key, openrouter::BASE_URL.to_owned())
    }

    /// Construct an adapter with an explicit base URL.
    ///
    /// **Internal — production callers MUST use [`Glm52OpenRouter::new`].**
    /// Tests call this with a wiremock server URL. Never accept a base URL
    /// from outside this crate; `with_base_url` does not validate the scheme.
    pub(crate) fn with_base_url(api_key: secrecy::SecretString, base_url: String) -> Self {
        Self {
            client: openrouter::build_client(),
            api_key,
            base_url,
            capabilities: Self::declared_capabilities(),
        }
    }
}

#[async_trait::async_trait]
impl Adapter for Glm52OpenRouter {
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
        let mut wire_req = openai_compat::build_request(messages, tools, params, MODEL_ID);
        wire_req.provider = Some(openai_compat::ProviderPreferences {
            order: PROVIDER_ORDER.to_vec(),
            allow_fallbacks: true,
        });
        let (body, latency) =
            openrouter::post_completion(&self.client, &self.base_url, &self.api_key, &wire_req)
                .await?;
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
    use crate::openrouter::COMPLETIONS_ENDPOINT;
    use crate::{Capability, Message, MessageContent, Params, Role, TokenCounts};
    use secrecy::SecretString;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const TEST_KEY: &str = "sk-or-test-SECRET_DO_NOT_LEAK_GLM_12345";

    fn make_adapter(base_url: &str) -> Glm52OpenRouter {
        Glm52OpenRouter::with_base_url(SecretString::from(TEST_KEY), base_url.to_owned())
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
            "id": "chatcmpl-glm",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello from GLM-5.2!"
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

    // GLM1: compute_cost for known token counts
    #[test]
    fn glm1_compute_cost_known_tokens() {
        let tokens = TokenCounts {
            input: 1000,
            output: 500,
            cached: 0,
        };
        // 1000 * 1 + 500 * 2 = 2000
        assert_eq!(compute_cost(tokens).microdollars, 2000);
    }

    #[test]
    fn glm1b_compute_cost_zero_tokens() {
        assert_eq!(compute_cost(TokenCounts::default()).microdollars, 0);
    }

    // GLM2: capabilities are well-formed and include reasoning + tool calling
    #[test]
    fn glm2_capabilities_are_well_formed() {
        let adapter = Glm52OpenRouter::new(SecretString::from("dummy"));
        let caps = adapter.capabilities();
        assert!(caps.is_well_formed());
        assert!(caps.contains(Capability::ToolCalling));
        assert!(caps.contains(Capability::ParallelToolCalls));
        assert!(caps.contains(Capability::StructuredOutput));
        assert!(caps.contains(Capability::Reasoning));
        assert!(!caps.contains(Capability::Vision));
    }

    // GLM3: HTTP status mapping (same contract as every OpenRouter adapter)
    #[tokio::test]
    async fn glm3_401_maps_to_auth_invalid_credential() {
        let server = MockServer::start().await;
        mount_mock(&server, 401, error_body("invalid_api_key")).await;

        let err = make_adapter(&server.uri())
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect_err("should fail");

        assert!(
            matches!(
                err,
                AdapterError::Auth {
                    kind: crate::AuthKind::InvalidCredential
                }
            ),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn glm3_429_maps_to_rate_limit() {
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

        let err = make_adapter(&server.uri())
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
    async fn glm3_500_maps_to_provider_error() {
        let server = MockServer::start().await;
        mount_mock(&server, 500, error_body("server_error")).await;

        let err = make_adapter(&server.uri())
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect_err("should fail");

        assert!(
            matches!(err, AdapterError::Provider { status: 500, .. }),
            "got: {err}"
        );
    }

    // GLM4: round-trip 200 response with correct cost and text
    #[tokio::test]
    async fn glm4_round_trip_200_response() {
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

        let response = make_adapter(&server.uri())
            .call(&[user_message("hello")], &[], &base_params())
            .await
            .expect("should succeed");

        assert!(
            matches!(&response.content[0], MessageContent::Text(t) if t == "Hello from GLM-5.2!")
        );
        assert_eq!(response.tokens.input, 10);
        assert_eq!(response.tokens.output, 5);
        // 10*1 + 5*2 = 20 µ$
        assert_eq!(response.cost.microdollars, 20);
    }

    // GLM5: the pinned provider order lands in the request body
    #[tokio::test]
    async fn glm5_provider_order_in_request_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(COMPLETIONS_ENDPOINT))
            .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
            .mount(&server)
            .await;

        let _ = make_adapter(&server.uri())
            .call(&[user_message("hi")], &[], &base_params())
            .await
            .expect("should succeed");

        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["model"], "z-ai/glm-5.2");
        assert_eq!(
            body["provider"]["order"],
            serde_json::json!(["novita", "gmicloud"])
        );
        assert_eq!(body["provider"]["allow_fallbacks"], serde_json::json!(true));
    }

    // GLM6: SECURITY — API key never appears in error output
    #[tokio::test]
    async fn glm6_no_api_key_in_error_display() {
        let server = MockServer::start().await;
        mount_mock(&server, 401, error_body("invalid_api_key")).await;

        let err = make_adapter(&server.uri())
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
    }

    // GLM7: SECURITY — redirects are not followed (no-redirect client policy)
    #[tokio::test]
    async fn glm7_redirect_not_followed() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(COMPLETIONS_ENDPOINT))
            .respond_with(
                ResponseTemplate::new(302)
                    .append_header("location", "http://attacker.example.com/leak"),
            )
            .mount(&server)
            .await;

        let err = make_adapter(&server.uri())
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

    // GLM8: adapter id is stable
    #[test]
    fn glm8_adapter_id_is_stable() {
        let adapter = Glm52OpenRouter::new(SecretString::from("dummy"));
        assert_eq!(adapter.id(), "z-ai/glm-5.2@openrouter");
    }
}
