//! SCAFFOLD_ORPHANED: a populated scaffold that no always-loaded tool config points at, so no
//! agent ever reads it.

use std::fs;
use std::sync::OnceLock;

use regex::Regex;

use super::{read_json, CheckContext};
use crate::drift::types::{codes, DriftIssue, SEVERITY_ERROR};

/// (path, format) of the files coding agents load on their own.
pub const ANCHOR_FILES: &[(&str, &str)] = &[
    ("CLAUDE.md", "markdown"),
    ("AGENTS.md", "markdown"),
    (".cursorrules", "markdown"),
    (".windsurfrules", "markdown"),
    (".github/copilot-instructions.md", "markdown"),
    (".opencode/opencode.json", "json"),
];

fn scaffold_reference_re(dir: &str) -> Regex {
    Regex::new(&format!(r"(^|[^\w./-]){}/", regex::escape(dir))).unwrap()
}

fn default_reference_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| scaffold_reference_re(".knobyte"))
}

/// The scaffold directory name as written in pointers (`.knobyte`).
fn scaffold_dir_name(ctx: &CheckContext) -> String {
    ctx.scaffold_prefix()
        .map(|p| p.trim_end_matches('/').to_string())
        .unwrap_or_else(|| ".knobyte".to_string())
}

/// An explicit `ai_tools: []` / `aiTools: []` in config.json records a decision to install no
/// tool config.
fn opted_out_of_tool_configs(ctx: &CheckContext) -> bool {
    let Some(cfg) = read_json(&ctx.scaffold_root.join("config.json")) else {
        return false;
    };
    ["ai_tools", "aiTools"].iter().any(|k| {
        cfg.get(*k)
            .and_then(|v| v.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(false)
    })
}

fn anchor_points_at_scaffold(ctx: &CheckContext, path: &str, format: &str, re: &Regex) -> bool {
    let Ok(content) = fs::read_to_string(ctx.project_root.join(path)) else {
        return false;
    };
    if format == "json" {
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) else {
            return false;
        };
        let Some(list) = parsed.get("instructions").and_then(|i| i.as_array()) else {
            return false;
        };
        let dir = format!("{}/", scaffold_dir_name(ctx));
        return list
            .iter()
            .filter_map(|v| v.as_str())
            .any(|e| e.starts_with(&dir) || re.is_match(e));
    }
    re.is_match(&content)
}

fn describe(paths: &[&str]) -> String {
    format!(
        "{} and {}",
        paths[..paths.len() - 1].join(", "),
        paths[paths.len() - 1]
    )
}

pub fn check_anchor_link(ctx: &CheckContext) -> Vec<DriftIssue> {
    let router = ctx.scaffold_root.join("ROUTER.md");
    if !router.exists() {
        return Vec::new();
    }
    if opted_out_of_tool_configs(ctx) {
        return Vec::new();
    }
    let dir = scaffold_dir_name(ctx);
    let custom_re;
    let re: &Regex = if dir == ".knobyte" {
        default_reference_re()
    } else {
        custom_re = scaffold_reference_re(&dir);
        &custom_re
    };

    let present: Vec<(&str, &str)> = ANCHOR_FILES
        .iter()
        .copied()
        .filter(|(p, _)| ctx.project_root.join(p).exists())
        .collect();

    if present.is_empty() {
        return vec![DriftIssue::new(
            codes::SCAFFOLD_ORPHANED,
            SEVERITY_ERROR,
            ctx.rel(&router),
            None,
            format!(
                "The scaffold is populated but no AI tool config exists to load it, so no agent will read it. Create one ({}) that points at `{}/ROUTER.md`.",
                ANCHOR_FILES.iter().map(|(p, _)| *p).collect::<Vec<_>>().join(", "),
                dir
            ),
        )];
    }

    if present
        .iter()
        .any(|(p, f)| anchor_points_at_scaffold(ctx, p, f, re))
    {
        return Vec::new();
    }

    let paths: Vec<&str> = present.iter().map(|(p, _)| *p).collect();
    let subject = if paths.len() == 1 {
        format!("{} never mentions `{}/`", paths[0], dir)
    } else {
        format!(
            "{} exist, but none of them mentions `{}/`",
            describe(&paths),
            dir
        )
    };
    vec![DriftIssue::new(
        codes::SCAFFOLD_ORPHANED,
        SEVERITY_ERROR,
        paths[0],
        None,
        format!(
            "{}, so the populated scaffold is never loaded. Add a line naming `{}/ROUTER.md` to it.",
            subject, dir
        ),
    )]
}
