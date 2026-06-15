//! Reverse-proxy fallback: forwards any not-yet-migrated request to the upstream TypeScript server.
//! This is what makes the strangler-fig seam reversible — a route group is "cut over" simply by
//! adding it to [`crate::RouteTable`]; until then its traffic streams through here unchanged
//! (including SSE, via a streaming body).

use axum::{
    body::Body,
    extract::{Request, State},
    http::header,
    response::{IntoResponse, Response},
};

use crate::ServerState;

/// Upstream TypeScript server connection.
pub struct Upstream {
    /// Base URL, e.g. `http://127.0.0.1:4097`.
    pub base_url: String,
    /// Shared HTTP client (connection-pooled).
    pub client: reqwest::Client,
}

impl Upstream {
    /// Create an upstream pointing at `base_url`.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            client: reqwest::Client::new(),
        }
    }
}

/// axum fallback handler: proxy the request to the upstream server.
pub async fn proxy_handler(State(state): State<ServerState>, req: Request) -> Response {
    match forward(&state, req).await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::error!(error = %err, "proxy to upstream failed");
            (
                axum::http::StatusCode::BAD_GATEWAY,
                format!("upstream proxy error: {err}"),
            )
                .into_response()
        }
    }
}

async fn forward(state: &ServerState, req: Request) -> anyhow::Result<Response> {
    let (parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let url = format!(
        "{}{}",
        state.proxy.base_url.trim_end_matches('/'),
        path_and_query
    );

    let bytes = axum::body::to_bytes(body, usize::MAX).await?;

    let mut builder = state
        .proxy
        .client
        .request(parts.method.clone(), &url)
        .body(bytes.to_vec());
    for (name, value) in parts.headers.iter() {
        if name == header::HOST {
            continue;
        }
        builder = builder.header(name.clone(), value.clone());
    }

    let upstream = builder.send().await?;

    let mut response = Response::builder().status(upstream.status());
    for (name, value) in upstream.headers().iter() {
        response = response.header(name.clone(), value.clone());
    }
    // Stream the upstream body through (works for SSE / chunked responses).
    let response = response.body(Body::from_stream(upstream.bytes_stream()))?;
    Ok(response)
}
