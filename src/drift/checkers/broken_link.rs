//! BROKEN_LINK: local markdown links whose target does not exist.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use super::CheckContext;
use crate::drift::types::{codes, DriftIssue, SEVERITY_ERROR, SEVERITY_WARNING};

fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[([^\]]*)\]\(([^)]+)\)").unwrap())
}

fn inline_code_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"`[^`]+`").unwrap())
}

fn title_split_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"^(\S+)(?:\s+["'].+["'])?$"#).unwrap())
}

fn external_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^(?:https?://|mailto:|knobyte://)").unwrap())
}

/// Remove commented-out spans of one line, continuing a comment opened on an earlier line.
fn strip_html_comments(line: &str, in_comment: bool) -> (String, bool) {
    let mut text = String::new();
    let mut idx = 0;
    let mut open = in_comment;
    while idx < line.len() {
        if open {
            match line[idx..].find("-->") {
                Some(close) => {
                    open = false;
                    idx += close + 3;
                }
                None => break,
            }
            continue;
        }
        match line[idx..].find("<!--") {
            Some(start) => {
                text.push_str(&line[idx..idx + start]);
                open = true;
                idx += start + 4;
            }
            None => {
                text.push_str(&line[idx..]);
                break;
            }
        }
    }
    (text, open)
}

fn normalize_target(raw: &str) -> String {
    let mut target = raw
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_string();
    if let Some(c) = title_split_re().captures(&target) {
        target = c[1].to_string();
    }
    if let Some(idx) = target.find(['#', '?']) {
        target.truncate(idx);
    }
    target
}

fn target_exists(target: &str, file_dir: &Path, ctx: &CheckContext) -> bool {
    if file_dir.join(target).exists() || ctx.project_root.join(target).exists() {
        return true;
    }
    if !ctx.scaffold_is_project() && ctx.scaffold_root.join(target).exists() {
        return true;
    }
    if let Some(prefix) = ctx.scaffold_prefix() {
        if let Some(rest) = target.strip_prefix(prefix.as_str()) {
            if ctx.project_root.join(rest).exists() {
                return true;
            }
        }
    }
    false
}

/// Scan one scaffold file; `file_path` is absolute, `source` project-relative.
pub fn check_broken_links(
    content: &str,
    file_path: &Path,
    source: &str,
    ctx: &CheckContext,
) -> Vec<DriftIssue> {
    let file_dir = file_path.parent().unwrap_or(&ctx.project_root);
    let mut issues = Vec::new();
    let mut in_fence = false;
    let mut in_comment = false;

    for (i, line) in content.split('\n').enumerate() {
        if !in_comment {
            if line.trim().starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                continue;
            }
        }
        let without_code = if in_comment {
            line.to_string()
        } else {
            inline_code_re().replace_all(line, "").to_string()
        };
        let (scan, open) = strip_html_comments(&without_code, in_comment);
        in_comment = open;

        for caps in link_re().captures_iter(&scan) {
            let target = normalize_target(&caps[2]);
            if target.is_empty() || external_re().is_match(&target) || target.starts_with('#') {
                continue;
            }
            if !target_exists(&target, file_dir, ctx) {
                let severity = if source.contains("patterns/") {
                    SEVERITY_WARNING
                } else {
                    SEVERITY_ERROR
                };
                issues.push(DriftIssue::new(
                    codes::BROKEN_LINK,
                    severity,
                    source,
                    Some(i + 1),
                    format!("Markdown link target does not exist: {}", target),
                ));
            }
        }
    }
    issues
}
