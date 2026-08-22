//! TODO_FIXME: unresolved TODO / FIXME markers in scaffold markdown.

use std::sync::OnceLock;

use regex::Regex;

use crate::drift::types::{codes, DriftIssue, SEVERITY_WARNING};

fn marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(TODO|FIXME)\b").unwrap())
}

/// Scan one file's `content`; `source` is its project-relative path.
pub fn check_todo_fixme(content: &str, source: &str) -> Vec<DriftIssue> {
    let mut issues = Vec::new();
    for (i, line) in content.split('\n').enumerate() {
        for caps in marker_re().captures_iter(line) {
            issues.push(DriftIssue::new(
                codes::TODO_FIXME,
                SEVERITY_WARNING,
                source,
                Some(i + 1),
                format!("Unresolved {} marker in scaffold", &caps[1]),
            ));
        }
    }
    issues
}
