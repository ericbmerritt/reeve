//! OpenAI-compatible chat completions wire types and request/response
//! translation.
//!
//! Used by adapters that route through `OpenRouter` or any other
//! OpenAI-compatible endpoint. All types here are `pub(crate)`.

use crate::{AdapterError, FinishReason, MessageContent, Role, TokenCounts, Tool, ToolCall};

// ── Wire request types ─────────────────────────────────────────────────────────

/// Request body for `POST /v1/chat/completions`.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ChatRequest<'a> {
    pub(crate) model: &'a str,
    pub(crate) max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f32>,
    pub(crate) messages: Vec<ChatRequestMessage<'a>>,
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    pub(crate) tools: Vec<ChatRequestTool<'a>>,
}

/// One message in the request.
///
/// The OpenAI-compatible wire format uses a flat object with optional fields
/// depending on role. We use a single struct with `skip_serializing_if` so we
/// never emit `null` for fields the provider ignores.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ChatRequestMessage<'a> {
    pub(crate) role: &'a str,
    /// Present for system, user, and assistant messages with text content.
    /// `None` for assistant messages that carry only tool calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) content: Option<&'a str>,
    /// Present for assistant messages with tool calls.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) tool_calls: Vec<ChatRequestToolCall<'a>>,
    /// Present for tool-result messages (role `"tool"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<&'a str>,
}

/// A tool call embedded in an assistant request message.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ChatRequestToolCall<'a> {
    pub(crate) id: &'a str,
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) function: ChatRequestFunctionCall<'a>,
}

/// The function invocation within a [`ChatRequestToolCall`].
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ChatRequestFunctionCall<'a> {
    pub(crate) name: &'a str,
    /// JSON-serialized arguments string (OpenAI-compatible wire convention).
    pub(crate) arguments: String,
}

/// A tool definition forwarded to the model.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ChatRequestTool<'a> {
    #[serde(rename = "type")]
    pub(crate) kind: &'static str,
    pub(crate) function: ChatRequestFunction<'a>,
}

/// The function definition within a [`ChatRequestTool`].
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ChatRequestFunction<'a> {
    pub(crate) name: &'a str,
    pub(crate) description: &'a str,
    pub(crate) parameters: &'a serde_json::Value,
}

// ── Wire response types ────────────────────────────────────────────────────────

/// Response body from `POST /v1/chat/completions`.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ChatResponse {
    pub(crate) choices: Vec<ChatChoice>,
    pub(crate) usage: ChatUsage,
}

/// One completion choice.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ChatChoice {
    pub(crate) message: ChatResponseMessage,
    pub(crate) finish_reason: String,
}

/// The assistant message in a response choice.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ChatResponseMessage {
    #[serde(default)]
    pub(crate) content: Option<String>,
    #[serde(default)]
    pub(crate) tool_calls: Vec<ChatResponseToolCall>,
}

/// A tool call in a response message.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ChatResponseToolCall {
    pub(crate) id: String,
    pub(crate) function: ChatResponseFunctionCall,
}

/// The function invocation within a [`ChatResponseToolCall`].
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ChatResponseFunctionCall {
    pub(crate) name: String,
    /// JSON-serialized arguments string (OpenAI-compatible wire convention).
    pub(crate) arguments: String,
}

/// Token usage in the response.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ChatUsage {
    pub(crate) prompt_tokens: u32,
    pub(crate) completion_tokens: u32,
}

/// Error response body.
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ChatErrorResponse {
    pub(crate) error: ChatErrorDetail,
}

/// Detail within a [`ChatErrorResponse`].
#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct ChatErrorDetail {
    #[serde(rename = "type")]
    pub(crate) error_type: Option<String>,
    #[expect(dead_code, reason = "deserialized from wire; we emit only error_type")]
    pub(crate) message: String,
}

// ── Request translation ────────────────────────────────────────────────────────

