//! TOOL_CONFIG_DRIFT: installed copies of the Knobyte tool config (CLAUDE.md, AGENTS.md,
//! .cursorrules, ...) must agree, ignoring Knobyte-managed blocks.

use std::fs;
use std::sync::OnceLock;

use regex::Regex;

use super::CheckContext;
use crate::drift::types::{codes, DriftIssue, SEVERITY_WARNING};

/// Files setup may install with identical content.
pub const TOOL_CONFIG_FILES: &[&str] = &[
    "CLAUDE.md",
    "AGENTS.md",
    ".cursorrules",
    ".windsurfrules",
    ".github/copilot-instructions.md",
];

/// Sentinel (at the start of a line) that marks a file as a copy of the Knobyte tool config.
pub const TOOL_CONFIG_MARKER: &str = "<!-- knobyte-tool-config";
/// Managed anchor block that may legitimately differ between copies.
pub const ANCHOR_START: &str = "<!-- knobyte-anchor:start -->";
pub const ANCHOR_END: &str = "<!-- knobyte-anchor:end -->";
/// Managed agent-skills block that may legitimately differ between copies.
pub const SKILLS_START: &str = "<!-- knobyte-agent:skills:start -->";
pub const SKILLS_END: &str = "<!-- knobyte-agent:skills:end -->";

fn marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^<!-- knobyte-tool-config\b").unwrap())
}

/// Whether `content` is a copy of the Knobyte tool config.
pub fn is_tool_config_copy(content: &str) -> bool {
    marker_re().is_match(content)
}

fn strip_managed_blocks(content: &str) -> String {
    let mut result = content.to_string();
    for (start, end) in [(SKILLS_START, SKILLS_END), (ANCHOR_START, ANCHOR_END)] {
        if let Some(from) = result.find(start) {
            if let Some(rel) = result[from..].find(end) {
                let to = from + rel + end.len();
                result = format!("{}{}", &result[..from], &result[to..]);
            }
        }
    }
    result.replace("\r\n", "\n").trim().to_string()
}

fn drift(path: &str, reference: &str) -> DriftIssue {
    DriftIssue::new(
        codes::TOOL_CONFIG_DRIFT,
        SEVERITY_WARNING,
        path,
        None,
        format!(
            "Tool config {} has drifted from {}. Re-copy the agreed version or edit both to match.",
            path, reference
        ),
    )
}

pub fn check_tool_config_sync(ctx: &CheckContext) -> Vec<DriftIssue> {
    let mut present: Vec<(&str, String)> = Vec::new();
    for rel in TOOL_CONFIG_FILES {
        let Ok(content) = fs::read_to_string(ctx.project_root.join(rel)) else {
            continue;
        };
        if !is_tool_config_copy(&content) {
            continue;
        }
        present.push((rel, strip_managed_blocks(&content)));
    }
    if present.len() < 2 {
        return Vec::new();
    }

    // Group by content, keeping file order.
    let mut groups: Vec<(String, Vec<&str>)> = Vec::new();
    for (path, content) in &present {
        match groups.iter_mut().find(|(c, _)| c == content) {
            Some((_, g)) => g.push(path),
            None => groups.push((content.clone(), vec![path])),
        }
    }
    if groups.len() == 1 {
        return Vec::new();
    }
    if present.len() == 2 {
        return vec![drift(present[1].0, present[0].0)];
    }

    let largest_len = groups.iter().map(|(_, g)| g.len()).max().unwrap_or(0);
    let tied = groups
        .iter()
        .filter(|(_, g)| g.len() == largest_len)
        .count()
        > 1;
    if tied {
        let summary = groups
            .iter()
            .map(|(_, g)| format!("[{}]", g.join(", ")))
            .collect::<Vec<_>>()
            .join(" vs ");
        return vec![DriftIssue::new(
            codes::TOOL_CONFIG_DRIFT,
            SEVERITY_WARNING,
            groups[0].1[0],
            None,
            format!(
                "Tool configs have diverged into {} groups with no majority, so none can be identified as the edited one: {}.",
                groups.len(),
                summary
            ),
        )];
    }
    let largest_idx = groups
        .iter()
        .position(|(_, g)| g.len() == largest_len)
        .unwrap();
    let reference = groups[largest_idx].1[0];
    let mut issues = Vec::new();
    for (i, (_, g)) in groups.iter().enumerate() {
        if i == largest_idx {
            continue;
        }
        for path in g {
            issues.push(drift(path, reference));
        }
    }
    issues
}
