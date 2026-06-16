//! `openai-chat` protocol (Chat Completions), ported from `packages/llm/src/protocols/openai-chat.ts`.
//!
//! Same `Protocol` shape as anthropic-messages, but the wire format differs in ways that exercise the
//! abstraction's generality: system is a `role:"system"` *message* (not top-level), message content is
//! a plain string, tools are wrapped in `{type:"function", function:{…, parameters}}`, and tool-call
//! arguments stream as `delta.tool_calls[].function.arguments` correlated by `index`. Crucially OpenAI
//! splits `finish_reason` and `usage` across the **last two chunks** and has no explicit terminal
//! event (the stream ends with `[DONE]`), so the terminal `Finish` (and the text/tool block ends) are
//! flushed in [`Protocol::on_halt`].

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ContentPart, FinishReason, Generation, LlmError, LlmEvent, LlmRequest, Message, Protocol, Role,
    ToolChoice, Usage,
};

/// The `openai-chat` protocol.
pub struct OpenAiChat;

const DEFAULT_MAX_TOKENS: u64 = 4096;
const TEXT_BLOCK_ID: &str = "block_0";

// ---- Request body (the `body.from` lowering target) ----

/// The OpenAI Chat Completions request body (core subset).
#[derive(Debug, Serialize, PartialEq)]
pub struct OpenAiBody {
    model: String,
    messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAiTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<OpenAiToolChoice>,
    stream: bool,
    stream_options: StreamOptions,
    max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize, PartialEq)]
struct OpenAiMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct OpenAiToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiFunctionCall,
}

#[derive(Debug, Serialize, PartialEq)]
struct OpenAiFunctionCall {
    name: String,
    /// OpenAI carries tool-call arguments as a JSON *string*.
    arguments: String,
}

#[derive(Debug, Serialize, PartialEq)]
struct OpenAiTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiToolFunction,
}

#[derive(Debug, Serialize, PartialEq)]
struct OpenAiToolFunction {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: Value,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(untagged)]
enum OpenAiToolChoice {
    /// `"auto"` / `"none"` / `"required"`.
    Mode(&'static str),
    /// `{ "type": "function", "function": { "name": … } }`.
    Function {
        #[serde(rename = "type")]
        kind: &'static str,
        function: NamedFunction,
    },
}

#[derive(Debug, Serialize, PartialEq)]
struct NamedFunction {
    name: String,
}

fn lower_tool_choice(choice: &ToolChoice) -> Option<OpenAiToolChoice> {
    match choice {
        ToolChoice::Auto => Some(OpenAiToolChoice::Mode("auto")),
        ToolChoice::None => Some(OpenAiToolChoice::Mode("none")),
        ToolChoice::Required => Some(OpenAiToolChoice::Mode("required")),
        ToolChoice::Tool(name) => Some(OpenAiToolChoice::Function {
            kind: "function",
            function: NamedFunction { name: name.clone() },
        }),
    }
}

fn content_to_string(result: &Value) -> String {
    match result {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn lower_message(message: &Message) -> OpenAiMessage {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    // Tool results are their own role with a tool_call_id; otherwise gather text + tool calls.
    if message.role == Role::Tool {
        let (id, content) = message
            .content
            .iter()
            .find_map(|p| match p {
                ContentPart::ToolResult { id, result, .. } => {
                    Some((id.clone(), content_to_string(result)))
                }
                _ => None,
            })
            .unwrap_or_default();
        return OpenAiMessage {
            role,
            content: Some(content),
            tool_calls: None,
            tool_call_id: Some(id),
        };
    }
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text(t) => text.push_str(t),
            ContentPart::ToolCall { id, name, input } => tool_calls.push(OpenAiToolCall {
                id: id.clone(),
                kind: "function",
                function: OpenAiFunctionCall {
                    name: name.clone(),
                    arguments: input.to_string(),
                },
            }),
            ContentPart::ToolResult { .. } => {}
        }
    }
    OpenAiMessage {
        role,
        content: if text.is_empty() && !tool_calls.is_empty() {
            None
        } else {
            Some(text)
        },
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        tool_call_id: None,
    }
}

fn lower_body(request: &LlmRequest) -> OpenAiBody {
    let Generation {
        max_tokens,
        temperature,
        top_p,
        stop,
        ..
    } = request.generation.clone();
    // System parts become a single leading system message.
    let mut messages = Vec::new();
    if !request.system.is_empty() {
        messages.push(OpenAiMessage {
            role: "system",
            content: Some(request.system.join("\n")),
            tool_calls: None,
            tool_call_id: None,
        });
    }
    messages.extend(request.messages.iter().map(lower_message));
    OpenAiBody {
        model: request.model.clone(),
        messages,
        tools: request
            .tools
            .iter()
            .map(|t| OpenAiTool {
                kind: "function",
                function: OpenAiToolFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.input_schema.clone(),
                },
            })
            .collect(),
        tool_choice: request.tool_choice.as_ref().and_then(lower_tool_choice),
        stream: true,
        stream_options: StreamOptions {
            include_usage: true,
        },
        max_tokens: max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        temperature,
        top_p,
        stop,
    }
}

