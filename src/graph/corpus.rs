//! Corpus policy for the code graph: which repository files are indexed, which are skipped
//! (and why), and which source files no extractor handles (coverage).
//!
//! The built-in exclusions are a floor: the configured `graph.ignore` globs (see
//! [`crate::config::GraphConfig`]) are additive and can never re-include them.

use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::config::{read_graph_config_section, DEFAULT_SCAFFOLD_DIR};
use crate::graph::extractor::is_supported_path;

/// Directory names never walked, whatever the configuration says (dependency, VCS, build and
/// tool-output directories). A directory named `target` is excluded only when it is a Cargo
/// build directory (next to a `Cargo.toml`), see [`is_cargo_target_dir`].
pub const BUILTIN_EXCLUDED_DIRS: [&str; 8] =
    ["node_modules", ".git", ".knobyte", "dist", "build", "coverage", ".next", "out"];

/// Default per-file size ceiling: larger source files are skipped and reported.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Default ceiling on indexable files; a larger corpus aborts rather than indexing a part of it.
pub const DEFAULT_MAX_FILES: usize = 20_000;
/// Default ceiling on the total bytes of indexable source; a larger corpus aborts.
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// Bytes inspected for a NUL byte when deciding a "source" file is binary.
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// A `target` directory that sits next to a `Cargo.toml` (a Rust crate's build output).
pub fn is_cargo_target_dir(dir: &Path) -> bool {
    dir.file_name().is_some_and(|n| n == "target")
        && dir.parent().is_some_and(|p| p.join("Cargo.toml").is_file())
}

/// True when the first bytes of `path` contain a NUL byte (binary content with a source
/// extension, e.g. compiled output or an encrypted file).
fn looks_binary(path: &Path) -> bool {
    use std::io::Read;
    let Ok(f) = std::fs::File::open(path) else { return false };
    let mut buf = Vec::with_capacity(BINARY_SNIFF_BYTES);
    if f.take(BINARY_SNIFF_BYTES as u64).read_to_end(&mut buf).is_err() {
        return false;
    }
    buf.contains(&0)
}

const MAX_IGNORE_GLOBS: usize = 64;
const MAX_IGNORE_GLOB_LEN: usize = 256;
/// Bounded coverage reporting: most common extensions first.
const MAX_UNINDEXED_ENTRIES: usize = 24;
const MAX_UNINDEXED_FILES: usize = 20_000;

