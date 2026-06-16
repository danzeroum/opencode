//! HTTP transport for protocol streams (`packages/llm/src/route/transport/http.ts` analog): lower the
//! request, POST it, and decode the SSE response into normalized events.
//!
//! This increment collects the response then decodes it (the events are identical to incremental
//! decoding; chunk-by-chunk streaming is a refinement). It runs over plain HTTP — the workspace
//! `reqwest` has no TLS backend yet; re-adding `rustls`+`ring` for real HTTPS provider calls is a
//! focused follow-up. Retry/backoff + secret redaction (`route/executor.ts`) are a later increment;
//! [`LlmError::Status::retryable`] is the classification that executor will consume.

use crate::{decode_sse, LlmError, LlmEvent, LlmRequest, Protocol};

/// Whether a non-success status is worth retrying (429 + 5xx-ish), mirroring `route/executor.ts`'s
/// status classification.
fn is_retryable(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504 | 529)
}

/// POST `protocol.build_body(request)` to `url` (with extra `headers`, e.g. `x-api-key`) and decode
/// the SSE response into the normalized event stream. Non-2xx responses become
/// [`LlmError::Status`]; network failures become [`LlmError::Http`].
pub async fn complete<P: Protocol>(
    client: &reqwest::Client,
    url: &str,
    headers: &[(&str, &str)],
    protocol: &P,
    request: &LlmRequest,
) -> Result<Vec<LlmEvent>, LlmError> {
    let body = protocol.build_body(request)?;
    let mut builder = client.post(url).json(&body);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = builder
        .send()
        .await
        .map_err(|e| LlmError::Http(e.to_string()))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|e| LlmError::Http(e.to_string()))?;
    if status >= 400 {
        return Err(LlmError::Status {
            code: status,
            retryable: is_retryable(status),
            message: text,
        });
    }
    decode_sse(protocol, &text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::AnthropicMessages;
    use crate::{LlmRequest, Message};
    use axum::{http::StatusCode, response::IntoResponse, routing::post, Router};

    const SSE_BODY: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    /// Spawn an in-test HTTP server (axum, already a vetted workspace dep — no wiremock/TLS) and return
    /// its base URL.
    async fn spawn(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn request() -> LlmRequest {
        LlmRequest {
            model: "claude-haiku-4-5-20251001".into(),
            messages: vec![Message::user_text("hi")],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn posts_and_decodes_the_sse_response() {
        let app = Router::new().route(
            "/v1/messages",
            post(|| async { ([("content-type", "text/event-stream")], SSE_BODY) }),
        );
        let base = spawn(app).await;
        let client = reqwest::Client::new();
        let events = complete(
            &client,
            &format!("{base}/v1/messages"),
            &[("x-api-key", "test"), ("anthropic-version", "2023-06-01")],
            &AnthropicMessages,
            &request(),
        )
        .await
        .unwrap();

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hi");
        assert!(matches!(events.last(), Some(LlmEvent::Finish { .. })));
    }

    #[tokio::test]
    async fn rate_limit_is_retryable_status() {
        let app = Router::new().route(
            "/v1/messages",
            post(|| async { (StatusCode::TOO_MANY_REQUESTS, "slow down").into_response() }),
        );
        let base = spawn(app).await;
        let client = reqwest::Client::new();
        let err = complete(
            &client,
            &format!("{base}/v1/messages"),
            &[],
            &AnthropicMessages,
            &request(),
        )
        .await
        .unwrap_err();
        match err {
            LlmError::Status {
                code, retryable, ..
            } => {
                assert_eq!(code, 429);
                assert!(retryable);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unauthorized_is_non_retryable_status() {
        let app = Router::new().route(
            "/v1/messages",
            post(|| async { (StatusCode::UNAUTHORIZED, "bad key").into_response() }),
        );
        let base = spawn(app).await;
        let client = reqwest::Client::new();
        let err = complete(
            &client,
            &format!("{base}/v1/messages"),
            &[],
            &AnthropicMessages,
            &request(),
        )
        .await
        .unwrap_err();
        match err {
            LlmError::Status {
                code, retryable, ..
            } => {
                assert_eq!(code, 401);
                assert!(!retryable);
            }
            other => panic!("expected Status, got {other:?}"),
        }
    }
}
