//! Native tools bridged to the runner's [`ToolBox`](crate::session::ToolBox) seam — the real,
//! `opencode-tools`-backed tools the session runner executes, replacing the test doubles.
//!
//! This is the first concrete step toward wiring the runner into the server: a [`NativeToolBox`] that
//! dispatches the agent's tool calls (`read` / `write` / `edit` / `ls` / `glob` / `grep` / `bash`) to
//! the ported `opencode-tools` helpers, plus the matching [`ToolDefinition`]s the model is offered via
//! [`tool_definitions`]. Authorization is **not** done here — that is the
//! [`PermissionGate`](crate::session::PermissionGate)'s job; this type only executes. Relative tool
//! paths resolve against the configured `root`. The filesystem/search helpers are synchronous, so they
//! run on `spawn_blocking`; `bash` is already async.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use opencode_llm::ToolDefinition;
use opencode_tools::process::{CommandOutput, ShellOptions};
use opencode_tools::ToolError;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::session::ToolBox;

/// Presents the model's questions to the user and returns their answers — the seam for the `question`
/// tool (the question-flow analogue of [`PermissionGate`](crate::session::PermissionGate)). The server
/// implements this over its pending-question store; without an asker the `question` tool isn't offered.
#[async_trait]
pub trait QuestionAsker: Send + Sync {
    /// Present `questions` for `session_id`; return the per-question answers (each a list of selected
    /// labels) or `Err` if the user dismissed them.
    async fn ask(
        &self,
        session_id: &str,
        questions: Vec<opencode_proto::QuestionV2Info>,
    ) -> Result<Vec<Vec<String>>, String>;
}

/// A [`ToolBox`] backed by the native `opencode-tools` helpers, rooted at a working directory.
pub struct NativeToolBox {
    root: PathBuf,
    shell_timeout: Duration,
    /// Optional human-in-the-loop asker enabling the `question` tool (set per session by the runner).
    asker: Option<Arc<dyn QuestionAsker>>,
    /// The session the `question` tool asks within.
    session_id: String,
}

impl NativeToolBox {
    /// A toolbox rooted at `root` (the project/working directory) with a 120s default `bash` timeout.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            shell_timeout: Duration::from_secs(120),
            asker: None,
            session_id: String::new(),
        }
    }

    /// Override the `bash` shell timeout.
    pub fn with_shell_timeout(mut self, timeout: Duration) -> Self {
        self.shell_timeout = timeout;
        self
    }

    /// Enable the `question` tool for `session_id`, routing the model's questions through `asker`.
    pub fn with_question_asker(
        mut self,
        asker: Arc<dyn QuestionAsker>,
        session_id: impl Into<String>,
    ) -> Self {
        self.asker = Some(asker);
        self.session_id = session_id.into();
        self
    }

    /// Resolve a (possibly relative) tool path against the toolbox root.
    fn resolve(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }
}

// ---- Tool input shapes (decoded from the model's JSON arguments) ----

#[derive(Deserialize)]
struct ReadInput {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

#[derive(Deserialize)]
struct EditInput {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize)]
struct LsInput {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize)]
struct BashInput {
    command: String,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Decode a tool's JSON arguments, mapping a decode failure to a tool-level error.
fn parse<T: serde::de::DeserializeOwned>(tool: &str, input: Value) -> Result<T, String> {
    serde_json::from_value(input).map_err(|e| format!("invalid input for `{tool}`: {e}"))
}

/// Run a synchronous `opencode-tools` helper off the async runtime, flattening the join + tool errors.
async fn blocking<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, ToolError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error.to_string()),
        Err(join) => Err(format!("tool task panicked: {join}")),
    }
}

/// A path relative to `root`, with `\` normalized to `/`.
fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Render a shell result: stdout, then stderr, then a status marker on non-success.
fn format_shell(out: &CommandOutput) -> String {
    let mut text = String::new();
    text.push_str(&out.stdout);
    if !out.stderr.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&out.stderr);
    }
    if out.timed_out {
        text.push_str("\n[timed out]");
    } else if out.code != Some(0) {
        let code = out
            .code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "killed".to_string());
        text.push_str(&format!("\n[exit code: {code}]"));
    }
    text
}

