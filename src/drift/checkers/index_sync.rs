//! INDEX_MISSING_ENTRY / INDEX_ORPHAN_ENTRY: `patterns/INDEX.md` must list exactly the pattern
//! files on disk.

use std::collections::BTreeSet;
use std::fs;
use std::sync::OnceLock;

use regex::Regex;

use super::{md_files_in, strip_html_comments, CheckContext};
use crate::drift::types::{codes, DriftIssue, SEVERITY_WARNING};

fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[[^\]\n]*?\]\(([^)\n]+?\.md(?:#[\w-]+)?)\)").unwrap())
}

pub(crate) fn backtick_md_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`([\w-]+\.md)`").unwrap())
}

/// Pattern files (INDEX.md and README.md excluded).
pub(crate) fn pattern_files(dir: &std::path::Path) -> Vec<String> {
    md_files_in(dir)
        .into_iter()
        .filter(|f| f != "INDEX.md" && f != "README.md")
        .collect()
}

pub fn check_index_sync(ctx: &CheckContext) -> Vec<DriftIssue> {
    let patterns_dir = ctx.scaffold_or_project("patterns");
    let index_path = patterns_dir.join("INDEX.md");
    let Ok(raw) = fs::read_to_string(&index_path) else {
        return Vec::new();
    };
    let index_file = ctx.rel(&index_path);
    let content = strip_html_comments(&raw);

    let mut referenced: BTreeSet<String> = BTreeSet::new();
    for caps in link_re().captures_iter(&content) {
        let target = caps[1].split('#').next().unwrap_or("").to_string();
        referenced.insert(target);
    }
    for caps in backtick_md_re().captures_iter(&content) {
        referenced.insert(caps[1].to_string());
    }

    let mut issues = Vec::new();
    for file in pattern_files(&patterns_dir) {
        if !referenced.contains(&file) {
            issues.push(DriftIssue::new(
                codes::INDEX_MISSING_ENTRY,
                SEVERITY_WARNING,
                index_file.clone(),
                None,
                format!(
                    "Pattern file patterns/{} exists but is not referenced in INDEX.md",
                    file
                ),
            ));
        }
    }
    for reference in &referenced {
        if !patterns_dir.join(reference).exists() {
            issues.push(DriftIssue::new(
                codes::INDEX_ORPHAN_ENTRY,
                SEVERITY_WARNING,
                index_file.clone(),
                None,
                format!(
                    "INDEX.md references {} but the file does not exist",
                    reference
                ),
            ));
        }
    }
    issues
}