/// Source extensions recognised as code that no Knobyte extractor handles.
pub const OTHER_KNOWN_SOURCE_EXTENSIONS: [&str; 49] = [
    ".astro", ".c", ".cc", ".clj", ".cljs", ".coffee", ".cpp", ".cr", ".cxx", ".d",
    ".dart", ".elm", ".erl", ".ex", ".exs", ".f90", ".f95", ".go", ".groovy", ".h", ".hh",
    ".hpp", ".hrl", ".hs", ".hxx", ".java", ".jl", ".kt", ".kts", ".lua", ".m", ".mm", ".nim",
    ".pas", ".php", ".pl", ".pm", ".r", ".rb", ".scala", ".sol", ".svelte", ".tcl",
    ".vue", ".zig", ".sh", ".bash", ".ps1", ".fs",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CorpusPolicy {
    /// Validated, normalised additional ignore globs (sorted, de-duplicated).
    pub ignore: Vec<String>,
    pub max_file_bytes: u64,
    pub max_files: usize,
    /// Total bytes of indexable source; a larger corpus aborts rather than indexing a part.
    #[serde(default = "default_max_total_bytes")]
    pub max_total_bytes: u64,
}

fn default_max_total_bytes() -> u64 {
    DEFAULT_MAX_TOTAL_BYTES
}

impl Default for CorpusPolicy {
    fn default() -> Self {
        Self {
            ignore: Vec::new(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_files: DEFAULT_MAX_FILES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
        }
    }
}

impl CorpusPolicy {
    /// Policy configured for a project: `<root>/.knobyte/config.json` -> `graph`.
    pub fn for_project(root: &Path) -> Self {
        Self::from_scaffold(&root.join(DEFAULT_SCAFFOLD_DIR))
    }

    /// Policy configured in `<scaffold>/config.json` -> `graph`.
    pub fn from_scaffold(scaffold: &Path) -> Self {
        let mut policy = Self::default();
        if let Some(cfg) = read_graph_config_section(scaffold) {
            policy.ignore = normalize_ignore_globs(&cfg.ignore);
            if let Some(b) = cfg.max_file_bytes.filter(|b| *b > 0) {
                policy.max_file_bytes = b;
            }
            if let Some(n) = cfg.max_files.filter(|n| *n > 0) {
                policy.max_files = n;
            }
            if let Some(n) = cfg.max_total_bytes.filter(|n| *n > 0) {
                policy.max_total_bytes = n;
            }
        }
        policy
    }

    /// Stable digest of everything that decides corpus membership. A changed policy makes an
    /// existing graph stale.
    pub fn hash(&self) -> String {
        let mut h = Sha256::new();
        h.update(b"knobyte-corpus-policy-2\0");
        for d in BUILTIN_EXCLUDED_DIRS {
            h.update(d.as_bytes());
            h.update(b"\0");
        }
        h.update(b"cargo-target\0binary-skip\0");
        h.update(self.max_total_bytes.to_le_bytes());
        for g in &self.ignore {
            h.update(g.as_bytes());
            h.update(b"\0");
        }
        h.update(self.max_file_bytes.to_le_bytes());
        h.update((self.max_files as u64).to_le_bytes());
        hex::encode(h.finalize())[..16].to_string()
    }
}

/// Keep only repository-relative globs (no absolute paths, drive letters or `..`), bounded in
/// number and length.
pub fn normalize_ignore_globs(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for g in raw {
        let g = g.trim().replace('\\', "/");
        if g.is_empty() || g.len() > MAX_IGNORE_GLOB_LEN || g.starts_with('/') || g.starts_with('!') {
            continue;
        }
        let bytes = g.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
            continue;
        }
        if g.split('/').any(|seg| seg == "..") {
            continue;
        }
        if !out.contains(&g) {
            out.push(g);
        }
        if out.len() >= MAX_IGNORE_GLOBS {
            break;
        }
    }
    out.sort();
    out
}

#[derive(Debug, Clone)]
pub struct IndexableFile {
    pub path: PathBuf,
    pub rel_path: String,
    pub size: u64,
}

/// A file the corpus policy declined to index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkippedFile {
    pub path: String,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnindexedExtension {
    pub extension: String,
    pub files: usize,
}

/// Source files no extractor handles, grouped by extension.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CoverageReport {
    /// Total unsupported source files seen.
    pub unindexed_total: usize,
    /// Most common extensions first (bounded).
    pub unindexed: Vec<UnindexedExtension>,
    /// True when the count stopped at its bound.
    pub truncated: bool,
    /// Files the policy skipped (oversized, ...).
    #[serde(default)]
    pub skipped: Vec<SkippedFile>,
}

#[derive(Debug, Clone)]
pub struct CorpusScan {
    pub files: Vec<IndexableFile>,
    pub total_bytes: u64,
    pub coverage: CoverageReport,
    pub policy: CorpusPolicy,
}

#[derive(Debug, Clone)]
pub struct CorpusLimitError {
    pub limit: &'static str,
    pub observed: usize,
    pub allowed: usize,
}

impl fmt::Display for CorpusLimitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "The graph corpus exceeded the {} safety bound ({} > {}). Exclude paths with \"graph.ignore\" \
             in .knobyte/config.json or raise \"graph.{}\".",
            self.limit, self.observed, self.allowed, self.limit
        )
    }
}

impl std::error::Error for CorpusLimitError {}

