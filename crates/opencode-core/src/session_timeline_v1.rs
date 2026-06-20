//! Read-time projection of the stored V2 [`SessionMessage`] timeline into the **V1**
//! [`MessageWithParts`] shape that `session.message`/`session.messages`
//! (`GET /session/{id}/message[/{messageID}]`) return — the endpoints the web GUI's chat history
//! actually consumes (`@opencode-ai/sdk/v2` maps `session.messages` to the V1 path).
//!
//! The `session_message` table (written by the runner via [`crate::session_timeline::project_turn`])
//! is the single source of truth; this maps it to V1 without a second store. V2 → V1 needs a little
//! back-fill the timeline doesn't carry: a `User` entry has no `agent`/`model` (those are session-level)
//! and an assistant turn has no working-directory `path` — both are supplied by the caller from the
//! session record. Part ids are synthesised deterministically (`prt_{messageID}_{index}`) so they're
//! stable across reloads.
//!
//! Marker entries that V1 doesn't model (`agent-switched`/`model-switched`/`system`/`shell`/
//! `compaction`) are skipped — their producers (mid-run switches, `session.shell`, `session.summarize`)
//! aren't wired in the Rust runner yet; see `docs/PENDENCIAS.md`.

use opencode_proto::{
    AssistantMessage, AssistantMessageTime, Message, MessageError, MessageModelRef, MessagePath,
    MessageTokens, MessageUnknownError, MessageUnknownErrorData, MessageWithParts, Part, PartTime,
    ReasoningPart, SessionMessage, SessionMessageAssistantContent, SessionMessageToolState,
    TextPart, TokenCache, ToolContent, ToolPart, ToolState, ToolStateCompleted, ToolStateError,
    ToolStatePending, ToolStateRunning, ToolTimeCompleted, ToolTimeStart, ToolTimeStartEnd,
    UserMessage, UserMessageTime,
};

/// Concatenate a tool call's output content blocks into display text.
fn tool_content_text(content: &[ToolContent]) -> String {
    let mut out = String::new();
    for block in content {
        if let ToolContent::Text { text } = block {
            out.push_str(text);
        }
    }
    out
}

/// Zero token usage — the V1 fallback when a turn carries none.
fn empty_tokens() -> MessageTokens {
    MessageTokens {
        total: None,
        input: 0.0,
        output: 0.0,
        reasoning: 0.0,
        cache: TokenCache {
            read: 0.0,
            write: 0.0,
        },
    }
}

/// Map a V2 tool-call state into the V1 [`ToolState`]. `name` becomes the completed-state `title`
/// (required in V1; the timeline has no separate title) and `created` seeds the V1 time fields the V2
/// state lacks.
fn v1_tool_state(state: &SessionMessageToolState, name: &str, created: i64) -> ToolState {
    match state {
        // V2 `pending.input` is the raw (still-streaming) argument string; V1 splits it into a parsed
        // `input` object (none yet) plus the `raw` string.
        SessionMessageToolState::Pending { input } => ToolState::Pending(ToolStatePending {
            status: "pending".to_string(),
            input: serde_json::json!({}),
            raw: input.clone(),
        }),
        SessionMessageToolState::Running { input, .. } => ToolState::Running(ToolStateRunning {
            status: "running".to_string(),
            input: input.clone(),
            title: None,
            metadata: None,
            time: ToolTimeStart { start: created },
        }),
        SessionMessageToolState::Completed { input, content, .. } => {
            ToolState::Completed(ToolStateCompleted {
                status: "completed".to_string(),
                input: input.clone(),
                output: tool_content_text(content),
                title: name.to_string(),
                metadata: serde_json::json!({}),
                time: ToolTimeCompleted {
                    start: created,
                    end: created,
                    compacted: None,
                },
                attachments: None,
            })
        }
        SessionMessageToolState::Error { input, error, .. } => ToolState::Error(ToolStateError {
            status: "error".to_string(),
            input: input.clone(),
            error: error.message.clone(),
            metadata: None,
            time: ToolTimeStartEnd {
                start: created,
                end: created,
            },
        }),
    }
}

/// Build a user/synthetic message's single text part.
fn user_text_part(message_id: &str, session_id: &str, text: String, synthetic: bool) -> Part {
    Part::Text(TextPart {
        id: format!("prt_{message_id}_0"),
        session_id: session_id.to_string(),
        message_id: message_id.to_string(),
        r#type: "text".to_string(),
        text,
        synthetic: synthetic.then_some(true),
        ignored: None,
        time: None,
        metadata: None,
    })
}

