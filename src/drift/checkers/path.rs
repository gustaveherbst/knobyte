//! MISSING_PATH: every path a scaffold file names must exist.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use regex::Regex;

use super::{glob_regex, read_json, walk_index, CheckContext, IndexedEntry};
use crate::drift::types::{codes, Claim, ClaimKind, DriftIssue, SEVERITY_ERROR, SEVERITY_WARNING};

fn placeholder_words() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(?:^|[/_-])(?:new|example|your|sample|my|foo|bar|placeholder|template)(?:[/_.-]|$)")
            .unwrap()
    })
}

fn naming_convention() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^(?:PascalCase|camelCase|kebab-case|snake_case|SCREAMING_SNAKE_CASE)\.")
            .unwrap()
    })
}

fn scoped_package() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^@([\w-]+)/([\w-]+)(/.*)?$").unwrap())
}

fn url_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:https?|ftp|file)://|^//").unwrap())
}

fn file_extension() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\.[A-Za-z0-9]+$").unwrap())
}

/// Directories never searched when looking for a file by name.
const SEARCH_SKIP: &[&str] = &["node_modules", "dist", ".git", "target"];

/// Lazily built index of the project tree, shared by every claim of one run.
struct ProjectIndex<'a> {
    ctx: &'a CheckContext,
    entries: Option<Vec<IndexedEntry>>,
    scaffold_entries: Option<Vec<IndexedEntry>>,
}

impl<'a> ProjectIndex<'a> {
    fn new(ctx: &'a CheckContext) -> Self {
        Self {
            ctx,
            entries: None,
            scaffold_entries: None,
        }
    }

    fn entries(&mut self) -> &[IndexedEntry] {
        let root = &self.ctx.project_root;
        self.entries
            .get_or_insert_with(|| walk_index(root, 6, SEARCH_SKIP))
    }

    fn scaffold_entries(&mut self) -> &[IndexedEntry] {
        let root = &self.ctx.scaffold_root;
        self.scaffold_entries
            .get_or_insert_with(|| walk_index(root, 5, &["node_modules"]))
    }
}

/// Check that every claimed path exists on disk.
pub fn check_paths(claims: &[Claim], ctx: &CheckContext) -> Vec<DriftIssue> {
    let path_claims: Vec<&Claim> = claims
        .iter()
        .filter(|c| c.kind == ClaimKind::Path && !c.negated)
        .collect();
    if path_claims.is_empty() {
        return Vec::new();
    }

    let workspace_names = collect_workspace_names(ctx);
    let ignored = collect_ignored_paths(
        &path_claims
            .iter()
            .map(|c| c.value.as_str())
            .collect::<Vec<_>>(),
        ctx,
    );
    let mut index = ProjectIndex::new(ctx);
    let mut issues = Vec::new();

    for claim in path_claims {
        let value = claim.value.as_str();
        if url_pattern().is_match(value) || naming_convention().is_match(value) {
            continue;
        }
        if is_unrooted_reference(value, ctx) {
            continue;
        }
        if path_exists(value, ctx, &workspace_names, &mut index) {
            continue;
        }
        if ignored.contains(value) {
            continue;
        }
        let is_pattern = claim.source.contains("patterns/");
        let is_placeholder = placeholder_words().is_match(value);
        let severity = if is_pattern || is_placeholder {
            SEVERITY_WARNING
        } else {
            SEVERITY_ERROR
        };
        issues.push(DriftIssue::from_claim(
            codes::MISSING_PATH,
            severity,
            claim,
            format!("Referenced path does not exist: {}", value),
        ));
    }
    issues
}

/// A value with no file type whose first segment exists at neither root reads like an API
/// route or a placeholder rather than a path.
fn is_unrooted_reference(value: &str, ctx: &CheckContext) -> bool {
    if value.starts_with('/') {
        return false;
    }
    let trimmed = value.trim_end_matches('/');
    let is_dir_ref = trimmed.len() != value.len();
    if !trimmed.contains('/') && !is_dir_ref {
        return false;
    }
    if file_extension().is_match(trimmed) {
        return false;
    }
    let first = trimmed.split('/').next().unwrap_or("");
    if first.is_empty() || first.starts_with('@') || first == "." || first == ".." {
        return false;
    }
    if ctx.project_root.join(first).exists() {
        return false;
    }
    if !ctx.scaffold_is_project() && ctx.scaffold_root.join(first).exists() {
        return false;
    }
    true
}

