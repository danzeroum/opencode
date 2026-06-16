//! `gemini` protocol (`generateContent` streaming), ported from `packages/llm/src/protocols/
//! gemini.ts`.
//!
//! Despite the common assumption that Gemini streams binary protobuf, its `?alt=sse` endpoint returns
//! **text SSE** (`data: {GenerateContentResponse}`), so it reuses [`crate::sse_frames`] like the other
//! text protocols. The shape differs: system is a top-level `systemInstruction`, messages are
//! `contents` with `parts`, tools are `functionDeclarations`, tool choice is a `functionCallingConfig`
//! mode, and a tool call arrives as a **complete** `functionCall` part (args as a JSON object, not
//! streamed) with **no id** (one is synthesized). `finishReason` + `usageMetadata` arrive together, so
//! the terminal `Finish` + the text-block end are flushed in [`Protocol::on_halt`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ContentPart, FinishReason, Generation, LlmError, LlmEvent, LlmRequest, Message, Protocol, Role,
    ToolChoice, Usage,
};

/// The `gemini` protocol.
pub struct Gemini;

const DEFAULT_MAX_TOKENS: u64 = 4096;
const TEXT_BLOCK_ID: &str = "block_0";

// ---- Request body (`body.from` lowering target) ----

/// The Gemini `generateContent` request body (core subset).
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GeminiBody {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<GeminiTools>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_config: Option<GeminiToolConfig>,
    generation_config: GeminiGenerationConfig,
}

#[derive(Debug, Serialize, PartialEq)]
struct GeminiContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<&'static str>,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(untagged)]
enum GeminiPart {
    Text {
        text: String,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCall,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GeminiFunctionResponse,
    },
}

#[derive(Debug, Serialize, PartialEq)]
struct GeminiFunctionCall {
    name: String,
    args: Value,
}

#[derive(Debug, Serialize, PartialEq)]
struct GeminiFunctionResponse {
    name: String,
    response: Value,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct GeminiTools {
    function_declarations: Vec<GeminiFunctionDecl>,
}

#[derive(Debug, Serialize, PartialEq)]
struct GeminiFunctionDecl {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    parameters: Value,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct GeminiToolConfig {
    function_calling_config: FunctionCallingConfig,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct FunctionCallingConfig {
    mode: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    allowed_function_names: Option<Vec<String>>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    max_output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
}

fn lower_tool_config(choice: &ToolChoice) -> GeminiToolConfig {
    let (mode, names) = match choice {
        ToolChoice::Auto => ("AUTO", None),
        ToolChoice::None => ("NONE", None),
        ToolChoice::Required => ("ANY", None),
        ToolChoice::Tool(name) => ("ANY", Some(vec![name.clone()])),
    };
    GeminiToolConfig {
        function_calling_config: FunctionCallingConfig {
            mode,
            allowed_function_names: names,
        },
    }
}

fn lower_message(message: &Message) -> GeminiContent {
    let role = match message.role {
        Role::Assistant => "model",
        // user / tool (functionResponse) / system-as-text all use the user role in `contents`.
        _ => "user",
    };
    let parts = message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text(text) => GeminiPart::Text { text: text.clone() },
            ContentPart::ToolCall { name, input, .. } => GeminiPart::FunctionCall {
                function_call: GeminiFunctionCall {
                    name: name.clone(),
                    args: input.clone(),
                },
            },
            ContentPart::ToolResult { name, result, .. } => GeminiPart::FunctionResponse {
                function_response: GeminiFunctionResponse {
                    name: name.clone(),
                    response: result.clone(),
                },
            },
        })
        .collect();
    GeminiContent {
        role: Some(role),
        parts,
    }
}

fn lower_body(request: &LlmRequest) -> GeminiBody {
    let Generation {
        max_tokens,
        temperature,
        top_p,
        top_k,
        stop,
    } = request.generation.clone();
    let system_instruction = (!request.system.is_empty()).then(|| GeminiContent {
        role: None,
        parts: request
            .system
            .iter()
            .map(|t| GeminiPart::Text { text: t.clone() })
            .collect(),
    });
    GeminiBody {
        contents: request.messages.iter().map(lower_message).collect(),
        system_instruction,
        tools: if request.tools.is_empty() {
            Vec::new()
        } else {
            vec![GeminiTools {
                function_declarations: request
                    .tools
                    .iter()
                    .map(|t| GeminiFunctionDecl {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        parameters: t.input_schema.clone(),
                    })
                    .collect(),
            }]
        },
        tool_config: request.tool_choice.as_ref().map(lower_tool_config),
        generation_config: GeminiGenerationConfig {
            max_output_tokens: max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            temperature,
            top_p,
            top_k,
            stop_sequences: stop,
        },
    }
}

// ---- Streaming decode ----

/// One streamed `GenerateContentResponse` (only the fields the decoder reads).
#[derive(Debug, Deserialize)]
pub struct GeminiChunk {
    #[serde(default)]
    candidates: Vec<Candidate>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<CandidateContent>,
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<ResponsePart>,
}

#[derive(Debug, Deserialize)]
struct ResponsePart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<RespFunctionCall>,
}

