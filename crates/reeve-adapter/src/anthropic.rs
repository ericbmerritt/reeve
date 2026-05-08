//! Anthropic Messages API wire types and request/response translation.
//!
//! All types here are `pub(crate)`: they are private implementation details.
//! Consumers of the adapter crate interact exclusively through the [`Adapter`]
//! trait surface defined in [`lib.rs`].

use crate::{AdapterError, FinishReason, MessageContent, Role, TokenCounts, Tool, ToolCall};

// ── Wire request types ─────────────────────────────────────────────────────────

/// Request body for `POST /v1/messages`.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct MessagesRequest<'a> {
    pub(crate) model: &'a str,
    pub(crate) max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system: Option<&'a str>,
    pub(crate) messages: Vec<MessagesRequestMessage<'a>>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub(crate) tools: Vec<MessagesRequestTool<'a>>,
}

/// A single message in the request conversation history.
///
/// `content` is always serialized as an array of typed blocks so a single
/// turn can carry mixed text and tool-use / tool-result blocks. Anthropic
/// also accepts a plain string for text-only turns; we use the array form
/// uniformly for simplicity.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct MessagesRequestMessage<'a> {
    pub(crate) role: &'a str,
    pub(crate) content: Vec<MessagesRequestContent<'a>>,
}

/// A single content block in a request message.
///
/// Mirrors Anthropic's wire shape: `{ "type": "text", "text": "..." }`,
/// `{ "type": "tool_use", "id": "...", "name": "...", "input": {...} }`, or
/// `{ "type": "tool_result", "tool_use_id": "...", "content": "...",
///   "is_error": false }`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum MessagesRequestContent<'a> {
    Text {
        text: &'a str,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a serde_json::Value,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: &'a str,
        is_error: bool,
    },
}

/// A tool definition forwarded to the model.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct MessagesRequestTool<'a> {
    pub(crate) name: &'a str,
    pub(crate) description: &'a str,
    pub(crate) input_schema: &'a serde_json::Value,
}

// ── Wire response types ────────────────────────────────────────────────────────

/// Response body from `POST /v1/messages`.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct MessagesResponse {
    pub(crate) content: Vec<MessagesResponseContent>,
    pub(crate) stop_reason: String,
    pub(crate) usage: MessagesResponseUsage,
}

/// A content block in the response.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum MessagesResponseContent {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
}

/// Token usage reported by Anthropic.
#[expect(
    clippy::struct_field_names,
    reason = "field names are dictated by the Anthropic wire format; renaming would break serde deserialization"
)]
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct MessagesResponseUsage {
    pub(crate) input_tokens: u32,
    pub(crate) output_tokens: u32,
    /// Tokens written to the prompt cache this call. Parsed from the wire
    /// format for completeness; the walking-skeleton cost model only uses
    /// `cache_read_input_tokens` for the `cached` field in [`TokenCounts`].
    /// A future billing layer may use this at a different rate.
    #[serde(default)]
    #[expect(
        dead_code,
        reason = "deserialized from wire but not yet used in cost model; \
                  future billing layer will consume this"
    )]
    pub(crate) cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    pub(crate) cache_read_input_tokens: Option<u32>,
}

/// Anthropic error response envelope.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ErrorResponse {
    pub(crate) error: ErrorDetail,
}

/// Detail block inside an Anthropic error response.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ErrorDetail {
    #[serde(rename = "type")]
    pub(crate) error_type: String,
    #[expect(dead_code, reason = "kept for completeness; we emit only error_type")]
    pub(crate) message: String,
}

// ── TranslationError ──────────────────────────────────────────────────────────

/// Error produced while translating Reeve types into Anthropic wire format.
///
/// If `MessageContent` gains a non-Text variant, add an `UnsupportedContent`
/// variant or its specific equivalent here.
#[non_exhaustive]
#[derive(Debug)]
pub(crate) enum TranslationError {
    /// Multiple `Role::System` messages were supplied. Anthropic accepts only
    /// one system prompt (the `system` field on the request body).
    MultipleSystemMessages,
    /// A `Role::System` message contained a non-text block, or more than one
    /// block. The system prompt must be a single text block.
    SystemMessageNotText,
}

