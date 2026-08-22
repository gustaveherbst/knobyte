//! Repository provenance (branch and HEAD commit) read directly from the `.git` directory,
//! without spawning `git`: cheap enough for every `graph status` and every build.
//!
//! Handles plain repositories, linked worktrees (`.git` file with `gitdir:`, `commondir`),
//! symbolic and detached HEADs, loose and packed refs.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepoState {
    /// Checked-out branch (`None` when HEAD is detached).
    pub branch: Option<String>,
    /// Commit HEAD points at (`None` on an unborn branch).
    pub head: Option<String>,
}

/// The git directory of the repository containing `root` (walking up), if any.
fn git_dir(root: &Path) -> Option<PathBuf> {
    let start = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut cur: Option<&Path> = Some(start.as_path());
    while let Some(dir) = cur {
        let dot = dir.join(".git");
        if dot.is_dir() {
            return Some(dot);
        }
        if dot.is_file() {
            let text = fs::read_to_string(&dot).ok()?;
            let target = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
            let p = PathBuf::from(target);
            return Some(if p.is_absolute() { p } else { dir.join(p) });
        }
        cur = dir.parent();
    }
    None
}

/// Directory holding shared refs (`commondir` of a linked worktree, else the git dir).
fn common_dir(git_dir: &Path) -> PathBuf {
    match fs::read_to_string(git_dir.join("commondir")) {
        Ok(c) => {
            let p = PathBuf::from(c.trim());
            if p.is_absolute() {
                p
            } else {
                git_dir.join(p)
            }
        }
        Err(_) => git_dir.to_path_buf(),
    }
}

fn is_object_id(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn resolve_ref(git_dir: &Path, common: &Path, name: &str, depth: usize) -> Option<String> {
    if depth > 8 {
        return None;
    }
    for dir in [git_dir, common] {
        if let Ok(v) = fs::read_to_string(dir.join(name)) {
            let v = v.trim();
            if let Some(next) = v.strip_prefix("ref:") {
                return resolve_ref(git_dir, common, next.trim(), depth + 1);
            }
            if is_object_id(v) {
                return Some(v.to_string());
            }
        }
    }
    let packed = fs::read_to_string(common.join("packed-refs")).ok()?;
    packed.lines().find_map(|l| {
        let (id, r) = l.split_once(' ')?;
        (r.trim() == name && is_object_id(id)).then(|| id.to_string())
    })
}

/// Branch and HEAD of the repository containing `root`; `None` outside a git repository or
/// when HEAD cannot be read.
pub fn repo_state(root: &Path) -> Option<RepoState> {
    let dir = git_dir(root)?;
    let common = common_dir(&dir);
    let head = fs::read_to_string(dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(r) = head.strip_prefix("ref:") {
        let r = r.trim();
        Some(RepoState {
            branch: Some(r.strip_prefix("refs/heads/").unwrap_or(r).to_string()),
            head: resolve_ref(&dir, &common, r, 0),
        })
    } else if is_object_id(head) {
        Some(RepoState { branch: None, head: Some(head.to_string()) })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_symbolic_detached_packed_and_worktree_heads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = root.join("repo/.git");
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(git.join("refs/heads/main"), format!("{}\n", a)).unwrap();
        let st = repo_state(&root.join("repo")).unwrap();
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.head.as_deref(), Some(a.as_str()));
        // Packed ref.
        fs::write(git.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        fs::write(git.join("packed-refs"), format!("# pack-refs\n{} refs/heads/feature/x\n", b)).unwrap();
        let st = repo_state(&root.join("repo")).unwrap();
        assert_eq!(st.branch.as_deref(), Some("feature/x"));
        assert_eq!(st.head.as_deref(), Some(b.as_str()));
        // Unborn branch.
        fs::write(git.join("HEAD"), "ref: refs/heads/new\n").unwrap();
        let st = repo_state(&root.join("repo")).unwrap();
        assert_eq!((st.branch.as_deref(), st.head), (Some("new"), None));
        // Detached.
        fs::write(git.join("HEAD"), format!("{}\n", a)).unwrap();
        let st = repo_state(&root.join("repo/sub")).unwrap();
        assert_eq!((st.branch, st.head.as_deref()), (None, Some(a.as_str())));
        // Linked worktree.
        let wt_git = git.join("worktrees/wt");
        fs::create_dir_all(&wt_git).unwrap();
        fs::write(wt_git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        fs::create_dir_all(root.join("wt")).unwrap();
        fs::write(root.join("wt/.git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        let st = repo_state(&root.join("wt")).unwrap();
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.head.as_deref(), Some(a.as_str()));
    }
}
