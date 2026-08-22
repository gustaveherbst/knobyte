//! STALE_PATTERN: a pattern file nothing in ROUTER.md or context/*.md leads to.

use std::collections::HashSet;
use std::fs;
use std::sync::OnceLock;

use regex::Regex;

use super::index_sync::{backtick_md_re, pattern_files};
use super::{md_files_in, strip_html_comments, CheckContext};
use crate::drift::markdown::parse_frontmatter;
use crate::drift::types::{codes, DriftIssue, SEVERITY_WARNING};

fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\[[^\]\n]*?\]\((?:\.\.?/)?patterns/([^)\n]+?\.md)(?:#[\w-]+)?\)").unwrap()
    })
}

fn edge_target_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?:^|/)patterns/([^/]+\.md)$").unwrap())
}

pub fn check_stale_patterns(ctx: &CheckContext) -> Vec<DriftIssue> {
    let patterns_dir = ctx.scaffold_or_project("patterns");
    if !patterns_dir.exists() {
        return Vec::new();
    }
    let patterns = pattern_files(&patterns_dir);
    if patterns.is_empty() {
        return Vec::new();
    }

    let mut referencing = Vec::new();
    let router = ctx.scaffold_or_project("ROUTER.md");
    if router.exists() {
        referencing.push(router);
    }
    let context_dir = ctx.scaffold_or_project("context");
    for f in md_files_in(&context_dir) {
        referencing.push(context_dir.join(f));
    }

    let mut referenced: HashSet<String> = HashSet::new();
    let mut related_ids: HashSet<String> = HashSet::new();
    for path in referencing {
        let Ok(raw) = fs::read_to_string(&path) else {
            continue;
        };
        let content = strip_html_comments(&raw);
        for caps in link_re().captures_iter(&content) {
            referenced.insert(caps[1].to_string());
        }
        for caps in backtick_md_re().captures_iter(&content) {
            referenced.insert(caps[1].to_string());
        }
        if let Some(fm) = parse_frontmatter(&raw) {
            for rel in fm.get("relations").and_then(|r| r.as_array()).into_iter().flatten() {
                if let Some(t) = rel.get("target_id").or_else(|| rel.get("target")).and_then(|t| t.as_str()) {
                    related_ids.insert(t.trim().to_string());
                }
            }
            if let Some(edges) = fm.get("edges").and_then(|e| e.as_array()) {
                for edge in edges {
                    if let Some(t) = edge.get("target").and_then(|t| t.as_str()) {
                        if let Some(c) = edge_target_re().captures(t) {
                            referenced.insert(c[1].to_string());
                        }
                    }
                }
            }
        }
    }

    let mut issues = Vec::new();
    for file in patterns {
        let path = patterns_dir.join(&file);
        // A relation to the pattern's entity id also leads to it.
        let related = !related_ids.is_empty()
            && fs::read_to_string(&path)
                .ok()
                .and_then(|c| parse_frontmatter(&c))
                .and_then(|fm| fm.get("id").and_then(|v| v.as_str()).map(|s| s.trim().to_string()))
                .is_some_and(|id| related_ids.contains(&id));
        if !referenced.contains(&file) && !related {
            issues.push(DriftIssue::new(
                codes::STALE_PATTERN,
                SEVERITY_WARNING,
                ctx.rel(&path),
                None,
                format!(
                    "Pattern file patterns/{} is not referenced from ROUTER.md or context/*.md",
                    file
                ),
            ));
        }
    }
    issues
}