impl std::fmt::Display for TranslationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MultipleSystemMessages => {
                write!(
                    f,
                    "multiple system-role messages supplied; Anthropic accepts only one"
                )
            }
            Self::SystemMessageNotText => {
                write!(f, "system-role message must be a single text block")
            }
        }
    }
}

impl std::error::Error for TranslationError {}

impl From<TranslationError> for AdapterError {
    fn from(err: TranslationError) -> Self {
        Self::BadRequest {
            message: err.to_string(),
        }
    }
}

// ── Request translation ────────────────────────────────────────────────────────

/// Translate one Reeve [`MessageContent`] block into the Anthropic wire shape.
fn translate_content(block: &MessageContent) -> MessagesRequestContent<'_> {
    match block {
        MessageContent::Text(t) => MessagesRequestContent::Text { text: t.as_str() },
        MessageContent::ToolUse { id, name, input } => MessagesRequestContent::ToolUse {
            id: id.as_str(),
            name: name.as_str(),
            input,
        },
        MessageContent::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => MessagesRequestContent::ToolResult {
            tool_use_id: tool_use_id.as_str(),
            content: content.as_str(),
            is_error: *is_error,
        },
    }
}

/// Extract a system-prompt string from a single-block, text-only message.
///
/// System prompts must be plain text. Returns `None` if the message contains
/// any non-text block or has more than one block.
fn extract_system_text(content: &[MessageContent]) -> Option<&str> {
    if content.len() != 1 {
        return None;
    }
    match &content[0] {
        MessageContent::Text(t) => Some(t.as_str()),
        MessageContent::ToolUse { .. } | MessageContent::ToolResult { .. } => None,
    }
}

/// Build a wire request from Reeve's internal types.
///
/// System prompt resolution: if `params.system_prompt` is `Some`, that
/// value wins. Otherwise, a single `Role::System` message in `messages`
/// is promoted to the wire `system` field. Multiple system-role
/// messages are an error (`TranslationError::MultipleSystemMessages`).
///
/// Note: an empty `messages` slice is forwarded as-is; Anthropic
/// rejects empty `messages` arrays with HTTP 400. The adapter does
/// not pre-validate message non-emptiness — caller responsibility.
///
/// # Errors
///
/// Returns [`TranslationError::MultipleSystemMessages`] if more than one
/// `Role::System` message is present.
pub(crate) fn build_request<'a>(
    messages: &'a [crate::Message],
    tools: &'a [Tool],
    params: &'a crate::Params,
    model: &'a str,
) -> Result<MessagesRequest<'a>, TranslationError> {
    // Params.system_prompt takes precedence; system-role messages in the slice
    // are also supported (one, at most). If both are present we treat
    // params.system_prompt as the canonical system prompt and ignore any
    // system-role message — but we still validate there is at most one.
    let system_from_messages: Option<&'a str> = extract_system(messages)?;

    let system: Option<&'a str> = params.system_prompt.as_deref().or(system_from_messages);

    let mut wire_messages: Vec<MessagesRequestMessage<'_>> = Vec::new();
    for msg in messages {
        if msg.role == Role::System {
            // System messages become the top-level `system` field, not a
            // conversation turn.
            continue;
        }
        let role_str = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => unreachable!("system filtered above"),
        };
        let blocks: Vec<MessagesRequestContent<'_>> =
            msg.content.iter().map(translate_content).collect();
        wire_messages.push(MessagesRequestMessage {
            role: role_str,
            content: blocks,
        });
    }

    let wire_tools: Vec<MessagesRequestTool<'_>> = tools
        .iter()
        .map(|t| MessagesRequestTool {
            name: t.name.as_str(),
            description: t.description.as_str(),
            input_schema: &t.input_schema,
        })
        .collect();

    Ok(MessagesRequest {
        model,
        max_tokens: params.max_tokens,
        temperature: params.temperature.map(crate::Temperature::value),
        system,
        messages: wire_messages,
        tools: wire_tools,
    })
}

