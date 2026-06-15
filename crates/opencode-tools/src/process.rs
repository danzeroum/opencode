//! Async process execution — the foundation for the `bash` tool.
//!
//! Spawns a child with piped stdio, captures stdout/stderr, and bounds execution with a timeout.
//! `kill_on_drop` ensures a timed-out child is terminated (the future owning the child is dropped
//! on timeout). Replaces the TS `cross-spawn` / `cross-spawn-spawner.ts` machinery (~508 lines)
//! with `tokio::process`. Streaming output and PTY support arrive in later increments.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use crate::ToolError;

/// Captured result of running a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Process exit code, or `None` if it was killed by a signal or timed out.
    pub code: Option<i32>,
    /// Captured standard output (lossy UTF-8).
    pub stdout: String,
    /// Captured standard error (lossy UTF-8).
    pub stderr: String,
    /// Whether the command exceeded its timeout (and was killed).
    pub timed_out: bool,
}

impl CommandOutput {
    /// Whether the process exited successfully (code 0) and did not time out.
    pub fn success(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }
}

/// Run `program` with `args` (in optional `cwd`), capturing output and bounding by `timeout`.
///
/// On timeout the child is killed (via `kill_on_drop`) and `timed_out` is set.
pub async fn run_command(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<CommandOutput, ToolError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let child = cmd.spawn()?;
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => {
            let output = result?;
            Ok(CommandOutput {
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                timed_out: false,
            })
        }
        // Timeout: the `wait_with_output` future (owning the child) is dropped here, which kills it.
        Err(_) => Ok(CommandOutput {
            code: None,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
        }),
    }
}

/// Options for [`run_shell`].
#[derive(Debug, Clone)]
pub struct ShellOptions {
    /// Working directory for the command.
    pub cwd: Option<PathBuf>,
    /// Timeout after which the command is killed.
    pub timeout: Duration,
    /// Maximum bytes retained per stream; longer output is truncated with a marker (mirrors the
    /// bash tool's output cap).
    pub max_output_bytes: usize,
}

impl Default for ShellOptions {
    fn default() -> Self {
        Self {
            cwd: None,
            timeout: Duration::from_secs(120),
            max_output_bytes: 64 * 1024,
        }
    }
}

/// Run `script` through the platform shell (`sh -c` on unix, `cmd /C` on Windows), capturing output
/// truncated to `max_output_bytes`. Foundation for the `bash` tool (tree-sitter command-approval is
/// a separate, deferred concern — it is only a TODO in `bash.ts` too).
pub async fn run_shell(script: &str, opts: &ShellOptions) -> Result<CommandOutput, ToolError> {
    #[cfg(windows)]
    let (program, flag) = ("cmd", "/C");
    #[cfg(not(windows))]
    let (program, flag) = ("sh", "-c");

    let mut out = run_command(program, &[flag, script], opts.cwd.as_deref(), opts.timeout).await?;
    truncate_utf8(&mut out.stdout, opts.max_output_bytes);
    truncate_utf8(&mut out.stderr, opts.max_output_bytes);
    Ok(out)
}

/// Truncate `s` to at most `max_bytes` on a char boundary, appending a marker if anything was cut.
fn truncate_utf8(s: &mut String, max_bytes: usize) {
    if s.len() <= max_bytes {
        return;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    s.push_str("\n… [output truncated]");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_stdout_stderr_and_exit_code() {
        let out = run_command(
            "sh",
            &["-c", "printf hello; printf oops 1>&2; exit 3"],
            None,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(out.stdout, "hello");
        assert_eq!(out.stderr, "oops");
        assert_eq!(out.code, Some(3));
        assert!(!out.timed_out);
        assert!(!out.success());
    }

    #[tokio::test]
    async fn reports_success() {
        let out = run_command("sh", &["-c", "exit 0"], None, Duration::from_secs(10))
            .await
            .unwrap();
        assert!(out.success());
    }

    #[tokio::test]
    async fn times_out_long_running_command() {
        let out = run_command("sh", &["-c", "sleep 10"], None, Duration::from_millis(150))
            .await
            .unwrap();
        assert!(out.timed_out);
        assert_eq!(out.code, None);
    }

    #[tokio::test]
    async fn run_shell_executes_and_captures() {
        let out = run_shell("echo hello && echo oops 1>&2", &ShellOptions::default())
            .await
            .unwrap();
        assert!(out.stdout.contains("hello"));
        assert!(out.stderr.contains("oops"));
        assert!(out.success());
    }

    #[tokio::test]
    async fn run_shell_truncates_long_output() {
        let opts = ShellOptions {
            max_output_bytes: 100,
            ..Default::default()
        };
        let out = run_shell("for i in $(seq 1 1000); do echo line$i; done", &opts)
            .await
            .unwrap();
        assert!(out.stdout.contains("[output truncated]"));
        assert!(out.stdout.len() < 160, "len was {}", out.stdout.len());
    }
}
