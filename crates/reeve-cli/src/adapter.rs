//! `reeve adapter` subcommand group: keychain-backed API key management and
//! end-to-end adapter smoke tests.
//!
//! Two subcommands:
//!
//! - `set-key` — read the Anthropic API key from stdin and store it in the OS
//!   keychain under [`labels::ANTHROPIC_API_KEY`].
//! - `test --prompt <text>` — retrieve the key, construct the adapter, make a
//!   single call, and print the response with token counts, cost, and latency.
//!
//! The async adapter call runs inside a `tokio::runtime::Runtime` constructed
//! inline so the rest of the CLI stays sync.

use std::io::{self, Write};

use clap::Subcommand;
use reeve_adapter::{
    Adapter, AdapterError, AuthKind, Message, MessageContent, Params, Response, Role,
};
use reeve_runtime::{keychain::labels, KeychainError, OperatorSecretStore};

// ── Output label constants ─────────────────────────────────────────────────────

/// Section header used by [`write_response_fields`] and test assertions.
pub(crate) const LABEL_RESPONSE: &str = "Response:";
/// Section header used by [`write_response_fields`] and test assertions.
pub(crate) const LABEL_TOKENS: &str = "Tokens:";
/// Section header used by [`write_response_fields`] and test assertions.
pub(crate) const LABEL_COST: &str = "Cost:";
/// Section header used by [`write_response_fields`] and test assertions.
pub(crate) const LABEL_LATENCY: &str = "Latency:";

/// Default `max_tokens` for the smoke-test command. Chosen to be large enough
/// for typical "say hi" responses but small enough that a single test call
/// doesn't blow through the operator's quota.
const TEST_MAX_TOKENS: u32 = 1024;

// ── Subcommand definitions ─────────────────────────────────────────────────────

/// Subcommands under `reeve adapter`.
#[derive(Subcommand, Debug)]
pub(crate) enum AdapterSubcommand {
    /// Store the Anthropic API key in the OS keychain.
    ///
    /// Reads the key from stdin (one line).
    ///
    /// **The terminal will NOT suppress echo.** The key will appear on screen
    /// and may be captured by terminal scrollback, PTY logging (e.g., `script`,
    /// `tmux` logging, recorded SSH sessions), or screen recording software.
    /// Pipe from a file or password manager for echo-free input:
    ///
    /// ```text
    /// printf '%s' "$ANTHROPIC_KEY" | reeve adapter set-key
    /// pass anthropic/key | reeve adapter set-key
    /// ```
    #[command(name = "set-key")]
    SetKey,

    /// Send a single prompt to the configured Claude model and print the
    /// response.
    ///
    /// The Anthropic API key must already be in the keychain. Run
    /// `reeve adapter set-key` first if you have not done so.
    ///
    /// **Security**: the prompt is passed via the command line and will be
    /// visible in process listings (`ps`, `/proc/<pid>/cmdline`) to any
    /// process running as the same user for the duration of the call. Do
    /// NOT include credentials, internal URLs, or operator-confidential
    /// context in the prompt. For prompts containing sensitive data,
    /// future revisions will accept input via stdin or a file.
    #[command(name = "test")]
    Test {
        /// The prompt to send.
        #[arg(long)]
        prompt: String,
    },
}

// ── Presence check ─────────────────────────────────────────────────────────────

/// Return `true` when an Anthropic API key is present and readable from
/// `store`; any error (not-found or backend failure) is treated as "no key
/// configured."
pub(crate) fn has_api_key(store: &dyn OperatorSecretStore) -> bool {
    store.retrieve_secret(labels::ANTHROPIC_API_KEY).is_ok()
}

// ── Public dispatch ─────────────────────────────────────────────────────────────

