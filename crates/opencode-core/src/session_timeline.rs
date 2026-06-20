//! Native session **projector**: fold a finished turn's conversation
//! ([`opencode_llm::Message`]s) into the V2 timeline ([`opencode_proto::SessionMessage`] rows the
//! `session_message` table stores and `v2.session.messages` serves).
//!
//! This replaces porting the entangled TS event-fold projector (`packages/core/src/session/projector.ts`,
//! coupled to Effect/drizzle): the Rust runner already produces the conversation, so we map it directly.
//! A pure function — the runner persists each entry via `SessionMessageStore::append` (a follow-up
//! slice). Mapping:
//! - `user` message → `SessionMessage::User { text }`
//! - `assistant` message → `SessionMessage::Assistant { content }` where text parts become `text`
//!   blocks and tool calls become `tool` blocks; a tool call's result (delivered in a later `tool`
//!   message, correlated by call id) sets its state to `completed`, else `running`.
//! - `tool` results are folded into the matching assistant tool block (not surfaced as their own
//!   entries); `system` messages are not surfaced.

use std::collections::BTreeMap;

use opencode_llm::{ContentPart, Message, Role, Usage};
use opencode_proto::{
    AssistantTokens, MessageTime, MessageTimeCompleted, ModelRef, SessionMessage,
    SessionMessageAssistantContent, SessionMessageToolState, TokenCache, ToolContent, ToolTime,
};

/// Concatenate a message's text parts (tool calls/results are handled separately).
fn text_of(content: &[ContentPart]) -> String {
    let mut out = String::new();
    for part in content {
        if let ContentPart::Text(t) = part {
            out.push_str(t);
        }
    }
    out
}

