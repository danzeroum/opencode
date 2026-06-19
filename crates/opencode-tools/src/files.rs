//! Filesystem mutation helpers backing the `write` / `edit` / `ls` tools.

use std::path::Path;

use crate::ToolError;

/// Write `contents` to `path`, creating parent directories as needed.
pub fn write_file(path: impl AsRef<Path>, contents: &str) -> Result<(), ToolError> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(path, contents)?;
    Ok(())
}

/// Replace occurrences of `find` with `replace` in the file at `path`, returning how many were
/// replaced. Mirrors the `edit` tool: errors if `find` is absent, and (unless `replace_all`) errors
/// if `find` is not unique — preventing ambiguous edits.
pub fn edit_file(
    path: impl AsRef<Path>,
    find: &str,
    replace: &str,
    replace_all: bool,
) -> Result<usize, ToolError> {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path)?;
    let count = content.matches(find).count();
    if count == 0 {
        return Err(ToolError::Failure(format!(
            "string not found in {}",
            path.display()
        )));
    }
    if !replace_all && count > 1 {
        return Err(ToolError::Failure(format!(
            "string not unique in {} ({count} matches); pass replace_all",
            path.display()
        )));
    }
    let updated = if replace_all {
        content.replace(find, replace)
    } else {
        content.replacen(find, replace, 1)
    };
    std::fs::write(path, updated)?;
    Ok(if replace_all { count } else { 1 })
}

/// List the entries directly under `dir` (names only), sorted, with `/` appended to directories.
pub fn list_dir(dir: impl AsRef<Path>) -> Result<Vec<String>, ToolError> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        entries.push(if is_dir { format!("{name}/") } else { name });
    }
    entries.sort();
    Ok(entries)
}

/// A single immediate child of a directory, with its kind and gitignore status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntryInfo {
    /// Entry file name.
    pub name: String,
    /// Whether the entry is a directory.
    pub is_dir: bool,
    /// Whether the entry is gitignored (per the `.gitignore`/`.ignore` hierarchy).
    pub ignored: bool,
}

/// List the immediate children of `dir` with their kind + gitignore status, sorted by name. The
/// `ignored` flag uses the [`ignore`] crate's hierarchical gitignore rules (the same the search tools
/// use): a gitignored entry is still listed, just flagged.
pub fn list_dir_nodes(dir: impl AsRef<Path>) -> Result<Vec<DirEntryInfo>, ToolError> {
    let dir = dir.as_ref();
    // Immediate children that survive the gitignore filter (hidden files kept; only gitignore decides).
    let mut not_ignored = std::collections::HashSet::new();
    for result in ignore::WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(false)
        .build()
        .flatten()
    {
        if result.depth() == 1 {
            if let Some(name) = result.file_name().to_str() {
                not_ignored.insert(name.to_string());
            }
        }
    }
    let mut nodes = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let ignored = !not_ignored.contains(&name);
        nodes.push(DirEntryInfo {
            name,
            is_dir,
            ignored,
        });
    }
    nodes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a/b/c.txt");
        write_file(&p, "hi").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hi");
    }

    #[test]
    fn edit_replaces_unique_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        write_file(&p, "alpha beta gamma").unwrap();
        assert_eq!(edit_file(&p, "beta", "BETA", false).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "alpha BETA gamma");
    }

    #[test]
    fn edit_rejects_missing_and_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        write_file(&p, "x x x").unwrap();
        assert!(edit_file(&p, "y", "z", false).is_err());
        assert!(edit_file(&p, "x", "z", false).is_err()); // not unique
        assert_eq!(edit_file(&p, "x", "z", true).unwrap(), 3); // replace_all ok
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "z z z");
    }

    #[test]
    fn list_dir_sorts_and_marks_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        write_file(dir.path().join("z.txt"), "").unwrap();
        write_file(dir.path().join("a.txt"), "").unwrap();
        assert_eq!(
            list_dir(dir.path()).unwrap(),
            vec!["a.txt", "sub/", "z.txt"]
        );
    }
}
