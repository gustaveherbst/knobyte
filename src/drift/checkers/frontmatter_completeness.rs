//! MISSING_FRONTMATTER_FIELD: `context/` and `patterns/` files should carry the recommended
//! frontmatter fields. Knobyte's own field names are accepted alongside the legacy ones
//! (`title` for `name`, `summary` for `description`).

use std::sync::OnceLock;

use regex::Regex;

use crate::drift::types::{codes, DriftIssue, SEVERITY_WARNING};

/// (reported field, accepted keys)
const RECOMMENDED_FIELDS: &[(&str, &[&str])] = &[
    ("name", &["name", "title"]),
    ("description", &["description", "summary"]),
    ("last_updated", &["last_updated"]),
];

const EXEMPT_PATTERN_FILES: &[&str] = &["INDEX.md", "README.md"];

fn in_scope() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(^|/)(context|patterns)/([^/]+\.md)$").unwrap())
}

fn truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        _ => true,
    }
}

pub fn check_frontmatter_completeness(
    frontmatter: Option<&serde_json::Value>,
    source: &str,
) -> Vec<DriftIssue> {
    let Some(caps) = in_scope().captures(source) else {
        return Vec::new();
    };
    if &caps[2] == "patterns" && EXEMPT_PATTERN_FILES.contains(&&caps[3]) {
        return Vec::new();
    }
    let mut issues = Vec::new();
    for (field, keys) in RECOMMENDED_FIELDS {
        let present = frontmatter
            .map(|fm| keys.iter().any(|k| fm.get(*k).map(truthy).unwrap_or(false)))
            .unwrap_or(false);
        if !present {
            issues.push(DriftIssue::new(
                codes::MISSING_FRONTMATTER_FIELD,
                SEVERITY_WARNING,
                source,
                None,
                format!("Missing recommended frontmatter field: `{}`", field),
            ));
        }
    }
    issues
}