/// Translate Reeve's message slice into the OpenAI-compatible wire format.
///
/// Role mapping:
/// - `Role::System` → `"system"` message with text content
/// - `Role::User` with only `ToolResult` blocks → one `"tool"` message per
///   result
/// - `Role::User` with `Text` blocks → one `"user"` message; any `ToolResult`
///   blocks in the same turn are emitted as `"tool"` messages after it
/// - `Role::Assistant` → `"assistant"` message; text content and tool calls
///   are combined in one message
#[expect(
    clippy::too_many_lines,
    reason = "linear match-heavy translation; splitting on line count would fragment \
              the role-dispatch sequence"
)]
pub(crate) fn build_request<'a>(
    messages: &'a [crate::Message],
    tools: &'a [Tool],
    params: &'a crate::Params,
    model: &'a str,
) -> ChatRequest<'a> {
    let mut wire_messages: Vec<ChatRequestMessage<'_>> = Vec::new();

    for msg in messages {
        match msg.role {
            Role::System => {
                let text = collect_text_content(&msg.content);
                wire_messages.push(ChatRequestMessage {
                    role: "system",
                    content: Some(text),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                });
            }
            Role::Assistant => {
                let text = collect_text_content(&msg.content);
                let tool_calls: Vec<ChatRequestToolCall<'_>> = msg
                    .content
                    .iter()
                    .filter_map(|block| {
                        if let MessageContent::ToolUse { id, name, input } = block {
                            let arguments = serde_json::to_string(input).unwrap_or_default();
                            Some(ChatRequestToolCall {
                                id: id.as_str(),
                                kind: "function",
                                function: ChatRequestFunctionCall {
                                    name: name.as_str(),
                                    arguments,
                                },
                            })
                        } else {
                            None
                        }
                    })
                    .collect();
                let content = if text.is_empty() && !tool_calls.is_empty() {
                    None
                } else {
                    Some(text)
                };
                wire_messages.push(ChatRequestMessage {
                    role: "assistant",
                    content,
                    tool_calls,
                    tool_call_id: None,
                });
            }
            Role::User => {
                // Text blocks become a user message; ToolResult blocks become
                // individual tool messages. The system_prompt shortcut path
                // (Params.system_prompt) is handled below.
                let has_text = msg
                    .content
                    .iter()
                    .any(|b| matches!(b, MessageContent::Text(_)));
                if has_text {
                    let text = collect_text_content(&msg.content);
                    wire_messages.push(ChatRequestMessage {
                        role: "user",
                        content: Some(text),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                    });
                }
                for block in &msg.content {
                    if let MessageContent::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = block
                    {
                        wire_messages.push(ChatRequestMessage {
                            role: "tool",
                            content: Some(content.as_str()),
                            tool_calls: Vec::new(),
                            tool_call_id: Some(tool_use_id.as_str()),
                        });
                    }
                }
            }
        }
    }

    // Prepend a system message from Params if present and no system-role
    // message was already in the slice.
    if let Some(sys) = &params.system_prompt {
        let already_has_system = messages.iter().any(|m| m.role == Role::System);
        if !already_has_system {
            wire_messages.insert(
                0,
                ChatRequestMessage {
                    role: "system",
                    content: Some(sys.as_str()),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                },
            );
        }
    }

    let wire_tools: Vec<ChatRequestTool<'_>> = tools
        .iter()
        .map(|t| ChatRequestTool {
            kind: "function",
            function: ChatRequestFunction {
                name: t.name.as_str(),
                description: t.description.as_str(),
                parameters: &t.input_schema,
            },
        })
        .collect();

    ChatRequest {
        model,
        max_tokens: params.max_tokens,
        temperature: params.temperature.map(crate::Temperature::value),
        messages: wire_messages,
        tools: wire_tools,
    }
}

/// Return the text of the first `Text` block in `content`, or `""` if none.
///
/// Reeve's protocol produces turns with a single text block; this is the fast
/// path. Multi-block text turns are not possible in the current protocol, so
/// only the first block is returned.
fn collect_text_content(content: &[MessageContent]) -> &str {
    for block in content {
        if let MessageContent::Text(t) = block {
            return t.as_str();
        }
    }
    ""
}

// ── Response translation ───────────────────────────────────────────────────────

/// Parse an OpenAI-compatible response into Reeve's internal shapes.
///
/// Returns `Err(AdapterError::Decode)` if:
/// - `choices` is empty
/// - a tool call's `arguments` field is not valid JSON
pub(crate) fn parse_response(
    body: ChatResponse,
) -> Result<
    (
        Vec<MessageContent>,
        Vec<ToolCall>,
        FinishReason,
        TokenCounts,
    ),
    AdapterError,
