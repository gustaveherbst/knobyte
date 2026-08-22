//! Pointing each selected tool's always-loaded file at the scaffold.
//!
//! A populated `.knobyte/` is inert unless some file the agent loads on its own tells it to
//! read the scaffold. Claude Code and Codex are handled by the skills installer (managed block
//! in the root `CLAUDE.md` / `AGENTS.md`); this module handles Cursor, Windsurf, Copilot and
//! OpenCode. A missing anchor gets the full rules template; an existing hand-written anchor
//! gets a pointer block appended, preserving every byte the user wrote.

use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::drift::checkers::tool_config_sync::{is_tool_config_copy, ANCHOR_END, ANCHOR_START};
use crate::managed_block::{detect_eol, plan_block_edit, BlockAction, BlockReason, BlockSpec};
use crate::setup::templates::{OPENCODE_TEMPLATE, TOOL_RULES_TEMPLATE};

/// Refuse to edit an existing pointer block larger than this.
const MAX_ANCHOR_BLOCK_BYTES: usize = 32 * 1024;

/// (tool, project-relative anchor path, json?)
pub const TOOL_ANCHORS: &[(&str, &str, bool)] = &[
    ("cursor", ".cursorrules", false),
    ("windsurf", ".windsurfrules", false),
    ("copilot", ".github/copilot-instructions.md", false),
    ("opencode", ".opencode/opencode.json", true),
];