/// Walk `messages` and extract the text of the single `Role::System` message.
///
/// Returns `Ok(None)` if no system message exists, `Ok(Some(&str))` for
/// exactly one, and `Err(TranslationError::MultipleSystemMessages)` for two or
/// more. A system-role message that does not consist of a single text block
/// is rejected as `Err(TranslationError::SystemMessageNotText)`.
fn extract_system(messages: &[crate::Message]) -> Result<Option<&str>, TranslationError> {
    let mut found: Option<&str> = None;
    for msg in messages {
        if msg.role != Role::System {
            continue;
        }
        let text =
            extract_system_text(&msg.content).ok_or(TranslationError::SystemMessageNotText)?;
        if found.is_some() {
            return Err(TranslationError::MultipleSystemMessages);
        }
        found = Some(text);
    }
    Ok(found)
}

// ── Response translation ───────────────────────────────────────────────────────

/// Parse an Anthropic response into Reeve's internal `Response` shape.
///
/// Today this function is infallible: `MessageContent` only has the
/// `Text` variant, so wire `text` blocks always translate cleanly.
/// When new wire content types land that have no Reeve internal
/// representation, this function should return `Result` and surface
/// the unrepresentable variant as an explicit error.
pub(crate) fn parse_response(
    body: MessagesResponse,
) -> (
    Vec<MessageContent>,
    Vec<ToolCall>,
    FinishReason,
    TokenCounts,
) {
    let mut content: Vec<MessageContent> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in body.content {
        match block {
            MessagesResponseContent::Text { text } => {
                content.push(MessageContent::Text(text));
            }
            MessagesResponseContent::ToolUse { id, name, input } => {
                tool_calls.push(ToolCall {
                    id,
                    name,
                    arguments: input,
                });
            }
        }
    }

    let finish_reason = map_stop_reason(&body.stop_reason);

    let tokens = TokenCounts {
        input: body.usage.input_tokens,
        output: body.usage.output_tokens,
        // cache_creation tokens are billed at a different rate; for the
        // walking-skeleton cost model we track only the cache-read amount.
        cached: body.usage.cache_read_input_tokens.unwrap_or(0),
    };

    (content, tool_calls, finish_reason, tokens)
}