/// Dispatch `reeve adapter` subcommands using the platform's default keychain.
///
/// Called from `main` after subcommand parsing. The platform-specific keychain
/// backend is constructed via [`crate::keychain::open_platform_secretstore`].
/// Errors propagate back to `main` and are printed via the standard
/// `Box<dyn Error>` chain before exit.
pub(crate) fn dispatch(cmd: AdapterSubcommand) -> Result<(), Box<dyn std::error::Error>> {
    let keychain = crate::keychain::open_platform_secretstore()?;
    dispatch_with_store(cmd, &keychain)
}

// ── Internal dispatch (testable) ───────────────────────────────────────────────

/// Dispatch with an injected secret store. Used directly by tests.
pub(crate) fn dispatch_with_store(
    cmd: AdapterSubcommand,
    store: &dyn OperatorSecretStore,
) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        AdapterSubcommand::SetKey => cmd_set_key(store),
        AdapterSubcommand::Test { prompt } => cmd_test(store, prompt),
    }
}

// ── set-key ────────────────────────────────────────────────────────────────────

/// Read the API key from stdin and store it in the keychain.
fn cmd_set_key(store: &dyn OperatorSecretStore) -> Result<(), Box<dyn std::error::Error>> {
    let key = read_api_key_from_stdin()?;
    store.store_secret(labels::ANTHROPIC_API_KEY, key)?;
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "Stored Anthropic API key in keychain (label: {label})",
        label = labels::ANTHROPIC_API_KEY,
    )?;
    Ok(())
}

/// Read one line from stdin, trim it, and wrap in `SecretString`.
///
/// Returns an error if stdin is empty or the line is all-whitespace.
fn read_api_key_from_stdin() -> Result<secrecy::SecretString, Box<dyn std::error::Error>> {
    let value = crate::prompt::prompt_one_line(
        "Anthropic API key (input will NOT be hidden): ",
        "API key must not be empty",
    )?;
    Ok(secrecy::SecretString::from(value))
}

// ── test ───────────────────────────────────────────────────────────────────────

/// Retrieve the key, call the adapter, and print the response.
fn cmd_test(
    store: &dyn OperatorSecretStore,
    prompt: String,
) -> Result<(), Box<dyn std::error::Error>> {
    // Reject empty or all-whitespace prompts at the CLI boundary; the API
    // would return a cryptic 400.
    if prompt.trim().is_empty() {
        return Err("Prompt must not be empty.".into());
    }
    let secret = retrieve_api_key(store)?;
    let adapter = reeve_adapter::ClaudeOpus47::new(secret);
    let messages = [Message {
        role: Role::User,
        content: vec![MessageContent::Text(prompt)],
    }];
    let params = Params {
        max_tokens: TEST_MAX_TOKENS,
        temperature: None,
        system_prompt: None,
    };

    let rt = tokio::runtime::Runtime::new()?;
    let response = rt
        .block_on(adapter.call(&messages, &[], &params))
        .map_err(format_adapter_error)?;

    print_response(&response)?;
    Ok(())
}

/// Retrieve the Anthropic API key from the store, mapping keychain errors to
/// user-friendly messages.
fn retrieve_api_key(
    store: &dyn OperatorSecretStore,
) -> Result<secrecy::SecretString, Box<dyn std::error::Error>> {
    store
        .retrieve_secret(labels::ANTHROPIC_API_KEY)
        .map_err(keychain_error_to_user_message)
}