/// Render a tool result payload as display text: a JSON string stays as-is, anything else is
/// JSON-encoded (so structured results round-trip into the `text` content block).
fn result_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Token usage block for the assistant entry (reasoning isn't broken out by the normalized `Usage`).
fn tokens(usage: &Usage) -> AssistantTokens {
    AssistantTokens {
        input: usage.input as f64,
        output: usage.output as f64,
        reasoning: 0.0,
        cache: TokenCache {
            read: usage.cache_read as f64,
            write: usage.cache_write as f64,
        },
    }
}

/// Project a finished turn's `messages` (seed user prompt + per-step assistant/tool messages) into
/// timeline entries. `next_id` supplies fresh `msg_…`/content ids (injected for deterministic tests);
/// `created` is the entries' timestamp; summed `usage` is attributed to the final assistant entry.
pub fn project_turn(
    agent: &str,
    model: &ModelRef,
    messages: &[Message],
    usage: &Usage,
    created: f64,
    mut next_id: impl FnMut() -> String,
) -> Vec<SessionMessage> {
    // Pass 1: collect tool results by call id (they arrive in `tool` messages after the call).
    let mut results: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    for message in messages {
        for part in &message.content {
            if let ContentPart::ToolResult { id, result, .. } = part {
                results.insert(id.clone(), result.clone());
            }
        }
    }
    let last_assistant = messages.iter().rposition(|m| m.role == Role::Assistant);

    let mut out = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        match message.role {
            Role::User => out.push(SessionMessage::User {
                id: next_id(),
                metadata: None,
                time: MessageTime { created },
                text: text_of(&message.content),
                files: None,
                agents: None,
            }),
            Role::Assistant => {
                let mut content = Vec::new();
                for part in &message.content {
                    match part {
                        ContentPart::Text(t) => {
                            content.push(SessionMessageAssistantContent::Text {
                                id: next_id(),
                                text: t.clone(),
                            });
                        }
                        ContentPart::ToolCall { id, name, input } => {
                            let state = match results.get(id) {
                                Some(result) => SessionMessageToolState::Completed {
                                    input: input.clone(),
                                    attachments: None,
                                    content: vec![ToolContent::Text {
                                        text: result_text(result),
                                    }],
                                    output_paths: None,
                                    structured: serde_json::json!({}),
                                    result: Some(result.clone()),
                                },
                                None => SessionMessageToolState::Running {
                                    input: input.clone(),
                                    structured: serde_json::json!({}),
                                    content: Vec::new(),
                                },
                            };
                            content.push(SessionMessageAssistantContent::Tool {
                                id: id.clone(),
                                name: name.clone(),
                                provider: None,
                                state,
                                time: ToolTime {
                                    created,
                                    ran: None,
                                    completed: None,
                                    pruned: None,
                                },
                            });
                        }
                        // Results are folded into the matching tool block above.
                        ContentPart::ToolResult { .. } => {}
                    }
                }
                let is_last = Some(index) == last_assistant;
                out.push(SessionMessage::Assistant {
                    id: next_id(),
                    metadata: None,
                    time: MessageTimeCompleted {
                        created,
                        completed: Some(created),
                    },
                    agent: agent.to_string(),
                    model: model.clone(),
                    content,
                    snapshot: None,
                    finish: is_last.then(|| "stop".to_string()),
                    cost: None,
                    tokens: is_last.then(|| tokens(usage)),
                    error: None,
                });
            }
            // Tool results were collected in pass 1; system messages aren't part of the timeline.
            Role::Tool | Role::System => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> ModelRef {
        ModelRef {
            id: "claude".into(),
            provider_id: "anthropic".into(),
            variant: None,
        }
    }

    /// A deterministic id generator for tests (`c1`, `c2`, …).
    fn counter() -> impl FnMut() -> String {
        let mut n = 0;
        move || {
            n += 1;
            format!("c{n}")
        }
    }

    #[test]
    fn projects_user_then_assistant_text() {
        let messages = vec![
            Message::user_text("hello"),
            Message::assistant_text("hi there"),
        ];
        let out = project_turn(
            "build",
            &model(),
            &messages,
            &Usage::default(),
            100.0,
            counter(),
        );
        assert_eq!(out.len(), 2);
        match &out[0] {
            SessionMessage::User { text, time, .. } => {
                assert_eq!(text, "hello");
                assert_eq!(time.created, 100.0);
            }
            other => panic!("expected user, got {other:?}"),
        }
        match &out[1] {
            SessionMessage::Assistant {
                content,
                agent,
                model,
                ..
            } => {
                assert_eq!(agent, "build");
                assert_eq!(model.provider_id, "anthropic");
                assert_eq!(content.len(), 1);
                assert!(matches!(
                    &content[0],
                    SessionMessageAssistantContent::Text { text, .. } if text == "hi there"
                ));
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_with_result_becomes_completed_block() {
        let messages = vec![
            Message::user_text("read it"),
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    input: serde_json::json!({ "path": "a.rs" }),
                }],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    id: "call_1".into(),
                    name: "read".into(),
                    result: serde_json::json!("file contents"),
                }],
            },
        ];
        let out = project_turn(
            "build",
            &model(),
            &messages,
            &Usage::default(),
            1.0,
            counter(),
        );
        // user + assistant (the tool message folds into the assistant's tool block).
        assert_eq!(out.len(), 2);
        let SessionMessage::Assistant { content, .. } = &out[1] else {
            panic!("expected assistant");
        };
        assert_eq!(content.len(), 1);
        match &content[0] {
            SessionMessageAssistantContent::Tool {
                id, name, state, ..
            } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "read");
                match state {
                    SessionMessageToolState::Completed {
                        content, result, ..
                    } => {
                        assert_eq!(
                            result.as_ref().unwrap(),
                            &serde_json::json!("file contents")
                        );
                        assert!(matches!(
                            &content[0],
                            ToolContent::Text { text } if text == "file contents"
                        ));
                    }
                    other => panic!("expected completed, got {other:?}"),
                }
            }
            other => panic!("expected tool block, got {other:?}"),
        }
    }

    #[test]
    fn unresolved_tool_call_is_running() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentPart::ToolCall {
                id: "call_x".into(),
                name: "bash".into(),
                input: serde_json::json!({}),
            }],
        }];
        let out = project_turn("a", &model(), &messages, &Usage::default(), 1.0, counter());
        let SessionMessage::Assistant { content, .. } = &out[0] else {
            panic!("expected assistant");
        };
        assert!(matches!(
            &content[0],
            SessionMessageAssistantContent::Tool {
                state: SessionMessageToolState::Running { .. },
                ..
            }
        ));
    }

    #[test]
    fn usage_lands_on_the_final_assistant_only() {
        let messages = vec![
            Message::assistant_text("step one"),
            Message::assistant_text("step two"),
        ];
        let usage = Usage {
            input: 10,
            output: 20,
            cache_read: 3,
            cache_write: 1,
        };
        let out = project_turn("a", &model(), &messages, &usage, 1.0, counter());
        let first = matches!(&out[0], SessionMessage::Assistant { tokens, .. } if tokens.is_none());
        assert!(first, "earlier assistant carries no usage");
        match &out[1] {
            SessionMessage::Assistant { tokens, finish, .. } => {
                let t = tokens.as_ref().expect("final assistant carries usage");
                assert_eq!(t.input, 10.0);
                assert_eq!(t.output, 20.0);
                assert_eq!(t.cache.read, 3.0);
                assert_eq!(finish.as_deref(), Some("stop"));
            }
            other => panic!("expected assistant, got {other:?}"),
        }
    }
}
