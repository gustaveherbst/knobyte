//! `knobyte update`: refresh Knobyte-owned files without touching populated content.
//!
//! - Missing scaffold files are created from the current templates (existing ones are kept).
//! - Infrastructure files (`SETUP.md`, `SYNC.md`, `patterns/README.md`) hold no project content
//!   and are refreshed to the current template; a differing previous copy is saved under
//!   `.knobyte/local/update-backups/` first.
//! - Managed blocks and anchors of the saved `aiTools` are refreshed, and unmodified managed
//!   skills are upgraded (user-edited skills are reported, never overwritten).

use std::fs;

use serde::Serialize;

use crate::config::{load_ai_tools, KnobyteConfig};
use crate::setup::anchor::{ensure_tool_anchors, AnchorOutcome};
use crate::setup::templates::{render, templates_for_mode, TemplateVars, INFRASTRUCTURE_FILES};
use crate::setup::{apply_setup, resolve_setup_mode};
use crate::skills::{sync_agent_assets, SkillSyncOptions};

#[derive(Debug, Clone, Serialize)]
pub struct UpdateChange {
    /// Project-relative path.
    pub path: String,
    /// `create`, `refresh`, `update`, `backup` or `conflict`.
    pub action: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateReport {
    pub dry_run: bool,
    pub mode: String,
    pub changes: Vec<UpdateChange>,
    pub conflicts: usize,
}

fn rel(config: &KnobyteConfig, p: &std::path::Path) -> String {
    p.strip_prefix(&config.project_root)
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| p.to_string_lossy().to_string())
}

pub fn run_update(config: &KnobyteConfig, dry_run: bool) -> Result<UpdateReport, String> {
    if !config.scaffold_root.is_dir() {
        return Err("No Knobyte scaffold found. Run `knobyte setup` first.".into());
    }
    let mode = resolve_setup_mode(config, None)?;
    let mut changes = Vec::new();
    let mut conflicts = 0;

    // Infrastructure files first, so apply_setup does not report them as created.
    let vars = TemplateVars::new(config.project_name(), Vec::new());
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    for (path_rel, template) in templates_for_mode(&mode) {
        if !INFRASTRUCTURE_FILES.contains(&path_rel) {
            continue;
        }
        let path = config.scaffold_root.join(path_rel);
        let desired = render(template, &vars);
        match fs::read_to_string(&path) {
            Ok(current) if current == desired => {}
            Ok(current) => {
                let backup = config.local_dir().join("update-backups").join(&stamp).join(path_rel);
                if !dry_run {
                    if let Some(parent) = backup.parent() {
                        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    fs::write(&backup, current).map_err(|e| e.to_string())?;
                    fs::write(&path, &desired).map_err(|e| e.to_string())?;
                }
                changes.push(UpdateChange {
                    path: rel(config, &path),
                    action: "refresh".into(),
                    detail: format!("refreshed to the current template (previous copy saved to {})", rel(config, &backup)),
                });
            }
            Err(_) => {}
        }
    }

    // Missing scaffold files, directories, gitignore rules and config.
    let report = apply_setup(config, &mode, dry_run)?;
    for a in report.actions.into_iter().filter(|a| a.action != "create_dir") {
        changes.push(UpdateChange {
            path: rel(config, std::path::Path::new(&a.path)),
            action: if a.action == "modify_file" { "update".into() } else { "create".into() },
            detail: a.detail,
        });
    }

    // Anchors, managed blocks and skills for the saved tool selection.
    let tools = load_ai_tools(&config.scaffold_root).unwrap_or_default();
    for a in ensure_tool_anchors(&config.project_root, &tools, dry_run) {
        let action = match a.outcome {
            AnchorOutcome::AlreadyLinked => continue,
            AnchorOutcome::Created => "create",
            AnchorOutcome::Appended | AnchorOutcome::Updated => "update",
            AnchorOutcome::Conflict => {
                conflicts += 1;
                "conflict"
            }
        };
        changes.push(UpdateChange {
            path: a.path.clone(),
            action: action.into(),
            detail: a.reason.unwrap_or_else(|| format!("{} pointer", a.tool)),
        });
    }
    let clients: Vec<&str> = tools.iter().map(String::as_str).filter(|t| *t == "claude" || *t == "codex").collect();
    if !clients.is_empty() {
        let assets = sync_agent_assets(
            config,
            &clients,
            SkillSyncOptions { dry_run, check_ignored: false, backup_conflicts: false },
        )?;
        for a in assets.actions.into_iter().filter(|a| a.action != "unchanged") {
            if a.action == "conflict" {
                conflicts += 1;
            }
            changes.push(UpdateChange { path: rel(config, std::path::Path::new(&a.path)), action: a.action, detail: a.message });
        }
    }

    Ok(UpdateReport { dry_run, mode, changes, conflicts })
}
