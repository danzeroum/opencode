//! Tools and system helpers (Phase 1): the ~18 tools (bash, read, edit, glob, grep, git, skill, …)
//! plus filesystem / git / search / PTY / process helpers.
//!
//! Search is built on the **ripgrep libraries** (`ignore`, `grep`, `globset`) so ignore/match
//! semantics are identical to the current `ripgrep.ts` / glob stack. `gix` (git), `portable-pty`
//! (PTY) and `tokio::process` (spawning) land in later increments. NB: **tree-sitter is out of
//! scope** for the backend.

use std::path::{Path, PathBuf};

use grep::regex::RegexMatcher;
use grep::searcher::sinks::UTF8;
use grep::searcher::Searcher;

pub mod files;
pub mod git;
pub mod process;

/// Errors produced while executing a tool.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The tool ran but failed (non-zero exit, validation failure, etc.).
    #[error("tool failure: {0}")]
    Failure(String),
    /// The tool was denied by the permission system.
    #[error("permission denied: {0}")]
    Denied(String),
    /// An invalid pattern (glob or regex) was supplied.
    #[error("invalid pattern: {0}")]
    Pattern(String),
    /// Underlying filesystem error.
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Read a UTF-8 file, optionally restricted to a half-open line range `[offset, offset+limit)`
/// (0-based lines). Mirrors the `read` tool's offset/limit behavior.
pub fn read_file(
    path: impl AsRef<Path>,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String, ToolError> {
    let content = std::fs::read_to_string(path)?;
    if offset.is_none() && limit.is_none() {
        return Ok(content);
    }
    let lines: Vec<&str> = content.lines().collect();
    let start = offset.unwrap_or(0).min(lines.len());
    let end = match limit {
        Some(l) => start.saturating_add(l).min(lines.len()),
        None => lines.len(),
    };
    Ok(lines[start..end].join("\n"))
}

/// Find files under `root` matching a glob `pattern`, honoring `.gitignore` (via `ignore`).
/// Patterns are matched against paths relative to `root`. Results are sorted.
pub fn glob(pattern: &str, root: impl AsRef<Path>) -> Result<Vec<PathBuf>, ToolError> {
    let root = root.as_ref();
    let matcher = globset::Glob::new(pattern)
        .map_err(|e| ToolError::Pattern(e.to_string()))?
        .compile_matcher();

    let mut out = Vec::new();
    for entry in ignore::Walk::new(root).flatten() {
        if entry.file_type().is_none_or(|t| !t.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(path);
        if matcher.is_match(rel) {
            out.push(path.to_path_buf());
        }
    }
    out.sort();
    Ok(out)
}

/// A single grep match: the file, 1-based line number, and the (trimmed) matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepMatch {
    /// File containing the match.
    pub path: PathBuf,
    /// 1-based line number.
    pub line_number: u64,
    /// The matching line, with the trailing newline removed.
    pub line: String,
}

/// Search files under `root` for `pattern` (a regular expression), honoring `.gitignore`.
/// Uses ripgrep's `grep` searcher for the same matching behavior as the `grep` tool.
pub fn grep(pattern: &str, root: impl AsRef<Path>) -> Result<Vec<GrepMatch>, ToolError> {
    let matcher = RegexMatcher::new(pattern).map_err(|e| ToolError::Pattern(e.to_string()))?;
    let mut results = Vec::new();

    for entry in ignore::Walk::new(root.as_ref()).flatten() {
        if entry.file_type().is_none_or(|t| !t.is_file()) {
            continue;
        }
        let path = entry.path().to_path_buf();
        let mut searcher = Searcher::new();
        // Per-file search errors (e.g. binary/non-UTF8) are skipped rather than failing the run.
        let _ = searcher.search_path(
            &matcher,
            &path,
            UTF8(|line_number, line| {
                results.push(GrepMatch {
                    path: path.clone(),
                    line_number,
                    line: line.trim_end().to_string(),
                });
                Ok(true)
            }),
        );
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn main() {}\n// TODO: x\n").unwrap();
        fs::write(dir.path().join("b.txt"), "hello\nworld\nhello world\n").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/c.rs"), "pub fn c() {}\n").unwrap();
        dir
    }

    #[test]
    fn read_file_honors_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        fs::write(&p, "l0\nl1\nl2\nl3\nl4\n").unwrap();
        assert_eq!(read_file(&p, Some(1), Some(2)).unwrap(), "l1\nl2");
        assert_eq!(read_file(&p, Some(3), None).unwrap(), "l3\nl4");
        assert_eq!(read_file(&p, Some(99), Some(5)).unwrap(), "");
    }

    #[test]
    fn glob_matches_recursively_and_sorts() {
        let dir = fixture();
        let found = glob("**/*.rs", dir.path()).unwrap();
        let rels: Vec<_> = found
            .iter()
            .map(|p| {
                p.strip_prefix(dir.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(rels, vec!["a.rs", "sub/c.rs"]);
    }

    #[test]
    fn grep_finds_matches_with_line_numbers() {
        let dir = fixture();
        let mut hits = grep(r"world", dir.path()).unwrap();
        hits.sort_by_key(|m| m.line_number);
        let lines: Vec<_> = hits
            .iter()
            .map(|m| (m.line_number, m.line.clone()))
            .collect();
        assert_eq!(
            lines,
            vec![(2, "world".to_string()), (3, "hello world".to_string())]
        );
    }

    #[test]
    fn invalid_glob_is_pattern_error() {
        let err = glob("[", ".").unwrap_err();
        assert!(matches!(err, ToolError::Pattern(_)));
    }
}