/// Display name of an AI tool id.
pub fn tool_display_name(tool: &str) -> &'static str {
    match tool {
        "claude" => "Claude Code",
        "codex" => "Codex",
        "cursor" => "Cursor",
        "windsurf" => "Windsurf",
        "copilot" => "GitHub Copilot",
        "opencode" => "OpenCode",
        _ => "Unknown tool",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnchorOutcome {
    /// No anchor existed; the full template was written.
    Created,
    /// A pointer block was appended to the user's existing file.
    Appended,
    /// An existing Knobyte pointer block was brought up to date.
    Updated,
    /// The anchor already points at the scaffold.
    AlreadyLinked,
    /// The file could not be edited safely and was left untouched.
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnchorResult {
    pub tool: String,
    pub path: String,
    pub outcome: AnchorOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn scaffold_reference_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(^|[^\w./-])\.knobyte/").unwrap())
}

/// Whether markdown text points an agent at the scaffold.
pub fn references_scaffold(content: &str) -> bool {
    scaffold_reference_re().is_match(content)
}

/// The block appended to a markdown anchor that has no Knobyte pointer.
pub fn render_anchor_block(eol: &str) -> String {
    [
        ANCHOR_START,
        "## Knobyte project context",
        "- At the start of every session, read `.knobyte/AGENTS.md` and `.knobyte/ROUTER.md` before project work; follow `ROUTER.md` to load only the relevant context.",
        "- Treat the scaffold as the source of truth for architecture, stack, conventions, and decisions; prefer it over re-deriving context from the code.",
        "- Do not claim an author, date, or historical event unless the retrieved data actually provides it.",
        ANCHOR_END,
    ]
    .join(eol)
}

fn conflict_reason(reason: BlockReason) -> String {
    match reason {
        BlockReason::InvalidEncoding => "the file is not valid UTF-8".to_string(),
        BlockReason::MalformedMarkers => format!(
            "it contains unbalanced {} / {} markers",
            ANCHOR_START, ANCHOR_END
        ),
        _ => "its existing Knobyte block is too large to edit safely".to_string(),
    }
}

/// Plan a markdown anchor edit: `(outcome, reason, desired bytes)`.
pub fn plan_markdown_anchor(current: Option<&[u8]>) -> (AnchorOutcome, Option<String>, Option<Vec<u8>>) {
    let Some(bytes) = current else {
        return (AnchorOutcome::Created, None, Some(TOOL_RULES_TEMPLATE.as_bytes().to_vec()));
    };
    let render = |eol: &str| render_anchor_block(eol);
    let pointing = |c: &str| is_tool_config_copy(c) || references_scaffold(c);
    let spec = BlockSpec {
        start: ANCHOR_START,
        end: ANCHOR_END,
        render: &render,
        max_block_bytes: MAX_ANCHOR_BLOCK_BYTES,
        legacy_hashes: &[],
        is_already_pointing: Some(&pointing),
    };
    let edit = plan_block_edit(&spec, Some(bytes));
    match edit.action {
        BlockAction::Noop => (AnchorOutcome::AlreadyLinked, None, None),
        BlockAction::Update | BlockAction::Create | BlockAction::Migrate => {
            let outcome = if edit.reason == BlockReason::Append {
                AnchorOutcome::Appended
            } else {
                AnchorOutcome::Updated
            };
            (outcome, None, edit.desired)
        }
        BlockAction::Conflict => (AnchorOutcome::Conflict, Some(conflict_reason(edit.reason)), None),
    }
}

/// Plan the OpenCode edit: append `.knobyte/AGENTS.md` to its `instructions` array.
pub fn plan_opencode_anchor(current: Option<&str>) -> (AnchorOutcome, Option<String>, Option<String>) {
    let Some(content) = current else {
        return (AnchorOutcome::Created, None, Some(OPENCODE_TEMPLATE.to_string()));
    };
    let bad = || {
        (
            AnchorOutcome::Conflict,
            Some("it is not a JSON object with an optional string `instructions` array".to_string()),
            None,
        )
    };
    let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(content) else {
        return bad();
    };
    let Some(obj) = parsed.as_object_mut() else {
        return bad();
    };
    let list: Vec<String> = match obj.get("instructions") {
        None => Vec::new(),
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        Some(_) => return bad(),
    };
    if list.iter().any(|e| e.starts_with(".knobyte/") || references_scaffold(e)) {
        return (AnchorOutcome::AlreadyLinked, None, None);
    }
    let mut new_list: Vec<serde_json::Value> = obj
        .get("instructions")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    new_list.push(serde_json::Value::String(".knobyte/AGENTS.md".to_string()));
    obj.insert("instructions".to_string(), serde_json::Value::Array(new_list));
    let eol = detect_eol(content);
    let serialized = format!("{}\n", serde_json::to_string_pretty(&parsed).unwrap_or_default());
    (AnchorOutcome::Appended, None, Some(serialized.replace('\n', eol)))
}

/// Ensure every selected non-agent tool's anchor points at the scaffold (or, with `dry_run`,
/// report what would happen). Unknown and agent tools are skipped.
pub fn ensure_tool_anchors(project_root: &Path, tools: &[String], dry_run: bool) -> Vec<AnchorResult> {
    let mut out = Vec::new();
    for (tool, rel, json) in TOOL_ANCHORS {
        if !tools.iter().any(|t| t == tool) {
            continue;
        }
        let path = project_root.join(rel);
        let (outcome, reason, desired): (AnchorOutcome, Option<String>, Option<Vec<u8>>) = if *json {
            let current = fs::read_to_string(&path).ok();
            let (o, r, d) = plan_opencode_anchor(current.as_deref());
            (o, r, d.map(String::into_bytes))
        } else {
            let current = fs::read(&path).ok();
            plan_markdown_anchor(current.as_deref())
        };
        let mut result = AnchorResult { tool: tool.to_string(), path: rel.to_string(), outcome, reason };
        if !dry_run {
            if let Some(bytes) = desired {
                let write = path
                    .parent()
                    .map(fs::create_dir_all)
                    .unwrap_or(Ok(()))
                    .and_then(|_| fs::write(&path, bytes));
                if let Err(e) = write {
                    result.outcome = AnchorOutcome::Conflict;
                    result.reason = Some(format!("it could not be written: {}", e));
                }
            }
        }
        out.push(result);
    }
    out
}

/// Human line for one anchor outcome.
pub fn describe_anchor(r: &AnchorResult, dry_run: bool) -> String {
    let would = if dry_run { "Would " } else { "" };
    match r.outcome {
        AnchorOutcome::Created => format!("{}{} {}", would, if dry_run { "create" } else { "Created" }, r.path),
        AnchorOutcome::Appended => format!(
            "{}{} a Knobyte pointer to your existing {}",
            would,
            if dry_run { "add" } else { "Added" },
            r.path
        ),
        AnchorOutcome::Updated => format!("{}{} the Knobyte pointer in {}", would, if dry_run { "refresh" } else { "Refreshed" }, r.path),
        AnchorOutcome::AlreadyLinked => format!("{} already points at .knobyte/; left unchanged", r.path),
        AnchorOutcome::Conflict => format!(
            "{} was left untouched because {}. Add this line to it by hand so the scaffold is loaded: `At the start of every session, read .knobyte/AGENTS.md and .knobyte/ROUTER.md.`",
            r.path,
            r.reason.as_deref().unwrap_or("it could not be edited safely")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_anchor_outcomes() {
        let (o, _, d) = plan_markdown_anchor(None);
        assert_eq!(o, AnchorOutcome::Created);
        assert!(is_tool_config_copy(&String::from_utf8(d.unwrap()).unwrap()));

        let (o, _, d) = plan_markdown_anchor(Some(b"# My rules\r\nbe nice\r\n"));
        assert_eq!(o, AnchorOutcome::Appended);
        let out = String::from_utf8(d.unwrap()).unwrap();
        assert!(out.starts_with("# My rules\r\nbe nice\r\n\r\n<!-- knobyte-anchor:start -->\r\n"));
        assert_eq!(plan_markdown_anchor(Some(out.as_bytes())).0, AnchorOutcome::AlreadyLinked);

        let (o, _, _) = plan_markdown_anchor(Some(b"Read .knobyte/ROUTER.md first.\n"));
        assert_eq!(o, AnchorOutcome::AlreadyLinked);
        let (o, r, _) = plan_markdown_anchor(Some(b"<!-- knobyte-anchor:start -->\nno end\n"));
        assert_eq!(o, AnchorOutcome::Conflict);
        assert!(r.unwrap().contains("unbalanced"));
    }

    #[test]
    fn opencode_anchor_outcomes() {
        assert_eq!(plan_opencode_anchor(None).0, AnchorOutcome::Created);
        let (o, _, d) = plan_opencode_anchor(Some("{\n  \"model\": \"x\"\n}\n"));
        assert_eq!(o, AnchorOutcome::Appended);
        let d = d.unwrap();
        assert!(d.contains("\"model\": \"x\"") && d.contains(".knobyte/AGENTS.md"));
        assert_eq!(plan_opencode_anchor(Some(&d)).0, AnchorOutcome::AlreadyLinked);
        assert_eq!(plan_opencode_anchor(Some("[1]")).0, AnchorOutcome::Conflict);
        assert_eq!(plan_opencode_anchor(Some("{\"instructions\": 3}")).0, AnchorOutcome::Conflict);
    }
}
