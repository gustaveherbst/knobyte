//! Repair brief for `knobyte sync`: drift issues grouped per file, with the file content,
//! filesystem context for missing paths, recent git changes, and the old/new bodies of drifted
//! groundings. This only builds the text; launching an agent with it is up to the caller.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::config::KnobyteConfig;
use crate::drift::checker::DriftReport;
use crate::drift::checkers::walk_index;
use crate::drift::types::{codes, ClaimKind, DriftIssue};
use crate::graph::grounding::{get_baseline, read_node_source, resolve_grounding_ref};
use crate::graph::GraphEngine;

/// One file and the issues to repair in it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncTarget {
    /// Project-relative path.
    pub file: String,
    pub issues: Vec<DriftIssue>,
}

impl SyncTarget {
    pub fn errors(&self) -> usize {
        self.issues.iter().filter(|i| i.is_error()).count()
    }
    pub fn warnings(&self) -> usize {
        self.issues
            .iter()
            .filter(|i| i.severity == "warning")
            .count()
    }
}

/// Per-file brief.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileBrief {
    pub file: String,
    /// Self-contained prompt for repairing just this file.
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SyncBrief {
    pub targets: Vec<SyncTarget>,
    /// One prompt covering every target ("fix all of them in one pass"); empty when clean.
    pub prompt: String,
    pub files: Vec<FileBrief>,
}

