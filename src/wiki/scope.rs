//! Which Markdown files make up the wiki corpus, which of them operations may write, and the
//! safety bounds on the corpus.

use std::fs;
use std::path::{Component, Path, PathBuf};

use regex::Regex;
use walkdir::WalkDir;

use crate::config::{WikiScopeConfig, WIKI_TEAM_OWNED_READ_ONLY};
use crate::wiki::diagnostics::diag;
use crate::wiki::models::{EntityTypeRegistry, WikiDiagnostic};

/// Bumped when parsing or index-time validation changes meaningfully, so existing indexes
/// re-read their corpus on the next refresh.
pub const WIKI_PARSER_REVISION: u32 = 2;

/// Corpus safety bounds.
pub const MAX_MARKDOWN_FILES: usize = 10_000;
pub const MAX_DIRECTORY_DEPTH: usize = 64;
pub const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_CORPUS_BYTES: u64 = 256 * 1024 * 1024;
/// Directory entries (files and directories, Markdown or not) one discovery walk may visit.
pub const MAX_DIRECTORY_ENTRIES: usize = 100_000;

/// Translate a glob (`*`, `**`, `?`) into an anchored regex over POSIX paths.
pub fn glob_to_regex(pattern: &str) -> Regex {
    let chars: Vec<char> = pattern.chars().collect();
    let mut src = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '*' {
            if chars.get(i + 1) == Some(&'*') {
                i += 1;
                if chars.get(i + 1) == Some(&'/') {
                    i += 1;
                    src.push_str("(?:[^/]+/)*");
                } else {
                    src.push_str(".*");
                }
            } else {
                src.push_str("[^/]*");
            }
        } else if c == '?' {
            src.push_str("[^/]");
        } else {
            src.push_str(&regex::escape(&c.to_string()));
        }
        i += 1;
    }
    src.push('$');
    Regex::new(&src).unwrap_or_else(|_| Regex::new("^$").expect("static regex"))
}

pub fn matches_any_glob(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| glob_to_regex(p).is_match(path))
}

/// Result of a corpus walk.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    /// `(scaffold-relative path, absolute path)`, sorted by path.
    pub files: Vec<(String, PathBuf)>,
    pub diagnostics: Vec<WikiDiagnostic>,
    /// The walk stopped at (or skipped a file over) a corpus safety bound, so `files` is not
    /// the whole corpus.
    pub limit_exceeded: bool,
}

/// The resolved wiki scope of one scaffold.
#[derive(Debug, Clone)]
pub struct WikiScope {
    pub scaffold_root: PathBuf,
    pub config: WikiScopeConfig,
    pub registry: EntityTypeRegistry,
}

impl WikiScope {
    pub fn load(scaffold_root: &Path) -> Self {
        Self::with_config(scaffold_root, WikiScopeConfig::load(scaffold_root))
    }

    pub fn with_config(scaffold_root: &Path, config: WikiScopeConfig) -> Self {
        let registry = EntityTypeRegistry::with_additional(&config.entity_types);
        Self {
            scaffold_root: scaffold_root.to_path_buf(),
            config,
            registry,
        }
    }

    /// Digest of the configuration that shapes what the index holds: the registered entity
    /// types (parse and validation), the exclusion globs (the corpus) and the parser revision.
    /// Part of the refresh key, so a config change re-evaluates every file even when no
    /// Markdown changed.
    pub fn config_digest(&self) -> String {
        let mut types = self.config.entity_types.clone();
        types.sort();
        types.dedup();
        let mut exclude = self.config.exclude.clone();
        exclude.sort();
        let payload = serde_json::json!({
            "parser": WIKI_PARSER_REVISION,
            "entityTypes": types,
            "exclude": exclude,
        });
        crate::graph::fingerprint::compute_file_hash(payload.to_string().as_bytes())
    }

    pub fn is_excluded(&self, rel: &str) -> bool {
        is_always_skipped(rel) || matches_any_glob(rel, &self.config.exclude)
    }

    /// True when operations must not write `rel` (team-owned or `wiki.readOnly`).
    pub fn is_read_only(&self, rel: &str) -> bool {
        let rel = rel.replace('\\', "/");
        let team: Vec<String> = WIKI_TEAM_OWNED_READ_ONLY
            .iter()
            .map(|s| s.to_string())
            .collect();
        matches_any_glob(&rel, &team) || matches_any_glob(&rel, &self.config.read_only)
    }

    /// Discover corpus files, sorted by path. Over-limit conditions are reported as
    /// `WIKI_CORPUS_LIMIT_EXCEEDED` diagnostics and the walk stops there.
    pub fn discover(&self) -> (Vec<(String, PathBuf)>, Vec<WikiDiagnostic>) {
        let d = self.discover_checked();
        (d.files, d.diagnostics)
    }