/// Paths git reports as ignored (runtime state that is absent from a clean checkout).
fn collect_ignored_paths(values: &[&str], ctx: &CheckContext) -> HashSet<String> {
    let mut ignored = HashSet::new();
    let mut candidates: Vec<&str> = values.iter().copied().filter(|v| !v.is_empty()).collect();
    candidates.sort();
    candidates.dedup();
    if candidates.is_empty() {
        return ignored;
    }
    let child = Command::new("git")
        .args(["check-ignore", "--stdin"])
        .current_dir(&ctx.project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return ignored;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(candidates.join("\n").as_bytes());
    }
    if let Ok(out) = child.wait_with_output() {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let t = line.trim();
            if !t.is_empty() {
                ignored.insert(t.to_string());
            }
        }
    }
    ignored
}

/// `name` of each workspace package (npm/yarn/bun `workspaces`, else `pnpm-workspace.yaml`).
fn collect_workspace_names(ctx: &CheckContext) -> HashSet<String> {
    let mut names = HashSet::new();
    let patterns = collect_workspace_patterns(ctx);
    if patterns.is_empty() {
        return names;
    }
    let dirs = walk_index(&ctx.project_root, 4, &["node_modules", ".git"]);
    for pattern in patterns {
        let Some(re) = glob_regex(&pattern) else {
            continue;
        };
        for dir in dirs.iter().filter(|d| d.is_dir && re.is_match(&d.rel)) {
            let pkg = ctx.project_root.join(&dir.rel).join("package.json");
            if let Some(name) = read_json(&pkg)
                .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
            {
                names.insert(name);
            }
        }
    }
    names
}

fn collect_workspace_patterns(ctx: &CheckContext) -> Vec<String> {
    if let Some(pkg) = read_json(&ctx.project_root.join("package.json")) {
        let ws = pkg.get("workspaces");
        let list = ws.and_then(|w| w.as_array()).or_else(|| {
            ws.and_then(|w| w.get("packages"))
                .and_then(|p| p.as_array())
        });
        if let Some(list) = list {
            let patterns: Vec<String> = list
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if !patterns.is_empty() {
                return patterns;
            }
        }
    }
    let pnpm = ctx.project_root.join("pnpm-workspace.yaml");
    let Ok(content) = fs::read_to_string(pnpm) else {
        return Vec::new();
    };
    serde_yaml::from_str::<serde_yaml::Value>(&content)
        .ok()
        .and_then(|doc| {
            doc.get("packages")
                .and_then(|p| p.as_sequence())
                .map(|seq| {
                    seq.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
        })
        .unwrap_or_default()
}

fn path_exists(
    value: &str,
    ctx: &CheckContext,
    workspace_names: &HashSet<String>,
    index: &mut ProjectIndex,
) -> bool {
    if ctx.project_root.join(value).exists() {
        return true;
    }
    if !ctx.scaffold_is_project() && ctx.scaffold_root.join(value).exists() {
        return true;
    }
    // `.knobyte/x` when this repository is itself the scaffold.
    if let Some(prefix) = ctx.scaffold_prefix() {
        if let Some(rest) = value.strip_prefix(prefix.as_str()) {
            if ctx.project_root.join(rest).exists() {
                return true;
            }
        }
    }

    if let Some(caps) = scoped_package().captures(value) {
        let pkg = format!("@{}/{}", &caps[1], &caps[2]);
        if ctx
            .project_root
            .join("node_modules")
            .join(&pkg)
            .join("package.json")
            .exists()
        {
            return true;
        }
        if workspace_names.contains(&pkg) {
            return true;
        }
    }

    // Bare filenames: search the project (and the scaffold) recursively.
    if !value.contains('/') {
        let scaffold_prefix = ctx.scaffold_prefix();
        let found = index.entries().iter().any(|e| {
            e.depth <= 5
                && e.rel.rsplit('/').next() == Some(value)
                && scaffold_prefix
                    .as_deref()
                    .map(|p| !e.rel.starts_with(p))
                    .unwrap_or(true)
        });
        if found {
            return true;
        }
        if !ctx.scaffold_is_project()
            && ctx.scaffold_root.exists()
            && index
                .scaffold_entries()
                .iter()
                .any(|e| e.rel.rsplit('/').next() == Some(value))
        {
            return true;
        }
    }

    // Subproject-relative paths: accept when exactly that suffix exists somewhere.
    if value.contains('/') && !value.starts_with('/') {
        let suffix = value.trim_start_matches("./").trim_end_matches('/');
        if !suffix.is_empty() {
            let tail = format!("/{}", suffix);
            if index
                .entries()
                .iter()
                .any(|e| e.rel == suffix || e.rel.ends_with(&tail))
            {
                return true;
            }
        }
    }
    false
}