#[derive(Debug, Deserialize)]
struct RespFunctionCall {
    name: String,
    #[serde(default)]
    args: Value,
}

#[derive(Debug, Default, Deserialize)]
struct UsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt: u64,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates: u64,
    #[serde(default, rename = "thoughtsTokenCount")]
    thoughts: u64,
    #[serde(default, rename = "cachedContentTokenCount")]
    cached: u64,
}

/// Accumulator for a Gemini stream.
#[derive(Default)]
pub struct GeminiState {
    text_started: bool,
    tool_counter: u32,
    finish_reason: Option<FinishReason>,
    usage: Usage,
    usage_seen: bool,
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "STOP" => FinishReason::Stop,
        "MAX_TOKENS" => FinishReason::Length,
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

impl Protocol for Gemini {
    type Body = GeminiBody;
    type Event = GeminiChunk;
    type State = GeminiState;

    fn name(&self) -> &'static str {
        "gemini"
    }

    fn build_body(&self, request: &LlmRequest) -> Result<GeminiBody, LlmError> {
        Ok(lower_body(request))
    }

    fn initial(&self) -> GeminiState {
        GeminiState::default()
    }

    fn decode_frame(&self, frame: &str) -> Result<GeminiChunk, LlmError> {
        serde_json::from_str(frame.trim()).map_err(|e| LlmError::Decode(e.to_string()))
    }