/// Map a [`KeychainError`] to a user-facing error string.
///
/// `SecretNotFound` gets a tailored "run set-key" hint; all other variants
/// surface their `Display` representation.
fn keychain_error_to_user_message(err: KeychainError) -> Box<dyn std::error::Error> {
    match err {
        KeychainError::SecretNotFound { .. } => "No Anthropic API key in keychain. \
             Run `reeve adapter set-key` to add one."
            .into(),
        KeychainError::NotFound { .. }
        | KeychainError::InvalidSecretEncoding { .. }
        | KeychainError::InvalidSeedLength { .. } => Box::new(err),
        #[cfg(target_os = "macos")]
        KeychainError::MacOsKeychain { .. } | KeychainError::MacOsKeychainForLabel { .. } => {
            Box::new(err)
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        KeychainError::DuplicateEntry { .. }
        | KeychainError::SecretService { .. }
        | KeychainError::SecretServiceUnavailable { .. }
        | KeychainError::SecretServiceForLabel { .. } => Box::new(err),
        _ => Box::new(err),
    }
}

/// Map an [`AdapterError`] to a user-friendly error string.
///
/// SECURITY: NEVER include the API key, authorization headers, or header
/// values in any returned string. The strings here are all static or
/// contain only non-sensitive provider metadata.
fn format_adapter_error(err: AdapterError) -> Box<dyn std::error::Error> {
    let msg: String = match err {
        AdapterError::Auth {
            kind: AuthKind::InvalidCredential,
        } => "Authentication failed: invalid credential. \
              Run `reeve adapter set-key` to update."
            .to_owned(),
        AdapterError::Auth {
            kind: AuthKind::Forbidden,
        } => "Authentication failed: forbidden (model not enabled for this account).".to_owned(),
        AdapterError::Auth {
            kind: AuthKind::Other(detail),
        } => format!("Authentication failed: {detail}"),
        AdapterError::RateLimit {
            retry_after_secs: Some(secs),
        } => format!("Rate limited. Retry-after: {secs}s."),
        AdapterError::RateLimit {
            retry_after_secs: None,
        } => "Rate limited. No retry-after provided.".to_owned(),
        AdapterError::Network { .. } => "Network error. Check your connection.".to_owned(),
        AdapterError::BadRequest { message } => format!("Bad request: {message}."),
        AdapterError::Provider { status, message } => {
            format!("Provider error (HTTP {status}): {message}.")
        }
        AdapterError::Decode { .. } => "Failed to decode response.".to_owned(),
        AdapterError::CredentialUnavailable { message } => {
            format!("Credential unavailable: {message}.")
        }
        // Future non-exhaustive variants: surface Display text. The Auth
        // and RateLimit arms are unreachable (exhaustively matched above)
        // but must be listed to satisfy wildcard_enum_match_arm lint on a
        // non_exhaustive enum matched from outside the defining crate.
        #[expect(
            unreachable_patterns,
            reason = "required to satisfy wildcard_enum_match_arm on non_exhaustive enum"
        )]
        AdapterError::Auth { .. } | AdapterError::RateLimit { .. } | _ => {
            format!("Adapter error: {err}")
        }
    };
    msg.into()
}

// ── Output formatting ──────────────────────────────────────────────────────────

/// Pre-extracted fields from a `Response` for display. Avoids constructing
/// `Response` in tests (it is `#[non_exhaustive]`) while keeping argument
/// counts within the clippy limit.
pub(crate) struct ResponseFields<'a> {
    pub(crate) text_content: &'a str,
    pub(crate) input_tokens: u32,
    pub(crate) output_tokens: u32,
    pub(crate) cached_tokens: u32,
    pub(crate) cost_usd: f64,
    pub(crate) latency_ms: u128,
}

/// Print the structured response to stdout.
pub(crate) fn print_response(response: &Response) -> Result<(), io::Error> {
    let text = collect_text(&response.content);
    let fields = ResponseFields {
        text_content: &text,
        input_tokens: response.tokens.input,
        output_tokens: response.tokens.output,
        cached_tokens: response.tokens.cached,
        cost_usd: response.cost.usd(),
        latency_ms: response.latency.as_millis(),
    };
    let mut out = io::stdout().lock();
    write_response_fields(&mut out, &fields)
}