#[async_trait]
impl ToolBox for NativeToolBox {
    async fn invoke(&self, name: &str, input: Value) -> Result<String, String> {
        match name {
            "read" => {
                let i: ReadInput = parse(name, input)?;
                let path = self.resolve(&i.path);
                blocking(move || opencode_tools::read_file(path, i.offset, i.limit)).await
            }
            "write" => {
                let i: WriteInput = parse(name, input)?;
                let path = self.resolve(&i.path);
                let bytes = i.content.len();
                blocking(move || opencode_tools::files::write_file(&path, &i.content)).await?;
                Ok(format!("Wrote {bytes} bytes to {}", i.path))
            }
            "edit" => {
                let i: EditInput = parse(name, input)?;
                let path = self.resolve(&i.path);
                let replaced = blocking(move || {
                    opencode_tools::files::edit_file(
                        &path,
                        &i.old_string,
                        &i.new_string,
                        i.replace_all,
                    )
                })
                .await?;
                Ok(format!("Replaced {replaced} occurrence(s)"))
            }
            "ls" => {
                let i: LsInput = parse(name, input)?;
                let path = self.resolve(i.path.as_deref().unwrap_or("."));
                let entries = blocking(move || opencode_tools::files::list_dir(&path)).await?;
                Ok(entries.join("\n"))
            }
            "glob" => {
                let i: GlobInput = parse(name, input)?;
                let root = self.resolve(i.path.as_deref().unwrap_or("."));
                let for_rel = root.clone();
                let matches = blocking(move || opencode_tools::glob(&i.pattern, &root)).await?;
                Ok(matches
                    .iter()
                    .map(|p| rel(&for_rel, p))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "grep" => {
                let i: GrepInput = parse(name, input)?;
                let root = self.resolve(i.path.as_deref().unwrap_or("."));
                let for_rel = root.clone();
                let matches = blocking(move || opencode_tools::grep(&i.pattern, &root)).await?;
                Ok(matches
                    .iter()
                    .map(|m| format!("{}:{}:{}", rel(&for_rel, &m.path), m.line_number, m.line))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "bash" => {
                let i: BashInput = parse(name, input)?;
                let opts = ShellOptions {
                    cwd: Some(self.root.clone()),
                    timeout: i
                        .timeout_secs
                        .map(Duration::from_secs)
                        .unwrap_or(self.shell_timeout),
                    ..Default::default()
                };
                let out = opencode_tools::process::run_shell(&i.command, &opts)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(format_shell(&out))
            }
            "question" => {
                let asker = self
                    .asker
                    .as_ref()
                    .ok_or_else(|| "question tool is not available".to_string())?;
                let questions: Vec<opencode_proto::QuestionV2Info> =
                    serde_json::from_value(input.get("questions").cloned().unwrap_or(Value::Null))
                        .map_err(|e| format!("invalid question input: {e}"))?;
                let answers = asker.ask(&self.session_id, questions).await?;
                serde_json::to_string(&json!({ "answers": answers })).map_err(|e| e.to_string())
            }
            other => Err(format!("unknown tool: {other}")),
        }
    }
}

/// The `question` [`ToolDefinition`] — offered only when a [`QuestionAsker`] is wired (the runner adds
/// it per session). Lets the model ask the user one or more multiple-choice questions and receive their
/// selected labels.
pub fn question_tool_definition() -> ToolDefinition {
    ToolDefinition {
        name: "question".to_string(),
        description: Some(
            "Ask the user one or more multiple-choice questions and wait for their answers."
                .to_string(),
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": { "type": "string" },
                            "header": { "type": "string" },
                            "options": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string" },
                                        "description": { "type": "string" }
                                    },
                                    "required": ["label", "description"]
                                }
                            },
                            "multiple": { "type": "boolean" },
                            "custom": { "type": "boolean" }
                        },
                        "required": ["question", "header", "options"]
                    }
                }
            },
            "required": ["questions"]
        }),
    }
}

