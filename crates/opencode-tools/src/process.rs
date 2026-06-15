//! Async process execution — the foundation for the `bash` tool.
//!
//! Spawns a child with piped stdio, captures stdout/stderr, and bounds execution with a timeout.
//! `kill_on_drop` ensures a timed-out child is terminated (the future owning the child is dropped
//! on timeout). Replaces the TS `cross-spawn` / `cross-spawn-spawner.ts` machinery (~508 lines)
//! with `tokio::process`. Streaming output and PTY support arrive in later increments.

use std::path::Path;
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
}