// ---- Streaming decode ----

/// One `chat.completion.chunk` (only the fields the decoder reads).
#[derive(Debug, Deserialize)]
pub struct OpenAiChunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallDelta>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Default)]
struct ToolAcc {
    id: String,
    name: String,
    arguments: String,
    started: bool,
}

/// Accumulator for an OpenAI chat stream.
#[derive(Default)]
pub struct OpenAiState {
    text_started: bool,
    tools: HashMap<u32, ToolAcc>,
    tool_order: Vec<u32>,
    finish_reason: Option<FinishReason>,
    usage: Usage,
    usage_seen: bool,
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

fn parse_arguments(buffer: &str) -> Value {
    let trimmed = buffer.trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| serde_json::json!({}))
}

impl Protocol for OpenAiChat {
    type Body = OpenAiBody;
    type Event = OpenAiChunk;
    type State = OpenAiState;

    fn name(&self) -> &'static str {
        "openai-chat"
    }

    fn build_body(&self, request: &LlmRequest) -> Result<OpenAiBody, LlmError> {
        Ok(lower_body(request))
    }

    fn initial(&self) -> OpenAiState {
        OpenAiState::default()
    }

    fn decode_frame(&self, frame: &str) -> Result<OpenAiChunk, LlmError> {
        serde_json::from_str(frame.trim()).map_err(|e| LlmError::Decode(e.to_string()))
    }

    fn step(&self, state: &mut OpenAiState, chunk: OpenAiChunk) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        if let Some(usage) = chunk.usage {
            state.usage = Usage {
                input: usage.prompt_tokens,
                output: usage.completion_tokens,
                cache_read: usage
                    .prompt_tokens_details
                    .map(|d| d.cached_tokens)
                    .unwrap_or(0),
                cache_write: 0,
            };
            state.usage_seen = true;
        }
        for choice in chunk.choices {
            if let Some(content) = choice.delta.content {
                if !state.text_started {
                    state.text_started = true;
                    out.push(LlmEvent::TextStart {
                        id: TEXT_BLOCK_ID.to_string(),
                    });
                }
                if !content.is_empty() {
                    out.push(LlmEvent::TextDelta {
                        id: TEXT_BLOCK_ID.to_string(),
                        text: content,
                    });
                }
            }
            for tc in choice.delta.tool_calls {
                let acc = state.tools.entry(tc.index).or_default();
                if acc.id.is_empty() && !state.tool_order.contains(&tc.index) {
                    state.tool_order.push(tc.index);
                }
                if let Some(id) = tc.id {
                    acc.id = id;
                }
                if let Some(function) = tc.function {
                    if let Some(name) = function.name {
                        acc.name = name;
                    }
                    if !acc.started && !acc.id.is_empty() && !acc.name.is_empty() {
                        acc.started = true;
                        out.push(LlmEvent::ToolInputStart {
                            id: acc.id.clone(),
                            name: acc.name.clone(),
                        });
                    }
                    if let Some(arguments) = function.arguments {
                        if !arguments.is_empty() {
                            acc.arguments.push_str(&arguments);
                            out.push(LlmEvent::ToolInputDelta {
                                id: acc.id.clone(),
                                text: arguments,
                            });
                        }
                    }
                }
            }
            if let Some(reason) = choice.finish_reason {
                state.finish_reason = Some(map_finish_reason(&reason));
            }
        }
        out
    }

    fn terminal(&self, _chunk: &OpenAiChunk) -> bool {
        // No explicit terminal event; the stream ends with `[DONE]` (dropped by SSE framing). The
        // terminal Finish is flushed in `on_halt`.
        false
    }

    fn on_halt(&self, state: &OpenAiState) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        if state.text_started {
            out.push(LlmEvent::TextEnd {
                id: TEXT_BLOCK_ID.to_string(),
            });
        }
        for index in &state.tool_order {
            if let Some(acc) = state.tools.get(index) {
                out.push(LlmEvent::ToolInputEnd { id: acc.id.clone() });
                out.push(LlmEvent::ToolCall {
                    id: acc.id.clone(),
                    name: acc.name.clone(),
                    input: parse_arguments(&acc.arguments),
                });
            }
        }
        let reason = state.finish_reason.unwrap_or(FinishReason::Unknown);
        let usage = state.usage_seen.then_some(state.usage);
        out.push(LlmEvent::StepFinish { reason, usage });
        out.push(LlmEvent::Finish { reason, usage });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_sse, Generation, LlmRequest, Message, ToolChoice, ToolDefinition};
    use serde_json::json;

    // Recorded cassettes from `packages/llm/test/fixtures/recordings/openai-chat/`.
    const STREAMS_TEXT: &str = r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":"","refusal":null},"finish_reason":null}],"usage":null}