fn walker(root: &Path, policy: &CorpusPolicy) -> ignore::Walk {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        .parents(false)
        .git_ignore(true)
        .filter_entry(|entry| {
            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            if !is_dir || entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            !BUILTIN_EXCLUDED_DIRS.contains(&name.as_ref()) && !is_cargo_target_dir(entry.path())
        });
    if !policy.ignore.is_empty() {
        let mut ov = OverrideBuilder::new(root);
        for g in &policy.ignore {
            // Override globs are whitelists unless negated; negated-only means "ignore these".
            let _ = ov.add(&format!("!{}", g));
            if !g.contains('/') {
                let _ = ov.add(&format!("!**/{}", g));
            }
        }
        if let Ok(ov) = ov.build() {
            builder.overrides(ov);
        }
    }
    builder.build()
}

fn extension_of(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let dot = name.rfind('.')?;
    if dot == 0 || dot == name.len() - 1 {
        return None;
    }
    Some(name[dot..].to_lowercase())
}

/// Walk the repository under a corpus policy.
pub fn scan_corpus(root: &Path, policy: &CorpusPolicy) -> Result<CorpusScan, CorpusLimitError> {
    let mut files = Vec::new();
    let mut total_bytes = 0u64;
    let mut skipped = Vec::new();
    let mut unindexed: BTreeMap<String, usize> = BTreeMap::new();
    let mut unindexed_total = 0usize;
    let mut truncated = false;

    for entry in walker(root, policy).flatten() {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let rel_path = match path.strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => path.to_string_lossy().to_string(),
        };
        if !is_supported_path(path) {
            if let Some(ext) = extension_of(path) {
                if OTHER_KNOWN_SOURCE_EXTENSIONS.contains(&ext.as_str()) {
                    if unindexed_total >= MAX_UNINDEXED_FILES {
                        truncated = true;
                    } else {
                        unindexed_total += 1;
                        *unindexed.entry(ext).or_default() += 1;
                    }
                }
            }
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        if size > policy.max_file_bytes {
            skipped.push(SkippedFile {
                path: rel_path,
                code: "GRAPH_SOURCE_TOO_LARGE".to_string(),
                message: format!(
                    "{} bytes exceeds the {}-byte per-file limit",
                    size, policy.max_file_bytes
                ),
            });
            continue;
        }
        if looks_binary(path) {
            // Binary content under a source extension cannot be parsed; skip and report it
            // rather than recording a failed parse that degrades the whole graph.
            skipped.push(SkippedFile {
                path: rel_path,
                code: "GRAPH_SOURCE_BINARY".to_string(),
                message: "contains NUL bytes (binary content); not indexed".to_string(),
            });
            continue;
        }
        total_bytes += size;
        files.push(IndexableFile {
            path: path.to_path_buf(),
            rel_path,
            size,
        });
        if files.len() > policy.max_files {
            return Err(CorpusLimitError {
                limit: "max_files",
                observed: files.len(),
                allowed: policy.max_files,
            });
        }
        if total_bytes > policy.max_total_bytes {
            return Err(CorpusLimitError {
                limit: "max_total_bytes",
                observed: total_bytes as usize,
                allowed: policy.max_total_bytes as usize,
            });
        }
    }
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    skipped.sort_by(|a, b| a.path.cmp(&b.path));

    let mut entries: Vec<UnindexedExtension> = unindexed
        .into_iter()
        .map(|(extension, files)| UnindexedExtension { extension, files })
        .collect();
    entries.sort_by(|a, b| b.files.cmp(&a.files).then(a.extension.cmp(&b.extension)));
    entries.truncate(MAX_UNINDEXED_ENTRIES);

    Ok(CorpusScan {
        files,
        total_bytes,
        coverage: CoverageReport {
            unindexed_total,
            unindexed: entries,
            truncated,
            skipped,
        },
        policy: policy.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_globs_are_repository_relative() {
        let globs = normalize_ignore_globs(&[
            "gen/**".into(),
            "/abs/**".into(),
            "C:/x/**".into(),
            "a/../b".into(),
            "!keep".into(),
            " gen/** ".into(),
        ]);
        assert_eq!(globs, vec!["gen/**".to_string()]);
    }
}
