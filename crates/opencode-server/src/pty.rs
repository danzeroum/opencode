//! Real PTY subsystem (the `pty` route group) backed by wezterm's `portable-pty`.
//!
//! A process-global [`PtyManager`] owns a registry of live [`PtySession`]s. Each session spawns a
//! child process attached to a pseudo-terminal; a dedicated OS thread drains the master's output into
//! a bounded scrollback ring (so the child never blocks on a full pty even with no client attached).
//! The `connect` route upgrades to a WebSocket and streams that scrollback + live output to the
//! client, forwarding the client's keystrokes back to the pty — the same protocol the TypeScript
//! `Pty.connect` implements (replay from a cursor, then a `0x00`-prefixed JSON meta frame, then live
//! bytes). `connect-token` issues a short-lived single-use ticket gated on the connect header + a
//! permitted Origin, mirroring `packages/core/src/pty`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::FromRequestParts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::{SinkExt, StreamExt};
use portable_pty::{
    native_pty_system, Child, CommandBuilder, MasterPty, PtySize as PortablePtySize,
};
use ulid::Ulid;

use opencode_proto::{
    EffectHttpApiForbidden, NotFoundData, NotFoundError, Pty, PtyConnectToken, PtyCreateRequest,
    PtyForbiddenError, PtyNotFoundError, PtyUpdateRequest,
};

/// Scrollback retained per session (matches the TS `BUFFER_LIMIT` of 2 MiB).
const BUFFER_LIMIT: usize = 2 * 1024 * 1024;
/// Max bytes per replay frame on connect (matches the TS `BUFFER_CHUNK` of 64 KiB).
const BUFFER_CHUNK: usize = 64 * 1024;
/// Connect-ticket lifetime in seconds (matches the TS `DEFAULT_TTL`).
const TICKET_TTL_SECS: i64 = 60;

/// Header that gates `connect-token` / ticketed `connect` (matches `pty-ticket.ts`).
const PTY_CONNECT_TOKEN_HEADER: &str = "x-opencode-ticket";
const PTY_CONNECT_TOKEN_HEADER_VALUE: &str = "1";
const PTY_CONNECT_TICKET_QUERY: &str = "ticket";

// ---------------------------------------------------------------------------
// Scrollback ring
// ---------------------------------------------------------------------------

/// The scrollback ring + absolute cursors for one PTY. `data` holds the most recent bytes; `base` is
/// the absolute cursor of `data[0]` and `end` is the absolute cursor just past the last byte
/// (`end == base + data.len()`). A subscriber replays from its own absolute cursor and is clamped to
/// what's still retained — matching the TS buffer/`bufferCursor`/`cursor` triplet.
struct PtyBuffer {
    data: Vec<u8>,
    base: usize,
    end: usize,
    /// Set by the reader thread when the child closes the pty (EOF).
    exited: bool,
}

impl PtyBuffer {
    fn append(&mut self, chunk: &[u8]) {
        self.data.extend_from_slice(chunk);
        self.end += chunk.len();
        if self.data.len() > BUFFER_LIMIT {
            let excess = self.data.len() - BUFFER_LIMIT;
            self.data.drain(..excess);
            self.base += excess;
        }
    }

    /// Bytes from absolute cursor `from` to `end` (clamped to retained data), plus the new cursor
    /// (`end`) the caller advances to after consuming them.
    fn read_from(&self, from: usize) -> (Vec<u8>, usize) {
        if from >= self.end {
            return (Vec::new(), self.end);
        }
        let start = from.max(self.base);
        let offset = start - self.base;
        (self.data[offset..].to_vec(), self.end)
    }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A live PTY: its OS-visible info, scrollback + wake signal, the master (resize), the writer
/// (input), and the child handle (kill).
pub struct PtySession {
    id: String,
    title: Mutex<String>,
    command: String,
    args: Vec<String>,
    cwd: String,
    pid: i64,
    buffer: Arc<Mutex<PtyBuffer>>,
    notify: Arc<tokio::sync::Notify>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
}

impl PtySession {
    /// A contract-shaped snapshot. `status` reflects whether the child has closed the pty.
    fn info(&self) -> Pty {
        Pty {
            id: self.id.clone(),
            title: self.title.lock().unwrap().clone(),
            command: self.command.clone(),
            args: self.args.clone(),
            cwd: self.cwd.clone(),
            status: if self.is_exited() {
                "exited"
            } else {
                "running"
            }
            .to_string(),
            pid: self.pid,
        }
    }

