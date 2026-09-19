use std::fs;
use std::path::PathBuf;
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

pub fn global_knobyte_dir() -> PathBuf {
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
    let path = registry_file_path();
    if !path.exists() {
        return Vec::new();
    }
    match fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str::<Vec<ProjectRegistryEntry>>(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn save_registry(entries: &[ProjectRegistryEntry]) -> std::io::Result<()> {
    let dir = global_knobyte_dir();
    fs::create_dir_all(&dir)?;
    let content = serde_json::to_string_pretty(entries)?;
    fs::write(registry_file_path(), content)
}

pub fn register_project(config: &KnobyteConfig) -> std::io::Result<()> {
    let mut entries = load_registry();
    let current_path = config.project_root.to_string_lossy().to_string();
    let scaffold_path = config.scaffold_root.to_string_lossy().to_string();
    let name = config.project_name();
    let now = Utc::now().to_rfc3339();

    if let Some(existing) = entries.iter_mut().find(|e| e.path == current_path) {
        existing.name = name;
        existing.scaffold_root = scaffold_path;
        existing.mode = config.mode.clone();
        existing.last_active = now;
    } else {
        entries.push(ProjectRegistryEntry {
            name,
            path: current_path,
            scaffold_root: scaffold_path,
            mode: config.mode.clone(),
            last_active: now,
        });
    }

    save_registry(&entries)
}

pub fn discover_projects(current_config: &KnobyteConfig) -> Vec<ProjectInfo> {
    // Register current project first
    let _ = register_project(current_config);

    let mut entries = load_registry();

    // Check sibling directories for any other .knobyte folders
    if let Some(parent) = current_config.project_root.parent() {
        if let Ok(dir_entries) = fs::read_dir(parent) {
            for entry in dir_entries.flatten() {
                let candidate_path = entry.path();
                if candidate_path.is_dir() && candidate_path.join(".knobyte").join("config.json").exists() {
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
                            last_active: Utc::now().to_rfc3339(),
                        });
                    }
                }
            }
        }
    }

    let current_path_str = current_config.project_root.to_string_lossy().to_string();
    let mut projects = Vec::new();

    for entry in entries {
        let is_current = entry.path == current_path_str;
        if is_current {
            let drift = run_drift_check(current_config);
            let members = list_members(current_config);
            let graph_status = GraphEngine::open(&current_config.graph_db_path()).ok().and_then(|g| g.status().ok());
            let wiki_count = WikiIndex::open(&current_config.wiki_db_path()).ok().and_then(|w| w.list().ok()).map(|l| l.len()).unwrap_or(0);

            projects.push(ProjectInfo {
                name: entry.name,
                path: entry.path,
                scaffold_root: entry.scaffold_root,
                mode: current_config.mode.clone(),
                drift_score: drift.score,
                file_count: drift.file_count,
                node_count: graph_status.as_ref().map(|s| s.node_count as usize).unwrap_or(0),
                edge_count: graph_status.as_ref().map(|s| s.edge_count as usize).unwrap_or(0),
                wiki_count,
                members: members.into_iter().map(|m| m.display_name).collect(),
                last_active: entry.last_active,
                is_current: true,
            });
        } else {
            let p_buf = PathBuf::from(&entry.path);
            let s_buf = PathBuf::from(&entry.scaffold_root);
            if s_buf.exists() {
                let cfg = KnobyteConfig::new(p_buf, s_buf);
                let drift = run_drift_check(&cfg);
                let members = list_members(&cfg);
                let graph_status = GraphEngine::open(&cfg.graph_db_path()).ok().and_then(|g| g.status().ok());
                let wiki_count = WikiIndex::open(&cfg.wiki_db_path()).ok().and_then(|w| w.list().ok()).map(|l| l.len()).unwrap_or(0);

                projects.push(ProjectInfo {
                    name: entry.name,
                    path: entry.path,
                    scaffold_root: entry.scaffold_root,
                    mode: cfg.mode.clone(),
                    drift_score: drift.score,
                    file_count: drift.file_count,
                    node_count: graph_status.as_ref().map(|s| s.node_count as usize).unwrap_or(0),
                    edge_count: graph_status.as_ref().map(|s| s.edge_count as usize).unwrap_or(0),
                    wiki_count,
                    members: members.into_iter().map(|m| m.display_name).collect(),
                    last_active: entry.last_active,
                    is_current: false,
                });
            } else {
                projects.push(ProjectInfo {
                    name: entry.name,
                    path: entry.path,
                    scaffold_root: entry.scaffold_root,
                    mode: entry.mode,
                    drift_score: 100.0,
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
    }

    projects.sort_by(|a, b| b.is_current.cmp(&a.is_current).then_with(|| a.name.cmp(&b.name)));
    projects
}

pub fn aggregate_contributors(_projects: &[ProjectInfo], current_config: &KnobyteConfig) -> Vec<ContributorImpact> {
    let mut contributors = Vec::new();

    // Map each project's members and events
    let current_members = list_members(current_config);
    let current_relays = list_relays(current_config);
    let current_events = read_events_from_path(&current_config.decisions_log_path());

    for member in &current_members {
        let mut member_events = Vec::new();
        let mut decisions_count = 0;
        let mut discoveries_count = 0;
        let mut notes_count = 0;
        let mut files_set = std::collections::HashSet::new();

        // Check matching events by actor id or display name
        for ev in &current_events {
            let is_match = ev.actor.as_deref() == Some(&member.id)
                || ev.actor.as_deref() == Some(&member.display_name)
                || ev.actor.as_deref() == member.git_aliases.first().and_then(|g| g.name.as_deref());

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
                    project: current_config.project_name(),
                });
            }
        }

        // Relays authored or claimed
        let relays_authored = current_relays.iter().filter(|r| r.sender == member.id || r.sender == member.display_name).count();
        let in_flight_relay = current_relays.iter()
            .find(|r| r.claimant.as_deref() == Some(&member.id) || r.claimant.as_deref() == Some(&member.display_name))
            .map(|r| r.title.clone());

        let git_alias = member.git_aliases.first().and_then(|g| g.name.clone()).unwrap_or_else(|| member.id.clone());

        let touched_projects = vec![current_config.project_name()];

        member_events.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

        contributors.push(ContributorImpact {
            id: member.id.clone(),
            display_name: member.display_name.clone(),
            status: member.status.clone(),
            git_alias,
            projects: touched_projects,
            decisions_count,
            discoveries_count,
            notes_count,
            relays_authored,
            in_flight_relay,
            files_touched_count: files_set.len(),
            recent_events: member_events,
        });
    }

    // If no explicit members exist, add a default actor from recent events if any
    if contributors.is_empty() {
        let mut default_events = Vec::new();
        let mut d_count = 0;
        let mut disc_count = 0;
        let mut n_count = 0;
        let mut files_set = std::collections::HashSet::new();

        for ev in &current_events {
            match ev.kind.as_str() {
                "decision" => d_count += 1,
                "discovery" => disc_count += 1,
                _ => n_count += 1,
            }
            for f in &ev.files {
                files_set.insert(f.clone());
            }
            default_events.push(ContributorEvent {
                timestamp: ev.timestamp.clone(),
                kind: ev.kind.clone(),
                summary: ev.summary.clone(),
                files: ev.files.clone(),
                project: current_config.project_name(),
            });
        }

        contributors.push(ContributorImpact {
            id: "contributor".to_string(),
            display_name: "Active Contributor".to_string(),
            status: "active".to_string(),
            git_alias: "contributor".to_string(),
            projects: vec![current_config.project_name()],
            decisions_count: d_count,
            discoveries_count: disc_count,
            notes_count: n_count,
            relays_authored: 0,
            in_flight_relay: None,
            files_touched_count: files_set.len(),
            recent_events: default_events,
        });
    }

    contributors
}
