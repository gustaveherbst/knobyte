//! Individual drift checkers. Each returns the issues it found; the orchestrator in
//! `drift::checker` runs them all and scores the union.

pub mod anchor_link;
pub mod broken_link;
pub mod command;
pub mod cross_file;
pub mod dependency;
pub mod edges;
pub mod frontmatter_completeness;
pub mod grounding_shape;
pub mod index_sync;
pub mod path;
pub mod script_coverage;
pub mod stale_pattern;
pub mod staleness;
pub mod todo_fixme;
pub mod tool_config_sync;

use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use walkdir::WalkDir;

/// Roots every checker resolves against.
#[derive(Debug, Clone)]
pub struct CheckContext {
    pub project_root: PathBuf,
    pub scaffold_root: PathBuf,
}

impl CheckContext {
    pub fn new(project_root: &Path, scaffold_root: &Path) -> Self {
        Self {
            project_root: project_root.to_path_buf(),
            scaffold_root: scaffold_root.to_path_buf(),
        }
    }

    /// Project-relative path of the scaffold directory with a trailing slash (`.knobyte/`),
    /// or `None` when the scaffold is the project root or lives outside it.
    pub fn scaffold_prefix(&self) -> Option<String> {
        let rel = self.scaffold_root.strip_prefix(&self.project_root).ok()?;
        let s = rel.to_string_lossy().replace('\\', "/");
        if s.is_empty() {
            None
        } else {
            Some(format!("{}/", s.trim_end_matches('/')))
        }
    }

    pub fn scaffold_is_project(&self) -> bool {
        self.scaffold_root == self.project_root
    }

    pub fn rel(&self, path: &Path) -> String {
        crate::drift::types::project_relative(&self.project_root, path)
    }

    /// Resolve a scaffold-level directory or file, preferring the scaffold root and falling
    /// back to the project root (scaffold-as-repository layouts).
    pub fn scaffold_or_project(&self, name: &str) -> PathBuf {
        let in_scaffold = self.scaffold_root.join(name);
        if in_scaffold.exists() {
            in_scaffold
        } else {
            self.project_root.join(name)
        }
    }
}

/// Read a JSON file, `None` when missing or malformed.
pub(crate) fn read_json(path: &Path) -> Option<serde_json::Value> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// One filesystem entry of a bounded project walk.
#[derive(Debug, Clone)]
pub(crate) struct IndexedEntry {
    /// Forward-slash path relative to the walk root.
    pub rel: String,
    /// Number of path components.
    pub depth: usize,
    pub is_dir: bool,
}

/// Walk `root` to `max_depth`, skipping the given directory names anywhere in the tree.
pub(crate) fn walk_index(root: &Path, max_depth: usize, skip_dirs: &[&str]) -> Vec<IndexedEntry> {
    let mut out = Vec::new();
    let walker = WalkDir::new(root)
        .min_depth(1)
        .max_depth(max_depth)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            !(e.file_type().is_dir()
                && skip_dirs.contains(&e.file_name().to_string_lossy().as_ref()))
        });
    for entry in walker.filter_map(|e| e.ok()) {
        let rel = match entry.path().strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        out.push(IndexedEntry {
            depth: entry.depth(),
            rel,
            is_dir: entry.file_type().is_dir(),
        });
    }
    out
}

/// Translate a simple glob (`*`, `**`, `?`) into an anchored regex over forward-slash paths.
pub(crate) fn glob_regex(pattern: &str) -> Option<Regex> {
    let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
    let mut re = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                if chars.get(i + 2) == Some(&'/') {
                    re.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
                continue;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    Regex::new(&re).ok()
}

/// Markdown files directly inside `dir` (non-recursive), sorted by name.
pub(crate) fn md_files_in(dir: &Path) -> Vec<String> {
    let mut out: Vec<String> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| n.ends_with(".md"))
            .collect(),
        Err(_) => Vec::new(),
    };
    out.sort();
    out
}

/// Remove `<!-- ... -->` comments (multi-line) from `content`.
pub(crate) fn strip_html_comments(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_translation() {
        let re = glob_regex("packages/*").unwrap();
        assert!(re.is_match("packages/ui"));
        assert!(!re.is_match("packages/ui/x"));
        let re = glob_regex("**/foo.rs").unwrap();
        assert!(re.is_match("foo.rs"));
        assert!(re.is_match("a/b/foo.rs"));
    }

    #[test]
    fn strips_comments() {
        assert_eq!(strip_html_comments("a<!-- x\ny -->b<!-- c"), "ab");
    }
}
