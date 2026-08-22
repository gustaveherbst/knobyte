//! Small, read-mostly git helpers for the Hub (branch / HEAD / dirty tree,
//! scoped status and diffs for the setup commit review).

use std::path::Path;
use std::process::Command;

use serde::Serialize;

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn has_git(root: &Path) -> bool {
    let g = root.join(".git");
    g.is_dir() || g.is_file()
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RepoState {
    pub available: bool,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub head_short: Option<String>,
    pub dirty: bool,
    pub changed_files: usize,
}

pub fn repo_state(root: &Path) -> RepoState {
    if !has_git(root) {
        return RepoState::default();
    }
    let branch = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "HEAD");
    let head = git(root, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let status = git(root, &["status", "--porcelain", "--untracked-files=normal"]);
    let changed = status.as_deref().map(|s| s.lines().filter(|l| !l.trim().is_empty()).count()).unwrap_or(0);
    RepoState {
        available: status.is_some(),
        head_short: head.as_ref().map(|h| h.chars().take(10).collect()),
        branch,
        head,
        dirty: changed > 0,
        changed_files: changed,
    }
}

/// Whether `path` (repository-relative) is committed in HEAD.
pub fn is_committed(root: &Path, path: &str) -> bool {
    validate_rel(path) && git(root, &["cat-file", "-e", &format!("HEAD:{}", path)]).is_some()
}

/// One changed path below the given pathspecs.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ChangedPath {
    pub path: String,
    /// `added` | `modified` | `deleted`
    pub status: String,
    pub additions: usize,
    pub deletions: usize,
    pub binary: bool,
}

fn validate_rel(p: &str) -> bool {
    !p.is_empty()
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.chars().any(|c| c.is_control())
        && p.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

/// Uncommitted changes (tracked and untracked) below `pathspecs`.
pub fn changed_paths(root: &Path, pathspecs: &[String]) -> Vec<ChangedPath> {
    let specs: Vec<&String> = pathspecs.iter().filter(|p| validate_rel(p)).collect();
    if specs.is_empty() || !has_git(root) {
        return Vec::new();
    }
    let mut args: Vec<&str> = vec!["status", "--porcelain=v1", "-z", "--untracked-files=all", "--"];
    args.extend(specs.iter().map(|s| s.as_str()));
    let Some(out) = git(root, &args) else { return Vec::new() };
    let mut result = Vec::new();
    let mut entries = out.split('\0').filter(|e| !e.is_empty());
    while let Some(e) = entries.next() {
        if e.len() < 4 {
            continue;
        }
        let code = &e[..2];
        let path = e[3..].to_string();
        if code.starts_with('R') || code.starts_with('C') {
            let _ = entries.next(); // original path
        }
        let status = if code == "??" || code.contains('A') {
            "added"
        } else if code.contains('D') {
            "deleted"
        } else {
            "modified"
        };
        let (additions, deletions, binary) = numstat(root, &path, status);
        result.push(ChangedPath { path, status: status.to_string(), additions, deletions, binary });
        if result.len() >= 500 {
            break;
        }
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    result
}

fn numstat(root: &Path, path: &str, status: &str) -> (usize, usize, bool) {
    if status == "added" {
        if let Ok(bytes) = std::fs::read(root.join(path)) {
            if bytes.contains(&0) {
                return (0, 0, true);
            }
            let text = String::from_utf8_lossy(&bytes);
            return (text.lines().count(), 0, false);
        }
        return (0, 0, false);
    }
    let out = git(root, &["diff", "--numstat", "HEAD", "--", path]).unwrap_or_default();
    let line = out.lines().next().unwrap_or("");
    let mut parts = line.split('\t');
    let a = parts.next().unwrap_or("0");
    let d = parts.next().unwrap_or("0");
    if a == "-" {
        return (0, 0, true);
    }
    (a.parse().unwrap_or(0), d.parse().unwrap_or(0), false)
}

/// Unified diff of one path against HEAD (untracked files rendered as additions).
pub fn diff_path(root: &Path, path: &str, status: &str, max_chars: usize) -> (String, bool) {
    if !validate_rel(path) {
        return (String::new(), false);
    }
    let text = if status == "added" {
        match std::fs::read(root.join(path)) {
            Ok(bytes) if bytes.contains(&0) => "Binary file".to_string(),
            Ok(bytes) => {
                let t = String::from_utf8_lossy(&bytes);
                let mut out = format!("--- /dev/null\n+++ b/{}\n", path);
                for l in t.lines() {
                    out.push('+');
                    out.push_str(l);
                    out.push('\n');
                }
                out
            }
            Err(_) => String::new(),
        }
    } else {
        git(root, &["diff", "--no-color", "--no-ext-diff", "HEAD", "--", path]).unwrap_or_default()
    };
    if text.chars().count() > max_chars {
        (text.chars().take(max_chars).collect(), true)
    } else {
        (text, false)
    }
}

/// Stage exactly `paths` and commit them with `message`. Never pushes.
pub fn commit_paths(root: &Path, paths: &[String], message: &str) -> Result<String, String> {
    let valid: Vec<&String> = paths.iter().filter(|p| validate_rel(p)).collect();
    if valid.is_empty() {
        return Err("Nothing to commit".to_string());
    }
    let run = |args: Vec<&str>| -> Result<std::process::Output, String> {
        Command::new("git")
            .args(&args)
            .current_dir(root)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|e| e.to_string())
    };
    let mut add = vec!["add", "-A", "--"];
    add.extend(valid.iter().map(|s| s.as_str()));
    let out = run(add)?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let mut commit = vec!["commit", "-m", message, "--"];
    commit.extend(valid.iter().map(|s| s.as_str()));
    let out = run(commit)?;
    if !out.status.success() {
        let msg = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        return Err(msg.trim().to_string());
    }
    git(root, &["rev-parse", "HEAD"]).map(|s| s.trim().to_string()).ok_or_else(|| "commit created but HEAD unreadable".into())
}
