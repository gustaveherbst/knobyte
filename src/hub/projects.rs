use std::fs;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use chrono::Utc;

use crate::config::KnobyteConfig;
use crate::drift::checker::run_drift_check;
use crate::events::read_events_from_path;
use crate::graph::GraphEngine;
use crate::team::members::list_members;
use crate::team::relay::list_relays;
use crate::wiki::WikiIndex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRegistryEntry {
    pub name: String,
    pub path: String,
    pub scaffold_root: String,
    pub mode: String,
    #[serde(default)]
    pub last_active: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub name: String,
    pub path: String,
    pub scaffold_root: String,
    pub mode: String,
    /// False when the registered project directory or scaffold no longer exists.
    pub available: bool,
    /// `healthy`, `warning`, `drifted` or `unavailable`.
    pub status: String,
    pub drift_score: f64,
    pub file_count: usize,
    pub node_count: usize,
    pub edge_count: usize,
    pub wiki_count: usize,
    pub members: Vec<String>,
    pub last_active: String,
    pub is_current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContributorEvent {
    pub timestamp: String,
    pub kind: String,
    pub summary: String,
    pub files: Vec<String>,
    pub project: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContributorImpact {
    pub id: String,
    pub display_name: String,
    pub status: String,
    pub git_alias: String,
    pub projects: Vec<String>,
    pub decisions_count: usize,
    pub discoveries_count: usize,
    pub notes_count: usize,
    pub relays_authored: usize,
    pub in_flight_relay: Option<String>,
    pub files_touched_count: usize,
    pub recent_events: Vec<ContributorEvent>,
}

/// Per-user Knobyte directory (`~/.knobyte`). `KNOBYTE_HOME` overrides it, which
/// keeps tests and sandboxed runs out of the real user registry.
pub fn global_knobyte_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("KNOBYTE_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        PathBuf::from(home).join(".knobyte")
    } else {
        PathBuf::from(".knobyte")
    }
}

pub fn registry_file_path() -> PathBuf {
    global_knobyte_dir().join("projects.json")
}

pub fn load_registry() -> Vec<ProjectRegistryEntry> {
    load_registry_at(&registry_file_path())
}

/// Load the registry from an explicit path (used by the Hub and tests).
pub fn load_registry_at(path: &Path) -> Vec<ProjectRegistryEntry> {
    match fs::read_to_string(path) {
        Ok(content) => serde_json::from_str::<Vec<ProjectRegistryEntry>>(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save_registry(entries: &[ProjectRegistryEntry]) -> std::io::Result<()> {
    save_registry_at(&registry_file_path(), entries)
}

pub fn save_registry_at(path: &Path, entries: &[ProjectRegistryEntry]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let content = serde_json::to_string_pretty(entries)?;
    // Write via a temp file + rename so a concurrent reader never sees a truncated file.
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)
}

/// Register the current project in `~/.knobyte/projects.json`.
/// The file is only written when the entry is new or its metadata changed.
pub fn register_project(config: &KnobyteConfig) -> std::io::Result<bool> {
    register_project_at(&registry_file_path(), config, false)
}

/// Register the current project in the registry at `path`.
///
/// Returns whether the file was written. With `touch`, `last_active` is
/// refreshed (one write, e.g. on Hub start); otherwise the registry is only
/// rewritten when the entry is missing or its name, scaffold root or mode changed.
pub fn register_project_at(path: &Path, config: &KnobyteConfig, touch: bool) -> std::io::Result<bool> {
    let mut entries = load_registry_at(path);
    let current_path = config.project_root.to_string_lossy().to_string();
    let scaffold_path = config.scaffold_root.to_string_lossy().to_string();
    let name = config.project_name();
    let now = Utc::now().to_rfc3339();

    let changed = if let Some(existing) = entries.iter_mut().find(|e| e.path == current_path) {
        let meta_changed = existing.name != name
            || existing.scaffold_root != scaffold_path
            || existing.mode != config.mode;
        if meta_changed || touch || existing.last_active.is_empty() {
            existing.name = name;
            existing.scaffold_root = scaffold_path;
            existing.mode = config.mode.clone();
            existing.last_active = now;
            true
        } else {
            false
        }
    } else {
        entries.push(ProjectRegistryEntry {
            name,
            path: current_path,
            scaffold_root: scaffold_path,
            mode: config.mode.clone(),
            last_active: now,
        });
        true
    };

    if changed {
        save_registry_at(path, &entries)?;
    }
    Ok(changed)
}

fn file_mtime_rfc3339(path: &Path) -> String {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| chrono::DateTime::<Utc>::from(t).to_rfc3339())
        .unwrap_or_default()
}

/// Classify a drift score into the Fleet health buckets.
pub fn health_status(score: f64) -> &'static str {
    if score >= 95.0 {
        "healthy"
    } else if score >= 80.0 {
        "warning"
    } else {
        "drifted"
    }
}

fn project_info_for(entry: ProjectRegistryEntry, cfg: &KnobyteConfig, is_current: bool) -> ProjectInfo {
    let drift = run_drift_check(cfg);
    let members = list_members(cfg);
    let graph_path = cfg.graph_db_path();
    let graph_status = if graph_path.exists() {
        GraphEngine::open(&graph_path).ok().and_then(|g| g.status().ok())
    } else {
        None
    };
    let wiki_path = cfg.wiki_db_path();
    let wiki_count = if wiki_path.exists() {
        WikiIndex::open_read_only(&wiki_path).ok().and_then(|w| w.entity_count().ok()).unwrap_or(0)
    } else {
        0
    };
    ProjectInfo {
        name: entry.name,
        path: entry.path,
        scaffold_root: entry.scaffold_root,
        mode: cfg.mode.clone(),
        available: true,
        status: health_status(drift.score).to_string(),
        drift_score: drift.score,
        file_count: drift.file_count,
        node_count: graph_status.as_ref().map(|s| s.node_count as usize).unwrap_or(0),
        edge_count: graph_status.as_ref().map(|s| s.edge_count as usize).unwrap_or(0),
        wiki_count,
        members: members.into_iter().map(|m| m.display_name).collect(),
        last_active: entry.last_active,
        is_current,
    }
}

pub fn discover_projects(current_config: &KnobyteConfig) -> Vec<ProjectInfo> {
    discover_projects_at(&registry_file_path(), current_config)
}

/// Discover registered (and sibling) projects using the registry at `registry_path`.
/// Projects whose directory or scaffold no longer exists are reported with
/// `available: false` / `status: "unavailable"` rather than as healthy.
pub fn discover_projects_at(registry_path: &Path, current_config: &KnobyteConfig) -> Vec<ProjectInfo> {
    let _ = register_project_at(registry_path, current_config, false);

    let mut entries = load_registry_at(registry_path);

    // Check sibling directories for any other .knobyte folders (not persisted).
    if let Some(parent) = current_config.project_root.parent() {
        if let Ok(dir_entries) = fs::read_dir(parent) {
            for entry in dir_entries.flatten() {
                let candidate_path = entry.path();
                let cfg_file = candidate_path.join(".knobyte").join("config.json");
                if candidate_path.is_dir() && cfg_file.exists() {
                    let path_str = candidate_path.to_string_lossy().to_string();
                    if !entries.iter().any(|e| e.path == path_str) {
                        let scaffold = candidate_path.join(".knobyte");
                        let cfg = KnobyteConfig::new(candidate_path.clone(), scaffold.clone());
                        let name = cfg.project_name();
                        entries.push(ProjectRegistryEntry {
                            name,
                            path: path_str,
                            scaffold_root: scaffold.to_string_lossy().to_string(),
                            mode: cfg.mode,
                            last_active: file_mtime_rfc3339(&cfg_file),
                        });
                    }
                }
            }
        }
    }

    let current_path_str = current_config.project_root.to_string_lossy().to_string();
    let mut projects = Vec::new();

    for entry in entries {
        if entry.path == current_path_str {
            projects.push(project_info_for(entry, current_config, true));
            continue;
        }
        let p_buf = PathBuf::from(&entry.path);
        let s_buf = PathBuf::from(&entry.scaffold_root);
        if p_buf.is_dir() && s_buf.is_dir() {
            let cfg = KnobyteConfig::new(p_buf, s_buf);
            projects.push(project_info_for(entry, &cfg, false));
        } else {
            projects.push(ProjectInfo {
                name: entry.name,
                path: entry.path,
                scaffold_root: entry.scaffold_root,
                mode: entry.mode,
                available: false,
                status: "unavailable".to_string(),
                drift_score: 0.0,
                file_count: 0,
                node_count: 0,
                edge_count: 0,
                wiki_count: 0,
                members: Vec::new(),
                last_active: entry.last_active,
                is_current: false,
            });
        }
    }

    projects.sort_by(|a, b| b.is_current.cmp(&a.is_current).then_with(|| a.name.cmp(&b.name)));
    projects
}

/// Per-member impact for the current project. Only registered team members are
/// reported; when there are none the list is empty (the Hub shows a
/// `knobyte member add` hint instead of inventing a contributor).
pub fn aggregate_contributors(_projects: &[ProjectInfo], current_config: &KnobyteConfig) -> Vec<ContributorImpact> {
    let mut contributors = Vec::new();

    let current_members = list_members(current_config);
    let current_relays = list_relays(current_config);
    let current_events = read_events_from_path(&current_config.decisions_log_path());
    let project_name = current_config.project_name();

    for member in &current_members {
        let mut member_events = Vec::new();
        let mut decisions_count = 0;
        let mut discoveries_count = 0;
        let mut notes_count = 0;
        let mut files_set = std::collections::HashSet::new();

        let alias_names: Vec<&str> = member.git_aliases.iter().filter_map(|g| g.name.as_deref()).collect();

        for ev in &current_events {
            let actor = match ev.actor.as_deref() {
                Some(a) => a,
                None => continue,
            };
            let is_match = actor == member.id || actor == member.display_name || alias_names.contains(&actor);

            if is_match {
                match ev.kind.as_str() {
                    "decision" => decisions_count += 1,
                    "discovery" => discoveries_count += 1,
                    _ => notes_count += 1,
                }
                for f in &ev.files {
                    files_set.insert(f.clone());
                }
                member_events.push(ContributorEvent {
                    timestamp: ev.timestamp.clone(),
                    kind: ev.kind.clone(),
                    summary: ev.summary.clone(),
                    files: ev.files.clone(),
                    project: project_name.clone(),
                });
            }
        }

        let relays_authored = current_relays
            .iter()
            .filter(|r| r.sender == member.id || r.sender == member.display_name)
            .count();
        // Only relays that are still in flight (claimed and not closed).
        let in_flight_relay = current_relays
            .iter()
            .filter(|r| r.status != "closed")
            .find(|r| r.claimant.as_deref() == Some(&member.id) || r.claimant.as_deref() == Some(&member.display_name))
            .map(|r| r.title.clone());

        let git_alias = member.git_aliases.first().and_then(|g| g.name.clone()).unwrap_or_else(|| member.id.clone());

        member_events.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        member_events.truncate(100);

        contributors.push(ContributorImpact {
            id: member.id.clone(),
            display_name: member.display_name.clone(),
            status: member.status.clone(),
            git_alias,
            projects: vec![project_name.clone()],
            decisions_count,
            discoveries_count,
            notes_count,
            relays_authored,
            in_flight_relay,
            files_touched_count: files_set.len(),
            recent_events: member_events,
        });
    }

    contributors
}
