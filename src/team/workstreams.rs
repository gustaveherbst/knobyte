use std::fs;
use std::path::Path;
use serde::{Deserialize, Serialize};

use crate::config::KnobyteConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkstreamStep {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(default, rename = "filesTouched")]
    pub files_touched: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkstreamCheckpoint {
    #[serde(rename = "stepId")]
    pub step_id: String,
    pub status: String,
    #[serde(rename = "gitHead")]
    pub git_head: String,
    #[serde(rename = "dirtyFiles")]
    pub dirty_files: Vec<String>,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workstream {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default)]
    pub steps: Vec<WorkstreamStep>,
    #[serde(default)]
    pub checkpoints: Vec<WorkstreamCheckpoint>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

pub fn get_git_state(project_root: &Path) -> (String, Vec<String>) {
    let head = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let dirty_files = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.len() > 3)
                .map(|l| l[3..].trim().to_string())
                .collect()
        })
        .unwrap_or_default();

    (head, dirty_files)
}

pub fn list_workstreams(config: &KnobyteConfig) -> Vec<Workstream> {
    let dir = config.workstreams_dir();
    if !dir.exists() {
        return Vec::new();
    }

    let mut list = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(w) = serde_json::from_str::<Workstream>(&content) {
                        list.push(w);
                    }
                }
            }
        }
    }

    list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    list
}

pub fn get_workstream(config: &KnobyteConfig, id: &str) -> Option<Workstream> {
    let path = config.workstreams_dir().join(format!("{}.json", id));
    if path.exists() {
        if let Ok(content) = fs::read_to_string(path) {
            return serde_json::from_str::<Workstream>(&content).ok();
        }
    }
    None
}

pub fn save_workstream(config: &KnobyteConfig, workstream: &Workstream) -> Result<(), String> {
    let dir = config.workstreams_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}.json", workstream.id));
    let content = serde_json::to_string_pretty(workstream).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn update_workstream_step(
    config: &KnobyteConfig,
    workstream_id: &str,
    step_id: &str,
    status: &str,
    evidence: Option<&str>,
    files_touched: Option<&[String]>,
) -> Result<Workstream, String> {
    let mut ws = get_workstream(config, workstream_id)
        .ok_or_else(|| format!("Workstream '{}' not found", workstream_id))?;

    let (git_head, dirty_files) = get_git_state(&config.project_root);
    let now = chrono::Utc::now().to_rfc3339();

    if let Some(step) = ws.steps.iter_mut().find(|s| s.id == step_id) {
        step.status = status.to_string();
        if let Some(ev) = evidence {
            step.evidence = Some(ev.to_string());
        }
        if let Some(files) = files_touched {
            for f in files {
                if !step.files_touched.contains(f) {
                    step.files_touched.push(f.clone());
                }
            }
        }
        step.updated_at = now.clone();
    } else {
        ws.steps.push(WorkstreamStep {
            id: step_id.to_string(),
            title: format!("Step {}", step_id),
            status: status.to_string(),
            files_touched: files_touched.map(|f| f.to_vec()).unwrap_or_default(),
            evidence: evidence.map(|e| e.to_string()),
            updated_at: now.clone(),
        });
    }

    ws.checkpoints.push(WorkstreamCheckpoint {
        step_id: step_id.to_string(),
        status: status.to_string(),
        git_head,
        dirty_files,
        timestamp: now.clone(),
    });

    ws.updated_at = now;
    save_workstream(config, &ws)?;
    Ok(ws)
}