impl SyncBrief {
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SyncBriefOptions {
    /// Include warnings in files without errors (`knobyte sync --warnings`).
    pub include_warnings: bool,
}

/// Issues worth repairing: every grounding issue, plus every issue of a file that has an error
/// (or everything with `include_warnings`).
pub fn select_sync_issues(report: &DriftReport, include_warnings: bool) -> Vec<DriftIssue> {
    if include_warnings {
        return report.issues.clone();
    }
    let error_files: BTreeSet<&str> = report
        .issues
        .iter()
        .filter(|i| i.is_error())
        .map(|i| i.file.as_str())
        .collect();
    report
        .issues
        .iter()
        .filter(|i| i.is_grounding() || error_files.contains(i.file.as_str()))
        .cloned()
        .collect()
}

/// Group issues by file, preserving first-seen order.
pub fn group_into_targets(issues: Vec<DriftIssue>) -> Vec<SyncTarget> {
    let mut targets: Vec<SyncTarget> = Vec::new();
    for issue in issues {
        match targets.iter_mut().find(|t| t.file == issue.file) {
            Some(t) => t.issues.push(issue),
            None => targets.push(SyncTarget {
                file: issue.file.clone(),
                issues: vec![issue],
            }),
        }
    }
    targets
}

/// Build the repair brief for `report` (errors' files plus every grounding issue).
pub fn build_sync_brief(config: &KnobyteConfig, report: &DriftReport) -> SyncBrief {
    build_sync_brief_with(config, report, SyncBriefOptions::default())
}

pub fn build_sync_brief_with(
    config: &KnobyteConfig,
    report: &DriftReport,
    opts: SyncBriefOptions,
) -> SyncBrief {
    let targets = group_into_targets(select_sync_issues(report, opts.include_warnings));
    if targets.is_empty() {
        return SyncBrief::default();
    }
    let engine = {
        let db = config.graph_db_path();
        if db.exists() {
            GraphEngine::open(&db).ok()
        } else {
            None
        }
    };
    let sections: Vec<String> = targets
        .iter()
        .map(|t| build_file_section(config, t, engine.as_ref()))
        .collect();

    let files = targets
        .iter()
        .zip(&sections)
        .map(|(t, section)| FileBrief {
            file: t.file.clone(),
            prompt: single_file_prompt(section, std::slice::from_ref(t)),
        })
        .collect();

    let numbered = sections
        .iter()
        .enumerate()
        .map(|(i, s)| format!("━━━ File {}/{} ━━━\n\n{}", i + 1, sections.len(), s))
        .collect::<Vec<_>>()
        .join("\n\n");
    let prompt = format!(
        "The following scaffold files have drift issues that need fixing. Fix all of them in one pass.\n\n{}\n\n{}\n\nUpdate each file to fix its issues. Only change what's necessary — do not rewrite sections that are correct.\nWhen a referenced path no longer exists, find the correct current path from the filesystem context above and update the reference.",
        numbered,
        grounding_repair_instructions(&targets)
    );

    SyncBrief {
        targets,
        prompt,
        files,
    }
}

fn single_file_prompt(section: &str, targets: &[SyncTarget]) -> String {
    format!(
        "The following scaffold file has drift issues that need fixing:\n\n{}\n\n{}\n\nUpdate the file to fix these issues. Only change what's necessary — do not rewrite sections that are correct.\nWhen a referenced path no longer exists, find the correct current path from the filesystem context above and update the reference.",
        section,
        grounding_repair_instructions(targets)
    )
}

/// Instructions for repairing groundings; empty when no target has a grounding issue.
pub fn grounding_repair_instructions(targets: &[SyncTarget]) -> String {
    if !targets
        .iter()
        .any(|t| t.issues.iter().any(|i| i.is_grounding()))
    {
        return String::new();
    }
    "GROUNDING REPAIR — repair the prose and the grounding references together:

Use the code graph for implementation context; do not sample source files. Start
with `knobyte graph scope \"<behavior being repaired>\"`, then use `knobyte graph query
where-defined <symbol>`, who-calls/what-calls, or `knobyte impact <symbol|file>`
to resolve exact behavior and candidates. READ BROAD, GROUND TIGHT: read the
whole useful neighborhood, but ground only symbols that embody claims the
repaired prose actually makes.

- GROUNDING_DRIFT (body changed): decide whether the claim changed from the supplied
  old/new body. Repair only affected prose. Do not run `knobyte graph ground` yourself;
  accepting the new body is the user's decision.
- GROUNDING_DRIFT (moved) / GROUNDING_MOVED_BY_NEIGHBORS: `knobyte sync` rewrites
  high-confidence moves. Verify the `grounds_to` entry and every `<!-- kb-ground: ... -->`
  anchor for that symbol use the new reference. Identity repair does not establish that
  the documented behavior still agrees with the implementation.
- GROUNDING_AMBIGUOUS: adjudicate the surfaced candidate with scope/query/impact. If it is
  the same behavior, use its qualified reference everywhere; otherwise choose the correct
  symbol or remove the stale grounding/anchor.
- GROUNDING_GONE: update prose that still describes the deleted symbol. Remove obsolete
  `grounds_to` entries and anchors; if replacement behavior exists, ground the replacement.
- GROUNDING_UNVERIFIED: run `knobyte graph refresh` first; the check could not see the
  current code.

References are readable `kind:path:qualified_name` strings (e.g.
`function:src/auth.rs:validate_token`), in frontmatter `grounds_to:` lists or
`<!-- kb-ground: <ref> -->` anchors. Before finishing, re-read each changed file and
verify every reference resolves, none is duplicated, and unrelated prose is untouched."
        .to_string()
}

fn build_file_section(
    config: &KnobyteConfig,
    target: &SyncTarget,
    engine: Option<&GraphEngine>,
) -> String {
    let path = config.project_root.join(&target.file);
    let content =
        fs::read_to_string(&path).unwrap_or_else(|_| "(file could not be read)".to_string());
    let issue_list = target
        .issues
        .iter()
        .map(|i| {
            let loc = i.line.map(|l| format!(" (line {})", l)).unwrap_or_default();
            format!("- [{}] {}{}: {}", i.severity, i.code, loc, i.message)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut section = format!(
        "**File:** {}\n\n**Issues found:**\n{}\n\n**Current file content:**\n```markdown\n{}\n```",
        target.file, issue_list, content
    );

    if let Some(ctx) = build_filesystem_context(config, target) {
        section.push_str(&format!(
            "\n\n**Filesystem context (what actually exists):**\n{}",
            ctx
        ));
    }

    let claimed_paths: Vec<String> = target
        .issues
        .iter()
        .filter_map(|i| i.claim.as_ref())
        .filter(|c| c.kind == ClaimKind::Path)
        .map(|c| c.value.clone())
        .collect();
    if !claimed_paths.is_empty() {
        let diff = git_diff(&config.project_root, &claimed_paths);
        if !diff.is_empty() {
            section.push_str(&format!(
                "\n\n**Recent git changes in referenced paths:**\n```diff\n{}\n```",
                diff
            ));
        }
    }

    if let Some(engine) = engine {
        let grounding = build_grounding_context(config, target, engine);
        if !grounding.is_empty() {
            section.push_str(&format!(
                "\n\n**Grounded symbol scope (use this exact old/new body):**\n{}",
                grounding
            ));
        }
    }
    section
}

fn git_diff(root: &Path, paths: &[String]) -> String {
    let mut args = vec!["diff", "HEAD~5", "HEAD", "--"];
    args.extend(paths.iter().map(String::as_str));
    Command::new("git")
        .args(&args)
        .current_dir(root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
        .unwrap_or_default()
}

/// For MISSING_PATH issues, list what actually exists where the path was expected.
fn build_filesystem_context(config: &KnobyteConfig, target: &SyncTarget) -> Option<String> {
    let missing: Vec<&str> = target
        .issues
        .iter()
        .filter(|i| i.code == codes::MISSING_PATH)
        .filter_map(|i| i.claim.as_ref())
        .filter(|c| c.kind == ClaimKind::Path)
        .map(|c| c.value.as_str())
        .collect();
    if missing.is_empty() {
        return None;
    }
    let root = &config.project_root;
    let mut sections = Vec::new();
    let mut listed: BTreeSet<String> = BTreeSet::new();
    for value in missing {
        let dir = match value.trim_end_matches('/').rsplit_once('/') {
            Some((d, _)) if value.contains('/') => d.to_string(),
            _ => ".".to_string(),
        };
        if listed.insert(dir.clone()) {
            if let Ok(rd) = fs::read_dir(root.join(&dir)) {
                let mut names: Vec<String> = rd
                    .filter_map(|e| e.ok())
                    .filter_map(|e| e.file_name().to_str().map(str::to_string))
                    .filter(|n| !n.starts_with('.'))
                    .collect();
                names.sort();
                if !names.is_empty() {
                    sections.push(format!("`{}/` contains: {}", dir, names.join(", ")));
                }
            }
        }
        let name = value
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(value);
        if let Some((_, ext)) = name.rsplit_once('.') {
            let mut skip = vec!["node_modules", "dist", ".git", "target"];
            let scaffold_dir = config
                .scaffold_root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(".knobyte")
                .to_string();
            skip.push(&scaffold_dir);
            let suffix = format!(".{}", ext);
            let matches: Vec<String> = walk_index(root, 5, &skip)
                .into_iter()
                .filter(|e| !e.is_dir && e.rel.ends_with(&suffix))
                .map(|e| e.rel)
                .collect();
            if !matches.is_empty() && matches.len() <= 20 {
                sections.push(format!(
                    "All `.{}` files in project: {}",
                    ext,
                    matches.join(", ")
                ));
                break;
            }
        }
    }
    (!sections.is_empty()).then(|| sections.join("\n"))
}

/// Old (baseline) and new (current or candidate) bodies for each grounding issue.
fn build_grounding_context(
    config: &KnobyteConfig,
    target: &SyncTarget,
    engine: &GraphEngine,
) -> String {
    let conn = engine.connection();
    let scaffold_rel = config
        .project_root
        .join(&target.file)
        .strip_prefix(&config.scaffold_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| target.file.clone());
    let mut rows = Vec::new();
    for issue in target.issues.iter().filter(|i| i.is_grounding()) {
        let Some(reference) = issue.symbol.as_deref() else {
            continue;
        };
        let old_body = get_baseline(conn, &scaffold_rel, reference)
            .map(|b| b.source)
            .unwrap_or_else(|| "(no baseline recorded)".to_string());
        let current = resolve_grounding_ref(conn, reference)
            .ok()
            .and_then(|r| r.node().cloned())
            .or_else(|| {
                issue.candidate.as_deref().and_then(|c| {
                    resolve_grounding_ref(conn, c)
                        .ok()
                        .and_then(|r| r.node().cloned())
                })
            });
        let new_body = current
            .as_ref()
            .and_then(|n| read_node_source(&config.project_root, n))
            .unwrap_or_else(|| "(symbol not found in the code graph)".to_string());
        let header = match &issue.candidate {
            Some(c) => format!("Reference: {} (candidate: {})", reference, c),
            None => format!("Reference: {}", reference),
        };
        rows.push(format!(
            "{}\nOld body:\n```\n{}\n```\nNew body:\n```\n{}\n```",
            header, old_body, new_body
        ));
    }
    rows.join("\n\n")
}
