//! Shared infrastructure for adapters on the `OpenRouter` route.
//!
//! Every `OpenRouter`-hosted model (`DeepSeek` R1, GLM-5.2, and future additions)
//! speaks the same OpenAI-compatible endpoint, needs the same no-redirect
//! client policy, and maps HTTP status to [`AdapterError`] identically. This
//! module owns those trust-sensitive, must-stay-identical pieces so they live
//! in one audited place rather than copied per adapter. Per-model specifics —
//! wire model ID, capabilities, cost rates, provider-routing preferences —
//! stay in the concrete adapter.
//!
//! # Security
//!
//! The API key is sent as a bearer token and never appears in any returned
//! [`AdapterError`]. Redirects are disabled so a 3xx cannot silently re-POST
//! the body and bearer token to a redirect target.

use std::time::{Duration, Instant};

use secrecy::ExposeSecret as _;

use crate::openai_compat::{ChatRequest, ChatResponse};
use crate::{openai_compat, AdapterError, AuthKind};

/// Base URL for the `OpenRouter` route.
pub(crate) const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// Chat completions endpoint (OpenAI-compatible).
pub(crate) const COMPLETIONS_ENDPOINT: &str = "/chat/completions";

const HTTP_UNAUTHORIZED: u16 = 401;
const HTTP_FORBIDDEN: u16 = 403;
const HTTP_TOO_MANY_REQUESTS: u16 = 429;

/// Build the `reqwest` client every `OpenRouter` adapter uses.
///
/// Redirects are disabled: an `OpenRouter` 3xx must surface as an error rather
/// than silently re-POSTing the request body — which carries the bearer token —
/// to the redirect target. This policy is security-critical; adapter tests
/// assert a 302 is not followed.
#[expect(
    clippy::expect_used,
    reason = "TLS backend init failure at construction time is unrecoverable; \
              falling back to Client::new() would void the no-redirect invariant"
)]
pub(crate) fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect(
            "reqwest client construction must succeed (rustls-tls init failure is \
             unrecoverable; do not fall back to Client::new() which would void the \
             no-redirect policy)",
        )
}

fn parse_retry_after(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

/// POST a chat-completion request to `OpenRouter` and decode the response body.
///
/// Returns the decoded [`ChatResponse`] together with the round-trip latency
/// (measured from request send to response headers received, excluding body
/// decode). Maps HTTP status to [`AdapterError`] uniformly for every `OpenRouter`
/// adapter: 401 → `Auth` invalid, 403 → `Auth` forbidden, 429 → `RateLimit`
/// (with `Retry-After`), other 4xx → `BadRequest`, and every remaining
/// non-2xx status → `Provider`. That catch-all covers 5xx and — because the
/// client disables redirects — any 3xx the server returns instead of
/// following it (the no-redirect security tests rely on this: a 302 surfaces
/// as an error rather than a silent re-POST of the bearer token).
pub(crate) async fn post_completion(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &secrecy::SecretString,
    request: &ChatRequest<'_>,
) -> Result<(ChatResponse, Duration), AdapterError> {
    let url = format!("{base_url}{COMPLETIONS_ENDPOINT}");
    let start = Instant::now();
    let http_response = client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", api_key.expose_secret()),
        )
        .json(request)
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
                let retry_after_secs = parse_retry_after(&http_response);
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

    Ok((body, latency))
}