/// The [`ToolDefinition`]s that [`NativeToolBox`] implements — the tools (and JSON schemas) to offer the
/// model so its calls match what the toolbox dispatches.
pub fn tool_definitions() -> Vec<ToolDefinition> {
    fn def(name: &str, description: &str, schema: Value) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: Some(description.to_string()),
            input_schema: schema,
        }
    }
    vec![
        def(
            "read",
            "Read a UTF-8 file, optionally restricted to a line range.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "description": "0-based first line" },
                    "limit": { "type": "integer", "description": "max lines" }
                },
                "required": ["path"]
            }),
        ),
        def(
            "write",
            "Write contents to a file, creating parent directories.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" }, "content": { "type": "string" } },
                "required": ["path", "content"]
            }),
        ),
        def(
            "edit",
            "Replace a string in a file (must be unique unless replace_all).",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "old_string": { "type": "string" },
                    "new_string": { "type": "string" },
                    "replace_all": { "type": "boolean" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ),
        def(
            "ls",
            "List the entries directly under a directory.",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }),
        ),
        def(
            "glob",
            "Find files matching a glob pattern, honoring .gitignore.",
            json!({
                "type": "object",
                "properties": { "pattern": { "type": "string" }, "path": { "type": "string" } },
                "required": ["pattern"]
            }),
        ),
        def(
            "grep",
            "Search file contents by regular expression, honoring .gitignore.",
            json!({
                "type": "object",
                "properties": { "pattern": { "type": "string" }, "path": { "type": "string" } },
                "required": ["pattern"]
            }),
        ),
        def(
            "bash",
            "Run a shell command in the project root and capture its output.",
            json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_secs": { "type": "integer" }
                },
                "required": ["command"]
            }),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hello\nworld\nhello world\n").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b.rs"), "fn main() {}\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn read_returns_file_contents() {
        let dir = fixture();
        let tb = NativeToolBox::new(dir.path());
        let out = tb.invoke("read", json!({ "path": "a.txt" })).await.unwrap();
        assert_eq!(out, "hello\nworld\nhello world\n");
        let ranged = tb
            .invoke("read", json!({ "path": "a.txt", "offset": 1, "limit": 1 }))
            .await
            .unwrap();
        assert_eq!(ranged, "world");
    }

    #[tokio::test]
    async fn write_then_read_roundtrips() {
        let dir = fixture();
        let tb = NativeToolBox::new(dir.path());
        let msg = tb
            .invoke("write", json!({ "path": "new/file.txt", "content": "abc" }))
            .await
            .unwrap();
        assert!(msg.contains("Wrote 3 bytes"));
        assert_eq!(
            fs::read_to_string(dir.path().join("new/file.txt")).unwrap(),
            "abc"
        );
    }

    #[tokio::test]
    async fn edit_replaces_a_unique_string() {
        let dir = fixture();
        let tb = NativeToolBox::new(dir.path());
        let msg = tb
            .invoke(
                "edit",
                json!({ "path": "sub/b.rs", "old_string": "main", "new_string": "run" }),
            )
            .await
            .unwrap();
        assert!(msg.contains("Replaced 1"));
        assert_eq!(
            fs::read_to_string(dir.path().join("sub/b.rs")).unwrap(),
            "fn run() {}\n"
        );
    }

    #[tokio::test]
    async fn ls_glob_grep_walk_the_tree() {
        let dir = fixture();
        let tb = NativeToolBox::new(dir.path());

        let ls = tb.invoke("ls", json!({})).await.unwrap();
        assert!(ls.contains("a.txt"));
        assert!(ls.contains("sub/"));

        let globbed = tb
            .invoke("glob", json!({ "pattern": "**/*.rs" }))
            .await
            .unwrap();
        assert_eq!(globbed, "sub/b.rs");

        let grepped = tb
            .invoke("grep", json!({ "pattern": "world" }))
            .await
            .unwrap();
        assert!(grepped.contains("a.txt:2:world"));
        assert!(grepped.contains("a.txt:3:hello world"));
    }

    #[tokio::test]
    async fn bash_runs_in_the_root() {
        let dir = fixture();
        let tb = NativeToolBox::new(dir.path());
        let out = tb
            .invoke("bash", json!({ "command": "echo hi && ls a.txt" }))
            .await
            .unwrap();
        assert!(out.contains("hi"));
        assert!(out.contains("a.txt"));
    }

    #[tokio::test]
    async fn unknown_tool_and_bad_input_error() {
        let dir = fixture();
        let tb = NativeToolBox::new(dir.path());
        assert!(tb
            .invoke("nope", json!({}))
            .await
            .unwrap_err()
            .contains("unknown tool"));
        // `read` requires `path`.
        assert!(tb
            .invoke("read", json!({}))
            .await
            .unwrap_err()
            .contains("invalid input for `read`"));
        // A missing file is a tool-level error (fed back to the model, not a panic).
        assert!(tb
            .invoke("read", json!({ "path": "missing.txt" }))
            .await
            .is_err());
    }

    #[test]
    fn definitions_cover_the_dispatched_tools() {
        let defs = tool_definitions();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["read", "write", "edit", "ls", "glob", "grep", "bash"]
        );
    }

    #[tokio::test]
    async fn question_tool_routes_through_the_asker() {
        use crate::session::ToolBox;
        struct StubAsker;
        #[async_trait]
        impl QuestionAsker for StubAsker {
            async fn ask(
                &self,
                session_id: &str,
                questions: Vec<opencode_proto::QuestionV2Info>,
            ) -> Result<Vec<Vec<String>>, String> {
                assert_eq!(session_id, "ses_1");
                assert_eq!(questions.len(), 1);
                Ok(vec![vec!["yes".to_string()]])
            }
        }
        let tb = NativeToolBox::new(".").with_question_asker(Arc::new(StubAsker), "ses_1");
        let input = json!({
            "questions": [{
                "question": "Proceed?",
                "header": "Confirm",
                "options": [{ "label": "yes", "description": "go" }]
            }]
        });
        let out = tb.invoke("question", input).await.unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["answers"][0][0], "yes");
        // Without an asker the tool is unavailable.
        let bare = NativeToolBox::new(".");
        assert!(bare
            .invoke("question", json!({ "questions": [] }))
            .await
            .is_err());
    }

    // The native toolbox drives a real tool through the session runner loop.
    #[tokio::test]
    async fn runner_executes_a_native_tool() {
        use crate::session::{run, Session};
        use opencode_llm::{FinishReason, LlmError, LlmEvent, LlmRequest, Message, Usage};
        use std::sync::{Arc, Mutex};

        struct ScriptEngine(Mutex<std::collections::VecDeque<Vec<LlmEvent>>>);
        #[async_trait]
        impl crate::session::LlmEngine for ScriptEngine {
            async fn complete(&self, _r: &LlmRequest) -> Result<Vec<LlmEvent>, LlmError> {
                Ok(self.0.lock().unwrap().pop_front().unwrap_or_default())
            }
        }

        let dir = fixture();
        let engine = ScriptEngine(Mutex::new(
            vec![
                vec![
                    LlmEvent::ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        input: json!({ "path": "a.txt" }),
                    },
                    LlmEvent::Finish {
                        reason: FinishReason::ToolCalls,
                        usage: Some(Usage::default()),
                    },
                ],
                vec![
                    LlmEvent::TextDelta {
                        id: "t".into(),
                        text: "done".into(),
                    },
                    LlmEvent::Finish {
                        reason: FinishReason::Stop,
                        usage: Some(Usage::default()),
                    },
                ],
            ]
            .into(),
        ));
        let mut session = Session::new("m", 8);
        session.tools = tool_definitions();
        let run = run(
            &engine,
            Arc::new(NativeToolBox::new(dir.path())),
            &session,
            vec![Message::user_text("read a.txt")],
        )
        .await
        .unwrap();
        // The tool result fed back to the model is the real file content.
        match &run.messages[2].content[0] {
            opencode_llm::ContentPart::ToolResult { result, .. } => {
                assert!(result.as_str().unwrap().contains("hello world"))
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }
}