data: {"choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}],"usage":null}

data: {"choices":[{"index":0,"delta":{"content":"!"},"finish_reason":null}],"usage":null}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":null}

data: {"choices":[],"usage":{"prompt_tokens":22,"completion_tokens":2,"total_tokens":24,"prompt_tokens_details":{"cached_tokens":0}}}

data: [DONE]
"#;

    const STREAMS_TOOL_CALL: &str = r#"data: {"choices":[{"index":0,"delta":{"role":"assistant","content":null,"tool_calls":[{"index":0,"id":"call_5wBV98AvGPwOyC6a2HtKh85w","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}],"usage":null}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\""}}]},"finish_reason":null}],"usage":null}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"city"}}]},"finish_reason":null}],"usage":null}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\":\""}}]},"finish_reason":null}],"usage":null}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"Paris"}}]},"finish_reason":null}],"usage":null}

data: {"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"}"}}]},"finish_reason":null}],"usage":null}

data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":null}

data: {"choices":[],"usage":{"prompt_tokens":67,"completion_tokens":5,"total_tokens":72,"prompt_tokens_details":{"cached_tokens":0}}}

data: [DONE]
"#;

    #[test]
    fn decodes_text_stream() {
        let events = decode_sse(&OpenAiChat, STREAMS_TEXT).unwrap();
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello!");
        assert!(matches!(events.first(), Some(LlmEvent::TextStart { .. })));
        assert!(events.iter().any(|e| matches!(e, LlmEvent::TextEnd { .. })));
        match events.last() {
            Some(LlmEvent::Finish { reason, usage }) => {
                assert_eq!(*reason, FinishReason::Stop);
                let usage = usage.unwrap();
                assert_eq!(usage.input, 22);
                assert_eq!(usage.output, 2);
            }
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    #[test]
    fn decodes_tool_call_stream() {
        let events = decode_sse(&OpenAiChat, STREAMS_TOOL_CALL).unwrap();
        let call = events
            .iter()
            .find_map(|e| match e {
                LlmEvent::ToolCall { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .expect("a tool-call event");
        assert_eq!(call.0, "call_5wBV98AvGPwOyC6a2HtKh85w");
        assert_eq!(call.1, "get_weather");
        assert_eq!(call.2, json!({ "city": "Paris" }));
        assert!(events
            .iter()
            .any(|e| matches!(e, LlmEvent::ToolInputStart { .. })));
        match events.last() {
            Some(LlmEvent::Finish { usage, .. }) => {
                let usage = usage.unwrap();
                assert_eq!(usage.input, 67);
                assert_eq!(usage.output, 5);
            }
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    fn body_value(request: &LlmRequest) -> serde_json::Value {
        serde_json::to_value(OpenAiChat.build_body(request).unwrap()).unwrap()
    }

    #[test]
    fn lowers_text_request_to_cassette_body() {
        let request = LlmRequest {
            model: "gpt-4o-mini".into(),
            system: vec!["You are concise.".into()],
            messages: vec![Message::user_text("Say hello in one short sentence.")],
            generation: Generation {
                max_tokens: Some(20),
                temperature: Some(0.0),
                ..Default::default()
            },
            ..Default::default()
        };
        let body = body_value(&request);
        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "You are concise."},
                {"role": "user", "content": "Say hello in one short sentence."}
            ])
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
        assert_eq!(body["max_tokens"], 20);
        assert_eq!(body["temperature"].as_f64(), Some(0.0));
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn lowers_tool_request_to_cassette_body() {
        let schema = json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
            "additionalProperties": false
        });
        let request = LlmRequest {
            model: "gpt-4o-mini".into(),
            system: vec!["Call tools exactly as requested.".into()],
            messages: vec![Message::user_text(
                "Call get_weather with city exactly Paris.",
            )],
            tools: vec![ToolDefinition {
                name: "get_weather".into(),
                description: Some("Get current weather for a city.".into()),
                input_schema: schema.clone(),
            }],
            tool_choice: Some(ToolChoice::Tool("get_weather".into())),
            generation: Generation {
                max_tokens: Some(80),
                temperature: Some(0.0),
                ..Default::default()
            },
        };
        let body = body_value(&request);
        assert_eq!(
            body["tools"],
            json!([{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get current weather for a city.",
                    "parameters": schema
                }
            }])
        );
        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "function": {"name": "get_weather"}})
        );
    }

    #[test]
    fn tool_choice_modes() {
        let with = |choice: Option<ToolChoice>| {
            body_value(&LlmRequest {
                model: "m".into(),
                tool_choice: choice,
                ..Default::default()
            })
        };
        assert_eq!(with(Some(ToolChoice::Auto))["tool_choice"], json!("auto"));
        assert_eq!(
            with(Some(ToolChoice::Required))["tool_choice"],
            json!("required")
        );
        assert_eq!(with(Some(ToolChoice::None))["tool_choice"], json!("none"));
        assert!(with(None).get("tool_choice").is_none());
    }

    #[test]
    fn lowers_assistant_tool_call_and_tool_result() {
        let request = LlmRequest {
            model: "m".into(),
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolCall {
                        id: "call_1".into(),
                        name: "get_weather".into(),
                        input: json!({ "city": "Paris" }),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        id: "call_1".into(),
                        name: "get_weather".into(),
                        result: json!("sunny"),
                    }],
                },
            ],
            ..Default::default()
        };
        let body = body_value(&request);
        assert_eq!(
            body["messages"][0],
            json!({
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                }]
            })
        );
        assert_eq!(
            body["messages"][1],
            json!({"role": "tool", "content": "sunny", "tool_call_id": "call_1"})
        );
    }
}