    /// Discover corpus files, sorted by path, with an explicit flag for "the walk hit a
    /// safety bound". A bounded discovery is incomplete: callers that mirror the corpus (the
    /// index refresh) must not treat unreached files as deleted.
    ///
    /// Symlinked Markdown files are followed when they resolve inside the scaffold; one that
    /// escapes the scaffold (or is broken) is reported and skipped, never silently dropped.
    pub fn discover_checked(&self) -> Discovery {
        let mut files = Vec::new();
        let mut diags = Vec::new();
        let mut limit_exceeded = false;
        if !self.scaffold_root.exists() {
            return Discovery {
                files,
                diagnostics: diags,
                limit_exceeded,
            };
        }
        let real_root = fs::canonicalize(&self.scaffold_root)
            .unwrap_or_else(|_| self.scaffold_root.clone());
        let mut corpus_bytes: u64 = 0;
        let walker = WalkDir::new(&self.scaffold_root)
            .max_depth(MAX_DIRECTORY_DEPTH)
            .sort_by_file_name()
            .into_iter()
            .filter_entry(|e| {
                // Prune dot-directories and `local/` early.
                let name = e.file_name().to_string_lossy();
                e.depth() == 0 || !(name.starts_with('.') || (e.depth() == 1 && name == "local"))
            });
        let mut visited = 0usize;
        for entry in walker.filter_map(|e| e.ok()) {
            visited += 1;
            if visited > MAX_DIRECTORY_ENTRIES {
                limit_exceeded = true;
                diags.push(diag(
                    "WIKI_CORPUS_LIMIT_EXCEEDED",
                    format!(
                        "The scaffold has more than {} directory entries; discovery stopped at {}",
                        MAX_DIRECTORY_ENTRIES,
                        entry.path().display()
                    ),
                    "",
                ));
                break;
            }
            let path = entry.path();
            let is_link = entry.path_is_symlink() || entry.file_type().is_symlink();
            if !is_link && !entry.file_type().is_file() {
                continue;
            }
            let rel = match path.strip_prefix(&self.scaffold_root) {
                Ok(p) => p.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
            let is_markdown = ext.eq_ignore_ascii_case("md") || ext.eq_ignore_ascii_case("mdx");
            if !is_link && !is_markdown {
                continue;
            }
            if self.is_excluded(&rel) {
                continue;
            }
            let size = if is_link {
                // Every symlink is checked, whatever it names: a directory or non-Markdown
                // link that escapes the scaffold (or is broken) is reported too, because a
                // writer could otherwise be redirected through it. Links are never followed
                // into directories; only a contained Markdown file link is indexed.
                let target = match fs::canonicalize(path) {
                    Ok(t) => t,
                    Err(_) => {
                        let mut d = diag(
                            "WIKI_PARSE_ERROR",
                            format!("Broken symlink at {}; it is not indexed", rel),
                            rel.clone(),
                        );
                        d.severity = "warning".into();
                        diags.push(d);
                        continue;
                    }
                };
                if !target.starts_with(&real_root) || target == real_root {
                    diags.push(diag(
                        "PATH_OUTSIDE_SCAFFOLD",
                        format!(
                            "{} is a symlink to {}, which is outside the scaffold. It is not indexed.",
                            rel,
                            target.display()
                        ),
                        rel.clone(),
                    ));
                    continue;
                }
                if !is_markdown {
                    continue;
                }
                match fs::metadata(&target) {
                    Ok(m) if m.is_file() => m.len(),
                    _ => continue,
                }
            } else {
                entry.metadata().map(|m| m.len()).unwrap_or(0)
            };
            if size > MAX_FILE_BYTES {
                limit_exceeded = true;
                diags.push(diag(
                    "WIKI_CORPUS_LIMIT_EXCEEDED",
                    format!(
                        "{} is {} bytes; files over {} bytes are not indexed",
                        rel, size, MAX_FILE_BYTES
                    ),
                    rel.clone(),
                ));
                continue;
            }
            corpus_bytes += size;
            if corpus_bytes > MAX_CORPUS_BYTES {
                limit_exceeded = true;
                diags.push(diag(
                    "WIKI_CORPUS_LIMIT_EXCEEDED",
                    format!(
                        "The wiki corpus exceeds {} bytes; indexing stopped before {}",
                        MAX_CORPUS_BYTES, rel
                    ),
                    rel,
                ));
                break;
            }
            if files.len() >= MAX_MARKDOWN_FILES {
                limit_exceeded = true;
                diags.push(diag(
                    "WIKI_CORPUS_LIMIT_EXCEEDED",
                    format!(
                        "The wiki corpus has more than {} Markdown files; indexing stopped before {}",
                        MAX_MARKDOWN_FILES, rel
                    ),
                    rel,
                ));
                break;
            }
            files.push((rel, path.to_path_buf()));
        }
        Discovery {
            files,
            diagnostics: diags,
            limit_exceeded,
        }
    }

    /// Read a corpus file as text (lossy for invalid UTF-8).
    pub fn read(&self, rel: &str) -> Option<String> {
        let bytes = fs::read(self.scaffold_root.join(rel)).ok()?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

/// Paths the wiki never indexes regardless of configuration: `local/` and dot-paths.
pub fn is_always_skipped(rel: &str) -> bool {
    rel.starts_with("local/") || rel.split('/').any(|seg| seg.starts_with('.'))
}

/// A normalized scaffold-relative POSIX path to a Markdown file: no `..`, no absolute parts,
/// no backslashes, no empty segments.
pub fn is_canonical_markdown_path(rel: &str) -> bool {
    if rel.is_empty() || rel.contains('\\') || rel.starts_with('/') || rel.contains("//") {
        return false;
    }
    if rel
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return false;
    }
    if Path::new(rel)
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return false;
    }
    let lower = rel.to_ascii_lowercase();
    lower.ends_with(".md") || lower.ends_with(".mdx")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_to_regex("**/node_modules/**").is_match("a/node_modules/x.md"));
        assert!(glob_to_regex("**/node_modules/**").is_match("node_modules/x.md"));
        assert!(glob_to_regex("team/**").is_match("team/members/a.md"));
        assert!(!glob_to_regex("team/*").is_match("team/members/a.md"));
        assert!(glob_to_regex("*.md").is_match("a.md"));
        assert!(is_canonical_markdown_path("context/a.md"));
        assert!(!is_canonical_markdown_path("../a.md"));
        assert!(!is_canonical_markdown_path("context/./a.md"));
    }
}