> {
    let choice = body
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| AdapterError::Decode {
            source: "response contained no choices".into(),
        })?;

    let msg = choice.message;
    let mut content: Vec<MessageContent> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    if let Some(text) = msg.content {
        if !text.is_empty() {
            content.push(MessageContent::Text(text));
        }
    }

    for tc in msg.tool_calls {
        let arguments: serde_json::Value =
            serde_json::from_str(&tc.function.arguments).map_err(|e| AdapterError::Decode {
                source: format!(
                    "tool call '{}' arguments is not valid JSON: {e}",
                    tc.function.name
                )
                .into(),
            })?;
        tool_calls.push(ToolCall {
            id: tc.id,
            name: tc.function.name,
            arguments,
        });
    }

    let finish_reason = map_finish_reason(&choice.finish_reason);
    let tokens = TokenCounts {
        input: body.usage.prompt_tokens,
        output: body.usage.completion_tokens,
        cached: 0,
    };

    Ok((content, tool_calls, finish_reason, tokens))
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::EndTurn,
        "length" => FinishReason::MaxTokens,
        "tool_calls" => FinishReason::ToolUse,
        _ => FinishReason::Other,
    }
}

/// Extract the `type` field from an error body, falling back to the
/// `message` field, then to `"unknown_error"`.
pub(crate) async fn extract_error_type(response: reqwest::Response) -> String {
    match response.json::<ChatErrorResponse>().await {
        Ok(body) => body
            .error
            .error_type
            .unwrap_or_else(|| "unknown_error".to_owned()),
        Err(_) => "unknown_error".to_owned(),
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, MessageContent, Params, Role, Tool};
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

    fn base_params() -> Params {
        Params {
            max_tokens: 1024,
            temperature: None,
            system_prompt: None,
        }
    }

    // OA1: system-role message appears as first wire message with role "system"
    #[test]
    fn oa1_system_role_maps_to_system_wire_message() {
        let messages = [system_text("You are helpful."), user_text("hi")];
        let params = base_params();
        let req = build_request(&messages, &[], &params, "model");
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[0].content, Some("You are helpful."));
        assert_eq!(req.messages[1].role, "user");
    }

    // OA2: Params.system_prompt is prepended when no system-role message exists
    #[test]
    fn oa2_params_system_prompt_prepended() {
        let params = Params {
            system_prompt: Some("Be concise.".to_owned()),
            ..base_params()
        };
        let messages = [user_text("hello")];
        let req = build_request(&messages, &[], &params, "model");
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[0].content, Some("Be concise."));
        assert_eq!(req.messages[1].role, "user");
    }

    // OA3: assistant tool-use blocks become tool_calls
    #[test]
    fn oa3_assistant_tool_use_becomes_tool_calls() {
        let messages = [Message {
            role: Role::Assistant,
            content: vec![MessageContent::ToolUse {
                id: "call_1".to_owned(),
                name: "search".to_owned(),
                input: json!({ "query": "hello" }),
            }],
        }];
        let params = base_params();
        let req = build_request(&messages, &[], &params, "model");
        let wire = &req.messages[0];
        assert_eq!(wire.role, "assistant");
        assert!(wire.content.is_none());
        assert_eq!(wire.tool_calls.len(), 1);
        assert_eq!(wire.tool_calls[0].id, "call_1");
        assert_eq!(wire.tool_calls[0].function.name, "search");
        let args: serde_json::Value =
            serde_json::from_str(&wire.tool_calls[0].function.arguments).unwrap();
        assert_eq!(args["query"], "hello");
    }

    // OA4: user ToolResult blocks become "tool" role messages
    #[test]
    fn oa4_user_tool_result_becomes_tool_messages() {
        let messages = [Message {
            role: Role::User,
            content: vec![MessageContent::ToolResult {
                tool_use_id: "call_1".to_owned(),
                content: "42".to_owned(),
                is_error: false,
            }],
        }];
        let params = base_params();
        let req = build_request(&messages, &[], &params, "model");
        assert_eq!(req.messages[0].role, "tool");
        assert_eq!(req.messages[0].tool_call_id, Some("call_1"));
        assert_eq!(req.messages[0].content, Some("42"));
    }

    // OA5: tools are translated to function-type tool definitions
    #[test]
    fn oa5_tools_become_function_type_definitions() {
        let messages = [user_text("hi")];
        let tools = [Tool {
            name: "search".to_owned(),
            description: "Search the web".to_owned(),
            input_schema: json!({ "type": "object" }),
        }];
        let params = base_params();
        let req = build_request(&messages, &tools, &params, "model");
        assert_eq!(req.tools.len(), 1);
        assert_eq!(req.tools[0].kind, "function");
        assert_eq!(req.tools[0].function.name, "search");
    }

    // OA6: parse_response extracts text content and finish reason
    #[test]
    fn oa6_parse_response_text_and_finish_reason() {
        let body = ChatResponse {
            choices: vec![ChatChoice {
                message: ChatResponseMessage {
                    content: Some("Hello!".to_owned()),
                    tool_calls: vec![],
                },
                finish_reason: "stop".to_owned(),
            }],
            usage: ChatUsage {
                prompt_tokens: 10,
                completion_tokens: 5,
            },
        };
        let (content, tool_calls, finish, tokens) = parse_response(body).unwrap();
        assert_eq!(content, vec![MessageContent::Text("Hello!".to_owned())]);
        assert!(tool_calls.is_empty());
        assert_eq!(finish, FinishReason::EndTurn);
        assert_eq!(tokens.input, 10);
        assert_eq!(tokens.output, 5);
    }

    // OA7: parse_response extracts tool calls with parsed arguments
    #[test]
    fn oa7_parse_response_tool_calls_parsed() {
        let body = ChatResponse {
            choices: vec![ChatChoice {
                message: ChatResponseMessage {
                    content: None,
                    tool_calls: vec![ChatResponseToolCall {
                        id: "call_99".to_owned(),
                        function: ChatResponseFunctionCall {
                            name: "search".to_owned(),
                            arguments: r#"{"query":"rust"}"#.to_owned(),
                        },
                    }],
                },
                finish_reason: "tool_calls".to_owned(),
            }],
            usage: ChatUsage {
                prompt_tokens: 5,
                completion_tokens: 3,
            },
        };
        let (content, tool_calls, finish, _) = parse_response(body).unwrap();
        assert!(content.is_empty());
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_99");
        assert_eq!(tool_calls[0].name, "search");
        assert_eq!(tool_calls[0].arguments["query"], "rust");
        assert_eq!(finish, FinishReason::ToolUse);
    }

    // OA8: parse_response returns Decode error for malformed tool arguments
    #[test]
    fn oa8_parse_response_bad_arguments_is_decode_error() {
        let body = ChatResponse {
            choices: vec![ChatChoice {
                message: ChatResponseMessage {
                    content: None,
                    tool_calls: vec![ChatResponseToolCall {
                        id: "call_bad".to_owned(),
                        function: ChatResponseFunctionCall {
                            name: "tool".to_owned(),
                            arguments: "not json".to_owned(),
                        },
                    }],
                },
                finish_reason: "tool_calls".to_owned(),
            }],
            usage: ChatUsage {
                prompt_tokens: 1,
                completion_tokens: 1,
            },
        };
        let err = parse_response(body).unwrap_err();
        assert!(matches!(err, AdapterError::Decode { .. }), "got: {err}");
    }

    // OA9: parse_response returns Decode error when choices is empty
    #[test]
    fn oa9_parse_response_empty_choices_is_decode_error() {
        let body = ChatResponse {
            choices: vec![],
            usage: ChatUsage {
                prompt_tokens: 0,
                completion_tokens: 0,
            },
        };
        let err = parse_response(body).unwrap_err();
        assert!(matches!(err, AdapterError::Decode { .. }), "got: {err}");
    }

    // OA10: map_finish_reason covers all known values
    #[test]
    fn oa10_map_finish_reason_coverage() {
        assert_eq!(map_finish_reason("stop"), FinishReason::EndTurn);
        assert_eq!(map_finish_reason("length"), FinishReason::MaxTokens);
        assert_eq!(map_finish_reason("tool_calls"), FinishReason::ToolUse);
        assert_eq!(map_finish_reason("unknown"), FinishReason::Other);
    }
}
