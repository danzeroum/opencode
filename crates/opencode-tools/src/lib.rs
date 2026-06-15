//! Tools and system helpers (Phase 1): the ~18 tools (bash, read, edit, glob, grep, git, skill, …)
//! plus filesystem / git / search / PTY / process helpers.
//!
//! Search is built on the **ripgrep libraries** (`ignore`, `grep`, `globset`) so ignore/match
//! semantics are identical to the current `ripgrep.ts` / glob stack. `gix` (git), `portable-pty`
//! (PTY) and `tokio::process` (spawning) land in later increments. NB: **tree-sitter is out of
//! scope** for the backend.

use std::path::{Path, PathBuf};

use grep::matcher::Matcher;
use grep::regex::RegexMatcher;
use grep::searcher::sinks::UTF8;
use grep::searcher::{Searcher, Sink, SinkMatch};

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

/// A submatch within a line: the matched text and its byte range within the line bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubMatch {
    /// The matched text.
    pub text: String,
    /// Start byte offset within the line.
    pub start: usize,
    /// End byte offset within the line.
    pub end: usize,
}

/// A detailed grep match mirroring ripgrep's JSON match shape (backs the `find.text` route).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailedMatch {
    /// File path (relative to the search root).
    pub path: String,
    /// The matching line, trailing newline trimmed.
    pub line_text: String,
    /// 1-based line number.
    pub line_number: u64,
    /// Absolute byte offset of the line within the file.
    pub absolute_offset: u64,
    /// Submatch ranges within the line.
    pub submatches: Vec<SubMatch>,
}

struct DetailSink<'a> {
    matcher: &'a RegexMatcher,
    path: String,
    out: &'a mut Vec<DetailedMatch>,
}

impl Sink for DetailSink<'_> {
    type Error = std::io::Error;
    fn matched(
        &mut self,
        _searcher: &Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, std::io::Error> {
        let bytes = mat.bytes();
        let mut submatches = Vec::new();
        let _ = self.matcher.find_iter(bytes, |m| {
            submatches.push(SubMatch {
                text: String::from_utf8_lossy(&bytes[m.start()..m.end()]).into_owned(),
                start: m.start(),
                end: m.end(),
            });
            true
        });
        self.out.push(DetailedMatch {
            path: self.path.clone(),
            line_text: String::from_utf8_lossy(bytes).trim_end().to_string(),
            line_number: mat.line_number().unwrap_or(0),
            absolute_offset: mat.absolute_byte_offset(),
            submatches,
        });
        Ok(true)
    }
}

/// Like [`grep`], but returns detailed matches (submatch ranges + byte offsets), mirroring the
/// ripgrep JSON match shape consumed by the `find.text` route. Honors `.gitignore`.
pub fn grep_detailed(
    pattern: &str,
    root: impl AsRef<Path>,
) -> Result<Vec<DetailedMatch>, ToolError> {
    let matcher = RegexMatcher::new(pattern).map_err(|e| ToolError::Pattern(e.to_string()))?;
    let root = root.as_ref();
    let mut out = Vec::new();
    for entry in ignore::Walk::new(root).flatten() {
        if entry.file_type().is_none_or(|t| !t.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let mut searcher = Searcher::new();
        let _ = searcher.search_path(
            &matcher,
            path,
            DetailSink {
                matcher: &matcher,
                path: rel,
                out: &mut out,
            },
        );
    }
    Ok(out)
}

/// Fuzzy-ish file search backing the `find.files` route: relative paths under `root` whose path
/// contains `query` (case-insensitive substring), honoring `.gitignore`, sorted and capped at
/// `limit`. An empty `query` lists all files (up to `limit`).
pub fn find_files(root: impl AsRef<Path>, query: &str, limit: usize) -> Vec<String> {
    let root = root.as_ref();
    let needle = query.to_lowercase();
    let mut out = Vec::new();
    for entry in ignore::Walk::new(root).flatten() {
        if entry.file_type().is_none_or(|t| !t.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        if needle.is_empty() || rel.to_lowercase().contains(&needle) {
            out.push(rel);
        }
    }
    out.sort();
    out.truncate(limit);
    out
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

    #[test]
    fn find_files_substring_sorted_and_limited() {
        let dir = fixture();
        // fixture(): a.rs, b.txt, sub/c.rs
        let all = find_files(dir.path(), "", 100);
        assert_eq!(all, vec!["a.rs", "b.txt", "sub/c.rs"]);
        let rs = find_files(dir.path(), ".rs", 100);
        assert_eq!(rs, vec!["a.rs", "sub/c.rs"]);
        let limited = find_files(dir.path(), "", 2);
        assert_eq!(limited.len(), 2);
    }

    #[test]
    fn grep_detailed_reports_submatches_and_offsets() {
        let dir = fixture(); // b.txt: "hello\nworld\nhello world\n"
        let mut hits = grep_detailed("world", dir.path()).unwrap();
        hits.retain(|m| m.path == "b.txt");
        hits.sort_by_key(|m| m.line_number);
        assert_eq!(hits.len(), 2);

        assert_eq!(hits[0].line_number, 2);
        assert_eq!(hits[0].line_text, "world");
        assert_eq!(
            hits[0].submatches,
            vec![SubMatch {
                text: "world".into(),
                start: 0,
                end: 5
            }]
        );

        assert_eq!(hits[1].line_number, 3);
        assert_eq!(hits[1].line_text, "hello world");
        assert_eq!(
            hits[1].submatches,
            vec![SubMatch {
                text: "world".into(),
                start: 6,
                end: 11
            }]
        );
    }
}