/// Project the stored V2 timeline into the V1 `{ info, parts }` list.
///
/// `fallback_agent`/`fallback_model` back-fill user messages (the timeline tracks neither at the user
/// level); `directory` fills the assistant `path` (`cwd`/`root`). Returns entries in input order
/// (callers pass the timeline in the order they want returned).
pub fn timeline_to_v1(
    timeline: &[SessionMessage],
    session_id: &str,
    fallback_agent: &str,
    fallback_model: &MessageModelRef,
    directory: &str,
) -> Vec<MessageWithParts> {
    let mut out = Vec::new();
    // An assistant turn's `parentID` is the most recent user message before it.
    let mut last_user_id: Option<String> = None;
    for entry in timeline {
        match entry {
            SessionMessage::User { id, time, text, .. } => {
                last_user_id = Some(id.clone());
                let parts = if text.is_empty() {
                    Vec::new()
                } else {
                    vec![user_text_part(id, session_id, text.clone(), false)]
                };
                out.push(MessageWithParts {
                    info: Message::User(UserMessage {
                        id: id.clone(),
                        session_id: session_id.to_string(),
                        role: "user".to_string(),
                        time: UserMessageTime {
                            created: time.created,
                        },
                        format: None,
                        summary: None,
                        agent: fallback_agent.to_string(),
                        model: fallback_model.clone(),
                        system: None,
                        tools: None,
                    }),
                    parts,
                });
            }
            SessionMessage::Synthetic { id, time, text, .. } => {
                last_user_id = Some(id.clone());
                out.push(MessageWithParts {
                    info: Message::User(UserMessage {
                        id: id.clone(),
                        session_id: session_id.to_string(),
                        role: "user".to_string(),
                        time: UserMessageTime {
                            created: time.created,
                        },
                        format: None,
                        summary: None,
                        agent: fallback_agent.to_string(),
                        model: fallback_model.clone(),
                        system: None,
                        tools: None,
                    }),
                    parts: vec![user_text_part(id, session_id, text.clone(), true)],
                });
            }
            SessionMessage::Assistant {
                id,
                time,
                agent,
                model,
                content,
                finish,
                cost,
                tokens,
                error,
                ..
            } => {
                let created = time.created as i64;
                let mut parts = Vec::new();
                for (index, block) in content.iter().enumerate() {
                    let part_id = format!("prt_{id}_{index}");
                    match block {
                        SessionMessageAssistantContent::Text { text, .. } => {
                            parts.push(Part::Text(TextPart {
                                id: part_id,
                                session_id: session_id.to_string(),
                                message_id: id.clone(),
                                r#type: "text".to_string(),
                                text: text.clone(),
                                synthetic: None,
                                ignored: None,
                                time: None,
                                metadata: None,
                            }));
                        }
                        SessionMessageAssistantContent::Reasoning { text, .. } => {
                            parts.push(Part::Reasoning(ReasoningPart {
                                id: part_id,
                                session_id: session_id.to_string(),
                                message_id: id.clone(),
                                r#type: "reasoning".to_string(),
                                text: text.clone(),
                                metadata: None,
                                time: PartTime {
                                    start: created,
                                    end: None,
                                },
                            }));
                        }
                        SessionMessageAssistantContent::Tool {
                            id: call_id,
                            name,
                            state,
                            ..
                        } => {
                            parts.push(Part::Tool(ToolPart {
                                id: part_id,
                                session_id: session_id.to_string(),
                                message_id: id.clone(),
                                r#type: "tool".to_string(),
                                call_id: call_id.clone(),
                                tool: name.clone(),
                                state: v1_tool_state(state, name, created),
                                metadata: None,
                            }));
                        }
                    }
                }
                out.push(MessageWithParts {
                    info: Message::Assistant(AssistantMessage {
                        id: id.clone(),
                        session_id: session_id.to_string(),
                        role: "assistant".to_string(),
                        time: AssistantMessageTime {
                            created,
                            completed: time.completed.map(|c| c as i64),
                        },
                        error: error.as_ref().map(|e| {
                            MessageError::Unknown(MessageUnknownError {
                                name: "UnknownError".to_string(),
                                data: MessageUnknownErrorData {
                                    message: e.message.clone(),
                                    reference: None,
                                },
                            })
                        }),
                        parent_id: last_user_id.clone().unwrap_or_default(),
                        model_id: model.id.clone(),
                        provider_id: model.provider_id.clone(),
                        mode: agent.clone(),
                        agent: agent.clone(),
                        path: MessagePath {
                            cwd: directory.to_string(),
                            root: directory.to_string(),
                        },
                        summary: None,
                        cost: cost.unwrap_or(0.0),
                        tokens: tokens
                            .as_ref()
                            .map(|t| MessageTokens {
                                total: Some(t.input + t.output),
                                input: t.input,
                                output: t.output,
                                reasoning: t.reasoning,
                                cache: t.cache.clone(),
                            })
                            .unwrap_or_else(empty_tokens),
                        structured: None,
                        variant: None,
                        finish: finish.clone(),
                    }),
                    parts,
                });
            }
            // Marker entries V1 doesn't model — skipped (see module docs).
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode_proto::{MessageTime, MessageTimeCompleted, ModelRef};

    fn model() -> MessageModelRef {
        MessageModelRef {
            provider_id: "anthropic".to_string(),
            model_id: "claude".to_string(),
            variant: None,
        }
    }

    #[test]
    fn user_then_assistant_projects_to_v1_with_parent_and_parts() {
        let timeline = vec![
            SessionMessage::User {
                id: "msg_u".to_string(),
                metadata: None,
                time: MessageTime { created: 1.0 },
                text: "hi".to_string(),
                files: None,
                agents: None,
            },
            SessionMessage::Assistant {
                id: "msg_a".to_string(),
                metadata: None,
                time: MessageTimeCompleted {
                    created: 2.0,
                    completed: Some(3.0),
                },
                agent: "build".to_string(),
                model: ModelRef {
                    id: "claude".to_string(),
                    provider_id: "anthropic".to_string(),
                    variant: None,
                },
                content: vec![SessionMessageAssistantContent::Text {
                    id: "c0".to_string(),
                    text: "hello".to_string(),
                }],
                snapshot: None,
                finish: Some("stop".to_string()),
                cost: Some(0.5),
                tokens: None,
                error: None,
            },
        ];
        let out = timeline_to_v1(&timeline, "ses_1", "build", &model(), "/work");
        assert_eq!(out.len(), 2);
        // User message + its text part.
        match &out[0].info {
            Message::User(u) => {
                assert_eq!(u.id, "msg_u");
                assert_eq!(u.agent, "build");
                assert_eq!(u.model.provider_id, "anthropic");
            }
            _ => panic!("expected user"),
        }
        assert_eq!(out[0].parts.len(), 1);
        // Assistant message: parentID points at the user, path filled, defaulted tokens, one text part.
        match &out[1].info {
            Message::Assistant(a) => {
                assert_eq!(a.parent_id, "msg_u");
                assert_eq!(a.provider_id, "anthropic");
                assert_eq!(a.path.cwd, "/work");
                assert_eq!(a.cost, 0.5);
                assert_eq!(a.time.completed, Some(3));
            }
            _ => panic!("expected assistant"),
        }
        match &out[1].parts[0] {
            Part::Text(t) => {
                assert_eq!(t.text, "hello");
                assert_eq!(t.id, "prt_msg_a_0");
            }
            _ => panic!("expected text part"),
        }
    }

    #[test]
    fn completed_tool_call_maps_state_and_output() {
        let timeline = vec![SessionMessage::Assistant {
            id: "msg_a".to_string(),
            metadata: None,
            time: MessageTimeCompleted {
                created: 1.0,
                completed: None,
            },
            agent: "build".to_string(),
            model: ModelRef {
                id: "claude".to_string(),
                provider_id: "anthropic".to_string(),
                variant: None,
            },
            content: vec![SessionMessageAssistantContent::Tool {
                id: "call_1".to_string(),
                name: "read".to_string(),
                provider: None,
                state: SessionMessageToolState::Completed {
                    input: serde_json::json!({ "path": "a.rs" }),
                    attachments: None,
                    content: vec![ToolContent::Text {
                        text: "file body".to_string(),
                    }],
                    output_paths: None,
                    structured: serde_json::json!({}),
                    result: None,
                },
                time: opencode_proto::ToolTime {
                    created: 1.0,
                    ran: None,
                    completed: None,
                    pruned: None,
                },
            }],
            snapshot: None,
            finish: None,
            cost: None,
            tokens: None,
            error: None,
        }];
        let out = timeline_to_v1(&timeline, "ses_1", "build", &model(), "/work");
        match &out[0].parts[0] {
            Part::Tool(tp) => {
                assert_eq!(tp.call_id, "call_1");
                assert_eq!(tp.tool, "read");
                match &tp.state {
                    ToolState::Completed(c) => {
                        assert_eq!(c.output, "file body");
                        assert_eq!(c.title, "read");
                    }
                    _ => panic!("expected completed state"),
                }
            }
            _ => panic!("expected tool part"),
        }
    }
}
