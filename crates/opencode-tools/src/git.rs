//! Git operations via the `git` binary.
//!
//! Faithful to `packages/core/src/git.ts`, which shells out to `git` (rather than using a Rust git
//! library). Reusing the binary keeps behavior identical (porcelain parsing, toplevel detection)
//! and avoids a heavy dependency; commands run through [`crate::process::run_command`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process::{run_command, CommandOutput};
use crate::ToolError;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// Run a `git` subcommand in `cwd`, capturing output.
pub async fn git(cwd: &Path, args: &[&str]) -> Result<CommandOutput, ToolError> {
    run_command("git", args, Some(cwd), GIT_TIMEOUT).await
}

/// Repository root (`git rev-parse --show-toplevel`), or `None` if `cwd` is not in a repo.
pub async fn root(cwd: &Path) -> Result<Option<PathBuf>, ToolError> {
    let out = git(cwd, &["rev-parse", "--show-toplevel"]).await?;
    Ok(out.success().then(|| PathBuf::from(out.stdout.trim())))
}

/// Current branch name (`git rev-parse --abbrev-ref HEAD`), or `None` (e.g. detached/unborn).
pub async fn current_branch(cwd: &Path) -> Result<Option<String>, ToolError> {
    let out = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).await?;
    Ok(out.success().then(|| out.stdout.trim().to_string()))
}

/// A `git status --porcelain` entry: the two-char XY status code and the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// Porcelain status code, trimmed (e.g. `??`, `M`, `A`).
    pub status: String,
    /// Path relative to the repo root.
    pub path: String,
}

/// Working-tree status (`git status --porcelain`).
pub async fn status(cwd: &Path) -> Result<Vec<StatusEntry>, ToolError> {
    let out = git(cwd, &["status", "--porcelain"]).await?;
    if !out.success() {
        return Err(ToolError::Failure(format!(
            "git status failed: {}",
            out.stderr.trim()
        )));
    }
    let mut entries = Vec::new();
    for line in out.stdout.lines() {
        if line.len() < 4 {
            continue;
        }
        let (code, path) = line.split_at(2);
        entries.push(StatusEntry {
            status: code.trim().to_string(),
            path: path.trim().to_string(),
        });
    }
    Ok(entries)
}

/// Whether the working tree is clean (no porcelain entries).
pub async fn is_clean(cwd: &Path) -> Result<bool, ToolError> {
    Ok(status(cwd).await?.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn init_repo(p: &Path) {
        git(p, &["init", "-q", "-b", "main"]).await.unwrap();
        git(p, &["config", "user.email", "t@example.com"])
            .await
            .unwrap();
        git(p, &["config", "user.name", "tester"]).await.unwrap();
        // Disable commit signing for the test repo (CI/dev may set commit.gpgsign globally).
        git(p, &["config", "commit.gpgsign", "false"])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn repo_root_status_and_branch_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        init_repo(p).await;

        assert!(is_clean(p).await.unwrap());

        crate::files::write_file(p.join("a.txt"), "hi").unwrap();
        let st = status(p).await.unwrap();
        assert_eq!(
            st,
            vec![StatusEntry {
                status: "??".to_string(),
                path: "a.txt".to_string()
            }]
        );
        assert!(!is_clean(p).await.unwrap());

        git(p, &["add", "-A"]).await.unwrap();
        let commit = git(p, &["commit", "-q", "-m", "init"]).await.unwrap();
        assert!(commit.success(), "commit failed: {}", commit.stderr);
        assert!(is_clean(p).await.unwrap());
        assert_eq!(current_branch(p).await.unwrap().as_deref(), Some("main"));

        let root = root(p).await.unwrap().unwrap();
        assert_eq!(
            std::fs::canonicalize(root).unwrap(),
            std::fs::canonicalize(p).unwrap()
        );
    }

    #[tokio::test]
    async fn non_repo_has_no_root() {
        let dir = tempfile::tempdir().unwrap();
        assert!(root(dir.path()).await.unwrap().is_none());
    }
}
