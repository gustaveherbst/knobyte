//! DEAD_EDGE: frontmatter navigation targets must exist.
//!
//! Two shapes are checked: legacy `edges: [{ target: <path> }]` (resolved from the
//! declaring file's directory, then the scaffold root, then the project root) and Knobyte wiki
//! `relations: [{ type, target_id }]`, whose target must be the `id` of a scaffold entity (or an
//! existing path).

use std::collections::HashSet;
use std::path::Path;

use super::CheckContext;
use crate::drift::types::{codes, DriftIssue, SEVERITY_ERROR};

fn target_exists(target: &str, file_path: &Path, ctx: &CheckContext) -> bool {
    let from_file = file_path
        .parent()
        .map(|d| d.join(target).exists())
        .unwrap_or(false);
    from_file || ctx.scaffold_root.join(target).exists() || ctx.project_root.join(target).exists()
}

pub fn check_edges(
    frontmatter: Option<&serde_json::Value>,
    file_path: &Path,
    source: &str,
    ctx: &CheckContext,
    entity_ids: &HashSet<String>,
) -> Vec<DriftIssue> {
    let Some(fm) = frontmatter else {
        return Vec::new();
    };
    let mut issues = Vec::new();

    if let Some(edges) = fm.get("edges").and_then(|e| e.as_array()) {
        for edge in edges {
            let Some(target) = edge.get("target").and_then(|t| t.as_str()) else {
                continue;
            };
            if target.is_empty() {
                continue;
            }
            if !target_exists(target, file_path, ctx) {
                issues.push(DriftIssue::new(
                    codes::DEAD_EDGE,
                    SEVERITY_ERROR,
                    source,
                    None,
                    format!("Frontmatter edge target does not exist: {}", target),
                ));
            }
        }
    }

    if let Some(relations) = fm.get("relations").and_then(|r| r.as_array()) {
        for rel in relations {
            let Some(target) = rel.get("target_id").and_then(|t| t.as_str()) else {
                continue;
            };
            let target = target.trim();
            if target.is_empty() || entity_ids.contains(target) {
                continue;
            }
            if target_exists(target, file_path, ctx) {
                continue;
            }
            issues.push(DriftIssue::new(
                codes::DEAD_EDGE,
                SEVERITY_ERROR,
                source,
                None,
                format!(
                    "Frontmatter relation target does not exist: {} (no scaffold entity has this id)",
                    target
                ),
            ));
        }
    }
    issues
}