    fn is_exited(&self) -> bool {
        self.buffer.lock().unwrap().exited
    }

    /// Forward client keystrokes to the pty.
    fn write_input(&self, data: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(data);
            let _ = w.flush();
        }
    }
}

/// Drain the master's output into the scrollback ring on a dedicated OS thread (the read is blocking),
/// waking WebSocket writers after each chunk and on EOF.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    buffer: Arc<Mutex<PtyBuffer>>,
    notify: Arc<tokio::sync::Notify>,
) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    buffer.lock().unwrap().append(&buf[..n]);
                    notify.notify_waiters();
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        buffer.lock().unwrap().exited = true;
        notify.notify_waiters();
    });
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// A pending single-use connect ticket.
struct Ticket {
    pty_id: String,
    expires_at: Instant,
}

/// Process-global registry of PTY sessions + connect tickets.
pub struct PtyManager {
    sessions: Mutex<HashMap<String, Arc<PtySession>>>,
    tickets: Mutex<HashMap<String, Ticket>>,
}

impl PtyManager {
    fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            tickets: Mutex::new(HashMap::new()),
        }
    }

    /// Spawn a real PTY + child process, register it, and return its info. Defaults mirror the TS
    /// `PtyPreparation`: an absent command runs the login shell (with `-l` for POSIX login shells),
    /// `cwd` defaults to the server's working directory, and `TERM`/`OPENCODE_TERMINAL` are set.
    pub fn create(&self, req: PtyCreateRequest) -> std::io::Result<Pty> {
        let to_io = |e: anyhow::Error| std::io::Error::other(e.to_string());

        let pair = native_pty_system()
            .openpty(PortablePtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(to_io)?;

        let command = req
            .command
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(default_shell);
        let mut args = req.args.clone().unwrap_or_default();
        if is_login_shell(&command) {
            args.push("-l".to_string());
        }
        let cwd = req
            .cwd
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_else(|_| ".".to_string())
            });

        let mut cmd = CommandBuilder::new(&command);
        cmd.args(&args);
        cmd.cwd(&cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("OPENCODE_TERMINAL", "1");
        if let Some(env) = &req.env {
            for (k, v) in env {
                cmd.env(k, v);
            }
        }

        let child = pair.slave.spawn_command(cmd).map_err(to_io)?;
        // Release the slave so the child's exit propagates as EOF on the master reader.
        drop(pair.slave);
        let pid = child.process_id().map(|p| p as i64).unwrap_or(0);
        let reader = pair.master.try_clone_reader().map_err(to_io)?;
        let writer = pair.master.take_writer().map_err(to_io)?;

        let id = format!("pty_{}", Ulid::new());
        let title = req
            .title
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("Terminal {}", &id[id.len().saturating_sub(4)..]));

        let buffer = Arc::new(Mutex::new(PtyBuffer {
            data: Vec::new(),
            base: 0,
            end: 0,
            exited: false,
        }));
        let notify = Arc::new(tokio::sync::Notify::new());
        spawn_reader(reader, buffer.clone(), notify.clone());

        let session = Arc::new(PtySession {
            id: id.clone(),
            title: Mutex::new(title),
            command,
            args,
            cwd,
            pid,
            buffer,
            notify,
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
        });
        let info = session.info();
        self.sessions.lock().unwrap().insert(id, session);
        Ok(info)
    }

    /// All running sessions; reaps any that have exited (matching the TS removal-on-exit).
    pub fn list(&self) -> Vec<Pty> {
        let mut sessions = self.sessions.lock().unwrap();
        let mut out = Vec::new();
        sessions.retain(|_, s| {
            if s.is_exited() {
                false
            } else {
                out.push(s.info());
                true
            }
        });
        out
    }

    /// A session's info, or `None` if unknown (or exited — which is then reaped).
    pub fn get(&self, id: &str) -> Option<Pty> {
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get(id) {
            Some(s) if s.is_exited() => {
                sessions.remove(id);
                None
            }
            Some(s) => Some(s.info()),
            None => None,
        }
    }

    /// Apply a title/size update; `None` if the session is unknown or exited.
    pub fn update(&self, id: &str, req: PtyUpdateRequest) -> Option<Pty> {
        let sessions = self.sessions.lock().unwrap();
        let session = match sessions.get(id) {
            Some(s) if !s.is_exited() => s,
            _ => return None,
        };
        if let Some(title) = req.title.filter(|t| !t.is_empty()) {
            *session.title.lock().unwrap() = title;
        }
        if let Some(size) = req.size {
            let _ = session.master.lock().unwrap().resize(PortablePtySize {
                rows: size.rows,
                cols: size.cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        Some(session.info())
    }

    /// Kill + remove a session; `false` if it was unknown.
    pub fn remove(&self, id: &str) -> bool {
        let session = self.sessions.lock().unwrap().remove(id);
        match session {
            Some(s) => {
                let _ = s.child.lock().unwrap().kill();
                true
            }
            None => false,
        }
    }

    /// The live session handle for a WebSocket attach; `None` if unknown (or exited — reaped).
    fn connect_session(&self, id: &str) -> Option<Arc<PtySession>> {
        let mut sessions = self.sessions.lock().unwrap();
        match sessions.get(id) {
            Some(s) if s.is_exited() => {
                sessions.remove(id);
                None
            }
            Some(s) => Some(s.clone()),
            None => None,
        }
    }

    /// Issue a single-use connect ticket (purging expired ones first).
    pub fn issue_ticket(&self, pty_id: &str) -> PtyConnectToken {
        let ticket = Ulid::new().to_string();
        let now = Instant::now();
        let mut tickets = self.tickets.lock().unwrap();
        tickets.retain(|_, t| t.expires_at > now);
        tickets.insert(
            ticket.clone(),
            Ticket {
                pty_id: pty_id.to_string(),
                expires_at: now + Duration::from_secs(TICKET_TTL_SECS as u64),
            },
        );
        PtyConnectToken {
            ticket,
            expires_in: TICKET_TTL_SECS,
        }
    }

    /// Validate + consume a ticket for `pty_id` (single use).
    pub fn consume_ticket(&self, ticket: &str, pty_id: &str) -> bool {
        let mut tickets = self.tickets.lock().unwrap();
        let now = Instant::now();
        match tickets.get(ticket) {
            Some(t) if t.expires_at <= now => {
                tickets.remove(ticket);
                false
            }
            Some(t) if t.pty_id != pty_id => false,
            Some(_) => {
                tickets.remove(ticket);
                true
            }
            None => false,
        }
    }
}

/// The process-global PTY registry (terminals are OS processes, so the registry is process-scoped).
pub fn manager() -> &'static PtyManager {
    static PTY_MANAGER: OnceLock<PtyManager> = OnceLock::new();
    PTY_MANAGER.get_or_init(PtyManager::new)
}

/// Optional WebSocket upgrade: `Some` for a real upgrade request, `None` for a plain GET (which gets
/// the JSON `200 boolean`). Needed because axum's `WebSocketUpgrade` only implements the non-optional
/// `FromRequestParts`, so `Option<WebSocketUpgrade>` isn't directly extractable.
pub struct MaybeWs(pub Option<WebSocketUpgrade>);

impl<S> FromRequestParts<S> for MaybeWs
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(MaybeWs(
            WebSocketUpgrade::from_request_parts(parts, state)
                .await
                .ok(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Route entry points (called by the thin annotated handlers in lib.rs)
// ---------------------------------------------------------------------------

/// `POST /pty/{ptyID}/connect-token` — issue a WebSocket connect ticket. Gated on the connect header
/// and a permitted Origin (403), and the session existing (404).
pub fn connect_token(id: String, headers: HeaderMap) -> Response {
    let header_ok = headers
        .get(PTY_CONNECT_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        == Some(PTY_CONNECT_TOKEN_HEADER_VALUE);
    if !header_ok || !origin_allowed(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(PtyForbiddenError {
                tag: "PtyForbiddenError".to_string(),
                message: "Invalid PTY connect token request".to_string(),
            }),
        )
            .into_response();
    }
    let manager = manager();
    if manager.get(&id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(PtyNotFoundError {
                tag: "PtyNotFoundError".to_string(),
                pty_id: id.clone(),
                message: format!("PTY session not found: {id}"),
            }),
        )
            .into_response();
    }
    Json(manager.issue_ticket(&id)).into_response()
}

/// `GET /pty/{ptyID}/connect` — attach to a PTY. For a WebSocket upgrade, streams scrollback + live
/// output and forwards keystrokes; for a plain GET, the JSON contract is a `200 boolean`. 404s an
/// unknown session; 403s an invalid ticket (when one is supplied).
pub async fn connect(
    id: String,
    params: HashMap<String, String>,
    headers: HeaderMap,
    ws: Option<WebSocketUpgrade>,
) -> Response {
    let manager = manager();
    let session = match manager.connect_session(&id) {
        Some(s) => s,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(NotFoundError {
                    name: "NotFoundError".to_string(),
                    data: NotFoundData {
                        message: format!("PTY session not found: {id}"),
                    },
                }),
            )
                .into_response();
        }
    };

    if let Some(ticket) = params
        .get(PTY_CONNECT_TICKET_QUERY)
        .filter(|s| !s.is_empty())
    {
        let valid = origin_allowed(&headers) && manager.consume_ticket(ticket, &id);
        if !valid {
            return (
                StatusCode::FORBIDDEN,
                Json(EffectHttpApiForbidden {
                    tag: "Forbidden".to_string(),
                }),
            )
                .into_response();
        }
    }

    match ws {
        Some(upgrade) => {
            let cursor = parse_cursor(&params);
            upgrade.on_upgrade(move |socket| handle_pty_socket(socket, session, cursor))
        }
        None => Json(true).into_response(),
    }
}

/// Replay-from-cursor (`-1` ⇒ live only, absent ⇒ full buffer) parsed from the query string.
fn parse_cursor(params: &HashMap<String, String>) -> Option<i64> {
    params
        .get("cursor")
        .and_then(|c| c.parse::<i64>().ok())
        .filter(|&c| c >= -1)
}

/// The bidirectional bridge: a writer task streams pty output → client, a reader task forwards client
/// input → pty. Either side ending tears the other down (and drops the socket).
async fn handle_pty_socket(socket: WebSocket, session: Arc<PtySession>, cursor: Option<i64>) {
    let (mut tx, mut rx) = socket.split();

    let writer_session = session.clone();
    let mut writer = tokio::spawn(async move {
        // Replay scrollback from the requested cursor, then a meta frame marking the live position.
        let (replay, mut local) = {
            let b = writer_session.buffer.lock().unwrap();
            let from = match cursor {
                Some(-1) => b.end,
                Some(c) if c >= 0 => c as usize,
                _ => 0,
            };
            b.read_from(from)
        };
        for chunk in replay.chunks(BUFFER_CHUNK) {
            if tx
                .send(Message::Binary(Bytes::copy_from_slice(chunk)))
                .await
                .is_err()
            {
                return;
            }
        }
        if tx.send(Message::Binary(meta_frame(local))).await.is_err() {
            return;
        }
        // Live: drain everything available, then wait for the next chunk (or exit).
        loop {
            let notified = writer_session.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            loop {
                let (chunk, new_end) = { writer_session.buffer.lock().unwrap().read_from(local) };
                if chunk.is_empty() {
                    break;
                }
                if tx.send(Message::Binary(Bytes::from(chunk))).await.is_err() {
                    return;
                }
                local = new_end;
            }
            if writer_session.is_exited() {
                let _ = tx.send(Message::Close(None)).await;
                return;
            }
            notified.await;
        }
    });

    let mut reader = tokio::spawn(async move {
        while let Some(Ok(msg)) = rx.next().await {
            match msg {
                Message::Text(t) => session.write_input(t.as_str().as_bytes()),
                Message::Binary(b) => session.write_input(&b),
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = &mut writer => reader.abort(),
        _ = &mut reader => writer.abort(),
    }
}

/// WebSocket control frame: `0x00` + UTF-8 JSON `{ cursor }` (matches the TS `meta()`).
fn meta_frame(cursor: usize) -> Bytes {
    let json = serde_json::json!({ "cursor": cursor }).to_string();
    let mut out = Vec::with_capacity(json.len() + 1);
    out.push(0u8);
    out.extend_from_slice(json.as_bytes());
    Bytes::from(out)
}

// ---------------------------------------------------------------------------
// Shell + origin helpers (mirror shell.ts / cors.ts)
// ---------------------------------------------------------------------------

fn default_shell() -> String {
    #[cfg(windows)]
    {
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string())
    }
    #[cfg(not(windows))]
    {
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string())
    }
}

/// Whether `command`'s basename is a POSIX login shell (gets a `-l`), matching the TS `META` table.
fn is_login_shell(command: &str) -> bool {
    let name = std::path::Path::new(command)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();
    matches!(
        name.as_str(),
        "bash" | "dash" | "fish" | "ksh" | "sh" | "zsh"
    )
}

fn origin_allowed(headers: &HeaderMap) -> bool {
    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    let host = headers.get("host").and_then(|v| v.to_str().ok());
    is_allowed_request_origin(origin, host)
}

/// Port of `isAllowedRequestOrigin`: no Origin ⇒ allowed; same host ⇒ allowed; else CORS rules.
fn is_allowed_request_origin(origin: Option<&str>, host: Option<&str>) -> bool {
    let origin = match origin.filter(|o| !o.is_empty()) {
        Some(o) => o,
        None => return true,
    };
    if let Some(h) = host {
        if url_host(origin).as_deref() == Some(h) {
            return true;
        }
    }
    is_allowed_cors_origin(origin)
}

/// Port of `isAllowedCorsOrigin` (the default, no configured allow-list).
fn is_allowed_cors_origin(origin: &str) -> bool {
    if origin.is_empty() {
        return true;
    }
    if origin.starts_with("http://localhost:") || origin.starts_with("http://127.0.0.1:") {
        return true;
    }
    if origin.starts_with("oc://renderer") {
        return true;
    }
    if matches!(
        origin,
        "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost"
    ) {
        return true;
    }
    is_opencode_origin(origin)
}

/// `^https://([a-z0-9-]+\.)*opencode\.ai$` — exact apex or a subdomain (rejects `evilopencode.ai`).
fn is_opencode_origin(origin: &str) -> bool {
    let host = match origin.strip_prefix("https://") {
        Some(h) => h,
        None => return false,
    };
    if host.contains('/') {
        return false;
    }
    host == "opencode.ai" || host.ends_with(".opencode.ai")
}

/// The `host` (incl. port) of a URL, matching JS `new URL(origin).host`.
fn url_host(origin: &str) -> Option<String> {
    let rest = origin.split_once("://").map(|(_, r)| r).unwrap_or(origin);
    let host = rest.split('/').next().unwrap_or(rest);
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_appends_trims_and_reads_from_cursor() {
        let mut b = PtyBuffer {
            data: Vec::new(),
            base: 0,
            end: 0,
            exited: false,
        };
        b.append(b"hello");
        let (data, end) = b.read_from(0);
        assert_eq!(data, b"hello");
        assert_eq!(end, 5);
        // Reading from the end yields nothing.
        assert_eq!(b.read_from(5).0, Vec::<u8>::new());
        // From an interior cursor.
        assert_eq!(b.read_from(2).0, b"llo");
    }

    #[test]
    fn buffer_trims_to_limit_and_clamps_stale_cursor() {
        let mut b = PtyBuffer {
            data: Vec::new(),
            base: 0,
            end: 0,
            exited: false,
        };
        let big = vec![b'x'; BUFFER_LIMIT + 1024];
        b.append(&big);
        assert_eq!(b.data.len(), BUFFER_LIMIT);
        assert_eq!(b.base, 1024);
        assert_eq!(b.end, BUFFER_LIMIT + 1024);
        // A cursor older than what's retained clamps to the retained window.
        let (data, end) = b.read_from(0);
        assert_eq!(data.len(), BUFFER_LIMIT);
        assert_eq!(end, BUFFER_LIMIT + 1024);
    }

    #[test]
    fn tickets_are_single_use_and_pty_scoped() {
        let m = PtyManager::new();
        let token = m.issue_ticket("pty_a");
        assert_eq!(token.expires_in, TICKET_TTL_SECS);
        // Wrong pty id is rejected without consuming.
        assert!(!m.consume_ticket(&token.ticket, "pty_b"));
        // Correct pty id consumes it once.
        assert!(m.consume_ticket(&token.ticket, "pty_a"));
        // Second use fails.
        assert!(!m.consume_ticket(&token.ticket, "pty_a"));
    }

    #[test]
    fn unknown_ticket_is_rejected() {
        let m = PtyManager::new();
        assert!(!m.consume_ticket("nope", "pty_a"));
    }

    #[test]
    fn login_shell_detection() {
        assert!(is_login_shell("/bin/bash"));
        assert!(is_login_shell("/usr/bin/zsh"));
        assert!(is_login_shell("sh"));
        assert!(!is_login_shell("/usr/bin/htop"));
        assert!(!is_login_shell("nu"));
    }

    #[test]
    fn origin_rules_match_cors_ts() {
        // No Origin → allowed.
        assert!(is_allowed_request_origin(None, Some("localhost:4096")));
        // Same host → allowed.
        assert!(is_allowed_request_origin(
            Some("http://localhost:4096"),
            Some("localhost:4096")
        ));
        // localhost / loopback with a different host → allowed by CORS rules.
        assert!(is_allowed_request_origin(
            Some("http://127.0.0.1:5173"),
            Some("localhost:4096")
        ));
        // opencode.ai subdomain allowed; look-alike rejected.
        assert!(is_allowed_cors_origin("https://app.opencode.ai"));
        assert!(is_allowed_cors_origin("https://opencode.ai"));
        assert!(!is_allowed_cors_origin("https://evilopencode.ai"));
        // A random cross-origin is rejected.
        assert!(!is_allowed_request_origin(
            Some("https://evil.example.com"),
            Some("localhost:4096")
        ));
    }

    #[test]
    fn create_get_update_remove_lifecycle() {
        let m = PtyManager::new();
        let created = m
            .create(PtyCreateRequest {
                command: Some("/bin/sh".to_string()),
                args: None,
                cwd: None,
                title: Some("test-term".to_string()),
                env: None,
            })
            .expect("spawn pty");
        assert_eq!(created.status, "running");
        assert_eq!(created.title, "test-term");
        assert!(created.pid > 0);
        assert!(created.id.starts_with("pty_"));

        // Visible via get + list.
        assert_eq!(m.get(&created.id).map(|p| p.id), Some(created.id.clone()));
        assert!(m.list().iter().any(|p| p.id == created.id));

        // Resize + retitle.
        let updated = m
            .update(
                &created.id,
                PtyUpdateRequest {
                    title: Some("renamed".to_string()),
                    size: Some(opencode_proto::PtySize {
                        rows: 40,
                        cols: 120,
                    }),
                },
            )
            .expect("update");
        assert_eq!(updated.title, "renamed");

        // Remove → gone.
        assert!(m.remove(&created.id));
        assert!(m.get(&created.id).is_none());
        assert!(!m.remove(&created.id));
    }

    #[test]
    fn meta_frame_is_zero_prefixed_json() {
        let frame = meta_frame(42);
        assert_eq!(frame[0], 0u8);
        let json: serde_json::Value = serde_json::from_slice(&frame[1..]).unwrap();
        assert_eq!(json["cursor"], 42);
    }
}