/// Map Anthropic's `stop_reason` string to [`FinishReason`].
fn map_stop_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" => FinishReason::EndTurn,
        "max_tokens" => FinishReason::MaxTokens,
        "tool_use" => FinishReason::ToolUse,
        "stop_sequence" => FinishReason::StopSequence,
        _ => FinishReason::Other,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, MessageContent, Params, Role, Temperature, Tool};
    use serde_json::json;

    fn user_text(text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![MessageContent::Text(text.to_owned())],
        }
    }

    fn system_text(text: &str) -> Message {
        Message {
            role: Role::System,
            content: vec![MessageContent::Text(text.to_owned())],
        }
    }

    fn assistant_text(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![MessageContent::Text(text.to_owned())],
        }
    }

    fn base_params() -> Params {
        Params {
            max_tokens: 1024,
            temperature: None,
            system_prompt: None,
        }
    }

    // ── AT1: build_request happy path ────────────────────────────────────────

    #[test]
    fn at1_build_request_simple_user_message() {
        let messages = [user_text("hello")];
        let params = base_params();
        let req =
            build_request(&messages, &[], &params, "claude-opus-4-7").expect("should succeed");

        assert_eq!(req.model, "claude-opus-4-7");
        assert_eq!(req.max_tokens, 1024);
        assert!(req.system.is_none());
        assert!(req.temperature.is_none());
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
        assert_eq!(req.messages[0].content.len(), 1);
        assert!(matches!(
            &req.messages[0].content[0],
            MessagesRequestContent::Text { text } if *text == "hello",
        ));
        assert!(req.tools.is_empty());
    }

    // ── AT2: build_request with system prompt ────────────────────────────────

    #[test]
    fn at2_build_request_system_from_params() {
        let messages = [user_text("hi")];
        let params = Params {
            system_prompt: Some("You are helpful.".to_owned()),
            ..base_params()
        };
        let req =
            build_request(&messages, &[], &params, "claude-opus-4-7").expect("should succeed");

        assert_eq!(req.system, Some("You are helpful."));
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn at2b_build_request_system_from_message() {
        let messages = [system_text("Be concise."), user_text("hi")];
        let params = base_params();
        let req =
            build_request(&messages, &[], &params, "claude-opus-4-7").expect("should succeed");

        assert_eq!(req.system, Some("Be concise."));
        // System message should not appear in the messages array.
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, "user");
    }

    #[test]
    fn at2c_build_request_params_system_overrides_message_system() {
        let messages = [system_text("From message."), user_text("hi")];
        let params = Params {
            system_prompt: Some("From params.".to_owned()),
            ..base_params()
        };
        let req =
            build_request(&messages, &[], &params, "claude-opus-4-7").expect("should succeed");

        assert_eq!(req.system, Some("From params."));
    }

    // ── AT3: build_request rejects multiple system messages ─────────────────

    #[test]
    fn at3_build_request_rejects_multiple_system_messages() {
        let messages = [system_text("first"), user_text("hi"), system_text("second")];
        let params = base_params();
        let result = build_request(&messages, &[], &params, "claude-opus-4-7");
        assert!(
            matches!(result, Err(TranslationError::MultipleSystemMessages)),
            "expected MultipleSystemMessages, got {result:?}"
        );
    }

    // ── AT4: build_request with non-Text content ─────────────────────────────
    //
    // `MessageContent` is `#[non_exhaustive]` with only a `Text` variant;
    // no non-Text variant can be constructed outside this crate without unsafe
    // code. `TranslationError::UnsupportedContent` was removed (C2) because it
    // was unreachable. When a future variant (Image, ToolResult, …) is added,
    // add an appropriate variant to `TranslationError` and test it here.

    // ── AT5: build_request with tools ────────────────────────────────────────

    #[test]
    fn at5_build_request_with_tools() {
        let schema = json!({ "type": "object", "properties": { "q": { "type": "string" } } });
        let tools = [Tool {
            name: "web_search".to_owned(),
            description: "Search the web.".to_owned(),
            input_schema: schema.clone(),
        }];
        let messages = [user_text("search for rust")];
        let params = base_params();
        let req =
            build_request(&messages, &tools, &params, "claude-opus-4-7").expect("should succeed");

        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].name, "web_search");
        assert_eq!(req.tools[0].description, "Search the web.");
        assert_eq!(*req.tools[0].input_schema, schema);
    }

    // ── AT5b: build_request with tool_use and tool_result blocks ─────────────

    /// `AT5b`: an assistant turn carrying `ToolUse` blocks and a follow-up
    /// user turn carrying `ToolResult` blocks both translate to the wire shape
    /// with the correct `type` discriminator and field set.
    #[test]
    fn at5b_build_request_round_trips_tool_blocks() {
        let assistant_turn = Message {
            role: Role::Assistant,
            content: vec![
                MessageContent::Text("calling search...".to_owned()),
                MessageContent::ToolUse {
                    id: "call_1".to_owned(),
                    name: "web_search".to_owned(),
                    input: json!({ "q": "rust async" }),
                },
            ],
        };
        let user_turn = Message {
            role: Role::User,
            content: vec![MessageContent::ToolResult {
                tool_use_id: "call_1".to_owned(),
                content: "[result text]".to_owned(),
                is_error: false,
            }],
        };
        let messages = [user_text("hi"), assistant_turn, user_turn];
        let params = base_params();
        let req = build_request(&messages, &[], &params, "claude-opus-4-7").expect("ok");
        // Serialize to JSON and assert on the wire-level shape so we catch
        // accidental tag/field renames.
        let json = serde_json::to_value(&req).expect("serialize");
        let msgs = json["messages"].as_array().expect("messages array");
        assert_eq!(msgs.len(), 3);

        // Assistant turn: text + tool_use blocks.
        let assistant_blocks = msgs[1]["content"].as_array().expect("array");
        assert_eq!(assistant_blocks.len(), 2);
        assert_eq!(assistant_blocks[0]["type"], "text");
        assert_eq!(assistant_blocks[0]["text"], "calling search...");
        assert_eq!(assistant_blocks[1]["type"], "tool_use");
        assert_eq!(assistant_blocks[1]["id"], "call_1");
        assert_eq!(assistant_blocks[1]["name"], "web_search");
        assert_eq!(assistant_blocks[1]["input"], json!({ "q": "rust async" }));

        // User turn: tool_result block with snake_case `tool_use_id`.
        let user_blocks = msgs[2]["content"].as_array().expect("array");
        assert_eq!(user_blocks.len(), 1);
        assert_eq!(user_blocks[0]["type"], "tool_result");
        assert_eq!(user_blocks[0]["tool_use_id"], "call_1");
        assert_eq!(user_blocks[0]["content"], "[result text]");
        assert_eq!(user_blocks[0]["is_error"], false);
    }

    // ── AT6: parse_response happy path ───────────────────────────────────────

    #[test]
    fn at6_parse_response_text_end_turn() {
        let body = MessagesResponse {
            content: vec![MessagesResponseContent::Text {
                text: "Hello!".to_owned(),
            }],
            stop_reason: "end_turn".to_owned(),
            usage: MessagesResponseUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        };
        let (content, tool_calls, finish_reason, tokens) = parse_response(body);

        assert_eq!(content.len(), 1);
        assert!(matches!(&content[0], MessageContent::Text(t) if t == "Hello!"));
        assert!(tool_calls.is_empty());
        assert_eq!(finish_reason, FinishReason::EndTurn);
        assert_eq!(tokens.input, 10);
        assert_eq!(tokens.output, 5);
        assert_eq!(tokens.cached, 0);
    }

    // ── AT7: parse_response with tool_use ────────────────────────────────────

    #[test]
    fn at7_parse_response_tool_use() {
        let body = MessagesResponse {
            content: vec![MessagesResponseContent::ToolUse {
                id: "call_xyz".to_owned(),
                name: "web_search".to_owned(),
                input: json!({ "q": "rust async" }),
            }],
            stop_reason: "tool_use".to_owned(),
            usage: MessagesResponseUsage {
                input_tokens: 20,
                output_tokens: 8,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        };
        let (content, tool_calls, finish_reason, tokens) = parse_response(body);

        assert!(content.is_empty());
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_xyz");
        assert_eq!(tool_calls[0].name, "web_search");
        assert_eq!(tool_calls[0].arguments, json!({ "q": "rust async" }));
        assert_eq!(finish_reason, FinishReason::ToolUse);
        assert_eq!(tokens.input, 20);
        assert_eq!(tokens.output, 8);
    }

    // ── AT8: parse_response unknown stop_reason ───────────────────────────────

    #[test]
    fn at8_parse_response_unknown_stop_reason() {
        let body = MessagesResponse {
            content: vec![MessagesResponseContent::Text {
                text: "ok".to_owned(),
            }],
            stop_reason: "unknown_future_reason".to_owned(),
            usage: MessagesResponseUsage {
                input_tokens: 1,
                output_tokens: 1,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        };
        let (_, _, finish_reason, _) = parse_response(body);
        assert_eq!(finish_reason, FinishReason::Other);
    }

    // ── stop_sequence ────────────────────────────────────────────────────────

    #[test]
    fn stop_sequence_maps_correctly() {
        let body = MessagesResponse {
            content: vec![],
            stop_reason: "stop_sequence".to_owned(),
            usage: MessagesResponseUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        };
        let (_, _, finish_reason, _) = parse_response(body);
        assert_eq!(finish_reason, FinishReason::StopSequence);
    }

    // ── max_tokens ───────────────────────────────────────────────────────────

    #[test]
    fn max_tokens_maps_correctly() {
        let body = MessagesResponse {
            content: vec![],
            stop_reason: "max_tokens".to_owned(),
            usage: MessagesResponseUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            },
        };
        let (_, _, finish_reason, _) = parse_response(body);
        assert_eq!(finish_reason, FinishReason::MaxTokens);
    }

    // ── AT2d: temperature is forwarded ───────────────────────────────────────

    #[test]
    fn at2d_temperature_is_forwarded() {
        let messages = [user_text("hi")];
        let params = Params {
            temperature: Some(Temperature::new(0.5).unwrap()),
            ..base_params()
        };
        let req =
            build_request(&messages, &[], &params, "claude-opus-4-7").expect("should succeed");
        let t = req.temperature.expect("temperature should be set");
        assert!((t - 0.5_f32).abs() < f32::EPSILON);
    }

    // ── AT6b: assistant role is forwarded ────────────────────────────────────

    #[test]
    fn at6b_assistant_role_is_forwarded() {
        let messages = [user_text("hello"), assistant_text("hi"), user_text("how?")];
        let params = base_params();
        let req =
            build_request(&messages, &[], &params, "claude-opus-4-7").expect("should succeed");
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[0].role, "user");
        assert_eq!(req.messages[1].role, "assistant");
        assert_eq!(req.messages[2].role, "user");
    }

    // ── AT6c: cached tokens from cache_read_input_tokens ────────────────────

    #[test]
    fn at6c_cache_read_tokens_are_forwarded() {
        let body = MessagesResponse {
            content: vec![MessagesResponseContent::Text {
                text: "ok".to_owned(),
            }],
            stop_reason: "end_turn".to_owned(),
            usage: MessagesResponseUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: Some(30),
                cache_read_input_tokens: Some(70),
            },
        };
        let (_, _, _, tokens) = parse_response(body);
        assert_eq!(tokens.input, 100);
        assert_eq!(tokens.output, 50);
        assert_eq!(tokens.cached, 70);
    }

    // ── AT15: empty messages forwarded as-is ─────────────────────────────────

    /// AT15: `build_request` with an empty message slice returns `Ok` with an
    /// empty wire messages array. Anthropic will reject this with HTTP 400, but
    /// the adapter does not pre-validate — that is caller responsibility (see
    /// doc on `build_request`).
    #[test]
    fn at15_empty_messages_forwarded() {
        let params = base_params();
        let result = build_request(&[], &[], &params, "claude-opus-4-7");
        assert!(
            result.is_ok(),
            "expected Ok for empty messages, got {result:?}"
        );
        let wire = result.unwrap();
        assert!(wire.messages.is_empty(), "wire messages should be empty");
    }

    // ── AT3b: system_prompt + multiple system messages ────────────────────────

    /// `AT3b`: when `params.system_prompt` is `Some` AND messages contain two
    /// system-role entries, `build_request` returns
    /// `Err(MultipleSystemMessages)`. The params override does not suppress
    /// system-message validation — we validate the slice unconditionally and
    /// then select which system text wins.
    #[test]
    fn at3b_params_system_with_multiple_system_messages_is_error() {
        let messages = [system_text("first"), user_text("hi"), system_text("second")];
        let params = Params {
            system_prompt: Some("From params.".to_owned()),
            ..base_params()
        };
        let result = build_request(&messages, &[], &params, "claude-opus-4-7");
        assert!(
            matches!(result, Err(TranslationError::MultipleSystemMessages)),
            "expected MultipleSystemMessages even with params.system_prompt set, got {result:?}"
        );
    }
}