    fn step(&self, state: &mut GeminiState, chunk: GeminiChunk) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        if let Some(usage) = chunk.usage_metadata {
            state.usage = Usage {
                input: usage.prompt,
                // Output is reasoning-inclusive: candidates + thoughts (Gemini reports them apart).
                output: usage.candidates + usage.thoughts,
                cache_read: usage.cached,
                cache_write: 0,
            };
            state.usage_seen = true;
        }
        for candidate in chunk.candidates {
            for part in candidate.content.unwrap_or_default().parts {
                if let Some(text) = part.text {
                    if !state.text_started {
                        state.text_started = true;
                        out.push(LlmEvent::TextStart {
                            id: TEXT_BLOCK_ID.to_string(),
                        });
                    }
                    if !text.is_empty() {
                        out.push(LlmEvent::TextDelta {
                            id: TEXT_BLOCK_ID.to_string(),
                            text,
                        });
                    }
                }
                if let Some(call) = part.function_call {
                    // Gemini function calls carry no id; synthesize a stable per-stream one.
                    let id = format!("call_{}", state.tool_counter);
                    state.tool_counter += 1;
                    out.push(LlmEvent::ToolInputStart {
                        id: id.clone(),
                        name: call.name.clone(),
                    });
                    out.push(LlmEvent::ToolInputEnd { id: id.clone() });
                    out.push(LlmEvent::ToolCall {
                        id,
                        name: call.name,
                        input: call.args,
                    });
                }
            }
            if let Some(reason) = candidate.finish_reason {
                state.finish_reason = Some(map_finish_reason(&reason));
            }
        }
        out
    }

    fn terminal(&self, _chunk: &GeminiChunk) -> bool {
        false
    }

    fn on_halt(&self, state: &GeminiState) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        if state.text_started {
            out.push(LlmEvent::TextEnd {
                id: TEXT_BLOCK_ID.to_string(),
            });
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

    // Recorded cassettes from `packages/llm/test/fixtures/recordings/gemini/`.
    const STREAMS_TEXT: &str = r#"data: {"candidates": [{"content": {"parts": [{"text": "Hello!"}],"role": "model"},"finishReason": "STOP","index": 0}],"usageMetadata": {"promptTokenCount": 11,"candidatesTokenCount": 2,"totalTokenCount": 29,"thoughtsTokenCount": 16},"modelVersion": "gemini-2.5-flash"}
"#;

    const STREAMS_TOOL_CALL: &str = r#"data: {"candidates": [{"content": {"parts": [{"functionCall": {"name": "get_weather","args": {"city": "Paris"}}}],"role": "model"},"finishReason": "STOP","index": 0}],"usageMetadata": {"promptTokenCount": 55,"candidatesTokenCount": 15,"totalTokenCount": 115,"thoughtsTokenCount": 45},"modelVersion": "gemini-2.5-flash"}
"#;

    #[test]
    fn decodes_text_stream() {
        let events = decode_sse(&Gemini, STREAMS_TEXT).unwrap();
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello!");
        match events.last() {
            Some(LlmEvent::Finish { reason, usage }) => {
                assert_eq!(*reason, FinishReason::Stop);
                let usage = usage.unwrap();
                assert_eq!(usage.input, 11);
                // candidates(2) + thoughts(16), reasoning-inclusive.
                assert_eq!(usage.output, 18);
            }
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    #[test]
    fn decodes_tool_call_stream() {
        let events = decode_sse(&Gemini, STREAMS_TOOL_CALL).unwrap();
        let call = events
            .iter()
            .find_map(|e| match e {
                LlmEvent::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
                _ => None,
            })
            .expect("a tool-call event");
        assert_eq!(call.0, "get_weather");
        assert_eq!(call.1, json!({ "city": "Paris" }));
        assert!(events
            .iter()
            .any(|e| matches!(e, LlmEvent::ToolInputStart { .. })));
        match events.last() {
            Some(LlmEvent::Finish { usage, .. }) => {
                let usage = usage.unwrap();
                assert_eq!(usage.input, 55);
                assert_eq!(usage.output, 60); // 15 + 45
            }
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    fn body_value(request: &LlmRequest) -> serde_json::Value {
        serde_json::to_value(Gemini.build_body(request).unwrap()).unwrap()
    }

    #[test]
    fn lowers_text_request_to_cassette_body() {
        let request = LlmRequest {
            model: "gemini-2.5-flash".into(),
            system: vec!["You are concise.".into()],
            messages: vec![Message::user_text("Reply with exactly: Hello!")],
            generation: Generation {
                max_tokens: Some(80),
                temperature: Some(0.0),
                ..Default::default()
            },
            ..Default::default()
        };
        let body = body_value(&request);
        assert_eq!(
            body["contents"],
            json!([{"role": "user", "parts": [{"text": "Reply with exactly: Hello!"}]}])
        );
        assert_eq!(
            body["systemInstruction"],
            json!({"parts": [{"text": "You are concise."}]})
        );
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 80);
        assert_eq!(body["generationConfig"]["temperature"].as_f64(), Some(0.0));
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn lowers_tool_request_to_cassette_body() {
        let schema = json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"]
        });
        let request = LlmRequest {
            model: "gemini-2.5-flash".into(),
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
            json!([{"functionDeclarations": [{
                "name": "get_weather",
                "description": "Get current weather for a city.",
                "parameters": schema
            }]}])
        );
        assert_eq!(
            body["toolConfig"],
            json!({"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["get_weather"]}})
        );
    }

    #[test]
    fn tool_config_modes() {
        let mode = |choice: ToolChoice| {
            body_value(&LlmRequest {
                model: "m".into(),
                tool_choice: Some(choice),
                ..Default::default()
            })["toolConfig"]["functionCallingConfig"]["mode"]
                .clone()
        };
        assert_eq!(mode(ToolChoice::Auto), json!("AUTO"));
        assert_eq!(mode(ToolChoice::None), json!("NONE"));
        assert_eq!(mode(ToolChoice::Required), json!("ANY"));
    }

    #[test]
    fn lowers_assistant_function_call_and_response() {
        let request = LlmRequest {
            model: "m".into(),
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolCall {
                        id: "ignored".into(),
                        name: "get_weather".into(),
                        input: json!({ "city": "Paris" }),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        id: "ignored".into(),
                        name: "get_weather".into(),
                        result: json!({ "temp": 18 }),
                    }],
                },
            ],
            ..Default::default()
        };
        let body = body_value(&request);
        assert_eq!(
            body["contents"][0],
            json!({"role": "model", "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}}]})
        );
        assert_eq!(
            body["contents"][1],
            json!({"role": "user", "parts": [{"functionResponse": {"name": "get_weather", "response": {"temp": 18}}}]})
        );
    }
}