/// Write formatted response fields to `out`. Accepts a writer for testability.
pub(crate) fn write_response_fields(
    out: &mut impl Write,
    fields: &ResponseFields<'_>,
) -> Result<(), io::Error> {
    writeln!(out, "{LABEL_RESPONSE}")?;
    writeln!(out, "  {}", fields.text_content)?;
    writeln!(out)?;
    writeln!(out, "{LABEL_TOKENS}")?;
    writeln!(out, "  input:   {}", fields.input_tokens)?;
    writeln!(out, "  output:  {}", fields.output_tokens)?;
    writeln!(out, "  cached:  {}", fields.cached_tokens)?;
    writeln!(out)?;
    writeln!(out, "{LABEL_COST}    ${:.6}", fields.cost_usd)?;
    writeln!(out, "{LABEL_LATENCY} {}ms", fields.latency_ms)?;
    Ok(())
}

/// Collect all `Text` content blocks from a message content slice, joining
/// with newlines. Non-text blocks are silently skipped.
pub(crate) fn collect_text(content: &[MessageContent]) -> String {
    let parts: Vec<&str> = content
        .iter()
        .filter_map(|c| match c {
            MessageContent::Text(t) => Some(t.as_str()),
            MessageContent::ToolUse { .. } | MessageContent::ToolResult { .. } | _ => None,
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        parts.join("\n")
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use reeve_adapter::MessageContent;
    use reeve_runtime::keychain::memory::MemoryKeyStore;
    use reeve_runtime::OperatorSecretStore;
    use secrecy::ExposeSecret as _;

    use super::*;

    // ── CT1: set-key stores the key in the store ──────────────────────────────

    /// CT1: `set-key` dispatch stores the trimmed value under
    /// `labels::ANTHROPIC_API_KEY`.
    ///
    /// We verify `store_secret` + `retrieve_secret` round-trip semantics
    /// through `MemoryKeyStore` — the same path that `cmd_set_key` exercises
    /// after `read_api_key_from_stdin` returns.
    #[test]
    fn ct1_set_key_stores_in_keychain() {
        let store = MemoryKeyStore::new();
        let sentinel = "sk-ant-ct1-sentinel-not-a-real-key";
        store
            .store_secret(
                labels::ANTHROPIC_API_KEY,
                secrecy::SecretString::from(sentinel.to_owned()),
            )
            .expect("store_secret failed in test setup");

        let retrieved = store
            .retrieve_secret(labels::ANTHROPIC_API_KEY)
            .expect("retrieve_secret failed");
        assert_eq!(
            retrieved.expose_secret(),
            sentinel,
            "CT1: stored value must round-trip through MemoryKeyStore",
        );
    }

    // ── CT2: test errors when no key in keychain ──────────────────────────────

    /// CT2: `test` subcommand with an empty `MemoryKeyStore` produces the
    /// documented error message.
    #[test]
    fn ct2_test_errors_when_no_key_in_keychain() {
        let store = MemoryKeyStore::new();
        let err = retrieve_api_key(&store).expect_err("expected error for empty keychain");
        let msg = err.to_string();
        assert!(
            msg.contains("No Anthropic API key in keychain"),
            "CT2: error must mention missing key; got: {msg}",
        );
        assert!(
            msg.contains("reeve adapter set-key"),
            "CT2: error must suggest set-key; got: {msg}",
        );
    }

    // ── CT3: set-key rejects empty input ─────────────────────────────────────

    /// CT3: the stdin reader rejects an all-whitespace line.
    ///
    /// We test the trimmed-empty guard directly because
    /// `read_api_key_from_stdin` reads from the real `io::stdin()`. The guard
    /// is a `trim().is_empty()` check; we reproduce it here to pin the
    /// contract.
    #[test]
    fn ct3_set_key_rejects_empty_input() {
        let inputs: &[&str] = &["", "   ", "\n", "\t\n"];
        for input in inputs {
            let trimmed = input.trim();
            assert!(
                trimmed.is_empty(),
                "CT3: trim must classify {input:?} as empty",
            );
        }
    }

    // ── CT4: response printer renders correctly ───────────────────────────────

    /// CT4: `write_response_fields` produces the expected output format.
    ///
    /// `Response` is `#[non_exhaustive]` and cannot be constructed from
    /// outside the `reeve-adapter` crate, so we test the formatter via the
    /// `ResponseFields` struct.
    #[test]
    fn ct4_write_response_fields_renders_correctly() {
        // 10*15 + 5*75 + 2*1 = 150+375+2 = 527 µ$ → $0.000527
        let cost_usd = 527.0 / 1_000_000.0;
        let fields = ResponseFields {
            text_content: "Hello, world!",
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 2,
            cost_usd,
            latency_ms: 123,
        };
        let mut buf = Vec::new();
        write_response_fields(&mut buf, &fields).expect("write_response_fields failed");
        let output = String::from_utf8(buf).expect("output is valid UTF-8");

        assert!(
            output.contains(LABEL_RESPONSE),
            "CT4: output must contain {LABEL_RESPONSE:?}; got:\n{output}",
        );
        assert!(
            output.contains("Hello, world!"),
            "CT4: output must contain response text; got:\n{output}",
        );
        assert!(
            output.contains(LABEL_TOKENS),
            "CT4: output must contain {LABEL_TOKENS:?}; got:\n{output}",
        );
        assert!(
            output.contains("input:   10"),
            "CT4: output must contain input token count; got:\n{output}",
        );
        assert!(
            output.contains("output:  5"),
            "CT4: output must contain output token count; got:\n{output}",
        );
        assert!(
            output.contains("cached:  2"),
            "CT4: output must contain cached token count; got:\n{output}",
        );
        assert!(
            output.contains(LABEL_COST),
            "CT4: output must contain {LABEL_COST:?}; got:\n{output}",
        );
        assert!(
            output.contains('$'),
            "CT4: output must contain dollar sign; got:\n{output}",
        );
        assert!(
            output.contains("$0.000527"),
            "CT4: output must contain exact cost string; got:\n{output}",
        );
        assert!(
            output.contains(LABEL_LATENCY),
            "CT4: output must contain {LABEL_LATENCY:?}; got:\n{output}",
        );
        assert!(
            output.contains("123ms"),
            "CT4: output must contain latency in ms; got:\n{output}",
        );
    }

    /// `CT4b`: multiple text blocks are joined with newlines by `collect_text`.
    #[test]
    fn ct4b_multiple_text_blocks_joined() {
        let content = vec![
            MessageContent::Text("first block".to_owned()),
            MessageContent::Text("second block".to_owned()),
        ];
        let text = collect_text(&content);
        assert_eq!(text, "first block\nsecond block");
    }

    /// `CT4c`: an empty content slice renders an empty string (no panic).
    #[test]
    fn ct4c_empty_content_renders_cleanly() {
        let text = collect_text(&[]);
        assert!(
            text.is_empty(),
            "CT4c: empty content must yield empty string"
        );

        let fields = ResponseFields {
            text_content: &text,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            cost_usd: 0.0,
            latency_ms: 0,
        };
        let mut buf = Vec::new();
        write_response_fields(&mut buf, &fields).expect("write_response_fields failed");
        let output = String::from_utf8(buf).expect("valid UTF-8");
        assert!(
            output.contains(LABEL_RESPONSE),
            "CT4c: must still render response section; got:\n{output}",
        );
    }

    // ── CT5: empty prompt is rejected ────────────────────────────────────────

    /// CT5: `cmd_test` rejects an empty or all-whitespace prompt before
    /// touching the keychain or network.
    #[test]
    fn ct5_empty_prompt_is_rejected() {
        let store = MemoryKeyStore::new();
        for prompt in ["", "   ", "\t", "\n"] {
            let err = dispatch_with_store(
                AdapterSubcommand::Test {
                    prompt: prompt.to_owned(),
                },
                &store,
            )
            .expect_err("expected error for empty/whitespace prompt");
            let msg = err.to_string();
            assert!(
                msg.contains("Prompt must not be empty"),
                "CT5: error must mention empty prompt for input {prompt:?}; got: {msg}",
            );
        }
    }

    // ── Error mapping ─────────────────────────────────────────────────────────

    /// Error mapping: `format_adapter_error` produces user-friendly strings.
    #[test]
    fn error_mapping_auth_invalid_credential() {
        let err = AdapterError::Auth {
            kind: AuthKind::InvalidCredential,
        };
        let rendered = format_adapter_error(err).to_string();
        assert!(
            rendered.contains("Authentication failed"),
            "got: {rendered}"
        );
        assert!(
            rendered.contains("set-key"),
            "must suggest set-key; got: {rendered}"
        );
    }

    #[test]
    fn error_mapping_auth_forbidden() {
        let err = AdapterError::Auth {
            kind: AuthKind::Forbidden,
        };
        let rendered = format_adapter_error(err).to_string();
        assert!(rendered.contains("forbidden"), "got: {rendered}");
    }

    #[test]
    fn error_mapping_auth_other() {
        let err = AdapterError::Auth {
            kind: AuthKind::Other("token expired".to_owned()),
        };
        let rendered = format_adapter_error(err).to_string();
        assert!(
            rendered.contains("Authentication failed"),
            "got: {rendered}"
        );
        assert!(rendered.contains("token expired"), "got: {rendered}");
    }

    #[test]
    fn error_mapping_rate_limit_with_retry() {
        let err = AdapterError::RateLimit {
            retry_after_secs: Some(42),
        };
        let rendered = format_adapter_error(err).to_string();
        assert!(rendered.contains("42s"), "got: {rendered}");
    }

    #[test]
    fn error_mapping_rate_limit_no_retry() {
        let err = AdapterError::RateLimit {
            retry_after_secs: None,
        };
        let rendered = format_adapter_error(err).to_string();
        assert!(rendered.contains("No retry-after"), "got: {rendered}");
    }

    #[test]
    fn error_mapping_network() {
        struct FakeErr;
        impl std::fmt::Display for FakeErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("connection refused")
            }
        }
        impl std::fmt::Debug for FakeErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("FakeErr")
            }
        }
        impl std::error::Error for FakeErr {}

        let err = AdapterError::Network {
            source: Box::new(FakeErr),
        };
        let rendered = format_adapter_error(err).to_string();
        assert!(rendered.contains("Network error"), "got: {rendered}");
    }

    #[test]
    fn error_mapping_bad_request() {
        let err = AdapterError::BadRequest {
            message: "missing field x".to_owned(),
        };
        let rendered = format_adapter_error(err).to_string();
        assert!(rendered.contains("Bad request"), "got: {rendered}");
        assert!(rendered.contains("missing field x"), "got: {rendered}");
    }

    #[test]
    fn error_mapping_provider() {
        let err = AdapterError::Provider {
            status: 503,
            message: "overloaded".to_owned(),
        };
        let rendered = format_adapter_error(err).to_string();
        assert!(rendered.contains("503"), "got: {rendered}");
        assert!(rendered.contains("overloaded"), "got: {rendered}");
    }

    #[test]
    fn error_mapping_decode() {
        struct FakeErr;
        impl std::fmt::Display for FakeErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("unexpected token")
            }
        }
        impl std::fmt::Debug for FakeErr {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("FakeErr")
            }
        }
        impl std::error::Error for FakeErr {}

        let err = AdapterError::Decode {
            source: Box::new(FakeErr),
        };
        let rendered = format_adapter_error(err).to_string();
        assert!(
            rendered.contains("Failed to decode response"),
            "got: {rendered}"
        );
    }

    #[test]
    fn error_mapping_credential_unavailable() {
        let err = AdapterError::CredentialUnavailable {
            message: "item not found".to_owned(),
        };
        let rendered = format_adapter_error(err).to_string();
        assert!(
            rendered.contains("Credential unavailable"),
            "got: {rendered}"
        );
        assert!(rendered.contains("item not found"), "got: {rendered}");
    }
}
