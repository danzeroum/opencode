//! Serving the web frontend (`packages/app` build output) from the Rust binary — the Rust analog of
//! the TypeScript `serveUIEffect` / embedded-UI. Point `OPENCODE_WEB_DIR` at the built SPA assets and
//! the server serves them as the router fallback (after the API routes), with an SPA fallback to
//! `index.html` for unmatched non-API GETs. (Embedding the assets into the binary at release time —
//! the TS build's `createEmbeddedWebUIBundle` equivalent — is a follow-up; serve-from-dir is the
//! runtime mechanism and the default-#3 "app served by the Rust binary".)

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;

/// The configured web-assets directory (`OPENCODE_WEB_DIR`), or `None` to disable static serving.
pub fn web_dir() -> Option<&'static PathBuf> {
    static WEB_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    WEB_DIR
        .get_or_init(|| {
            std::env::var("OPENCODE_WEB_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .as_ref()
}

/// Content-type by file extension (the subset a SolidJS/Vite SPA ships).
fn content_type(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Resolve `req_path` to a file strictly under `dir`, rejecting traversal (`..`/`.`/empty segments)
/// and absolute escapes. Returns the bytes + content-type, or `None` if not a readable file.
async fn read_under(dir: &Path, req_path: &str) -> Option<(Vec<u8>, &'static str)> {
    let rel = req_path.trim_start_matches('/');
    if rel.is_empty()
        || rel
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return None;
    }
    let bytes = tokio::fs::read(dir.join(rel)).await.ok()?;
    Some((bytes, content_type(rel)))
}

fn ok_response(bytes: Vec<u8>, ct: &'static str) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from(bytes))
        .expect("static response")
}

/// Serve the exact asset at `req_path` under `dir`, or `None` if it isn't a file.
pub async fn serve_asset(dir: &Path, req_path: &str) -> Option<Response> {
    let (bytes, ct) = read_under(dir, req_path).await?;
    Some(ok_response(bytes, ct))
}

/// Serve `index.html` (the SPA entry / client-routing fallback), or `None` if absent.
pub async fn serve_index(dir: &Path) -> Option<Response> {
    let bytes = tokio::fs::read(dir.join("index.html")).await.ok()?;
    Some(ok_response(bytes, "text/html; charset=utf-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serves_assets_and_spa_index_and_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("index.html"),
            b"<!doctype html><div id=app>",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("assets")).unwrap();
        std::fs::write(dir.path().join("assets/app.js"), b"console.log(1)").unwrap();

        // Exact asset with correct content-type.
        let resp = serve_asset(dir.path(), "/assets/app.js").await.unwrap();
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/javascript; charset=utf-8"
        );
        // Missing asset → None (caller does the SPA fallback).
        assert!(serve_asset(dir.path(), "/session/abc").await.is_none());
        // SPA index fallback.
        assert!(serve_index(dir.path()).await.is_some());
        // Path traversal is rejected.
        assert!(serve_asset(dir.path(), "/../Cargo.toml").await.is_none());
        assert!(serve_asset(dir.path(), "/assets/../../secret")
            .await
            .is_none());
    }
}
