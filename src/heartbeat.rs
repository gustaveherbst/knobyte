use std::time::{Duration, SystemTime};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::KnobyteConfig;
use crate::team::workstreams::{get_git_state, list_workstreams};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleFileInfo {
    pub path: String,
    pub age_days: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatReport {
    pub ok: bool,
    pub scaffold_exists: bool,
    pub stale_files: Vec<StaleFileInfo>,
    pub memory_cleanup_status: String,
    #[serde(default, rename = "dirtyFilesCount")]
    pub dirty_files_count: usize,
    #[serde(default, rename = "uncommittedStepsWarning", skip_serializing_if = "Option::is_none")]
    pub uncommitted_steps_warning: Option<String>,
}

pub fn check_heartbeat(config: &KnobyteConfig, stale_threshold_days: u64) -> HeartbeatReport {
    let scaffold_root = &config.scaffold_root;
    let scaffold_exists = scaffold_root.exists();

    if !scaffold_exists {
        return HeartbeatReport {
            ok: false,
            scaffold_exists: false,
            stale_files: Vec::new(),
            memory_cleanup_status: "scaffold_missing".to_string(),
            dirty_files_count: 0,
            uncommitted_steps_warning: None,
        };
    }

    let now = SystemTime::now();
    let threshold = Duration::from_secs(stale_threshold_days * 86400);

    let mut stale_files = Vec::new();

    for entry in WalkDir::new(scaffold_root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        if let Ok(metadata) = path.metadata() {
            if let Ok(modified) = metadata.modified() {
                if let Ok(age) = now.duration_since(modified) {
                    if age > threshold {
                        let rel_path = match path.strip_prefix(scaffold_root) {
                            Ok(p) => p.to_string_lossy().to_string(),
                            Err(_) => path.to_string_lossy().to_string(),
                        };
                        stale_files.push(StaleFileInfo {
                            path: rel_path,
                            age_days: age.as_secs() / 86400,
                        });
                    }
                }
            }
        }
    }

    // Git and uncommitted workstream check
    let (_head, dirty_files) = get_git_state(&config.project_root);
    let dirty_files_count = dirty_files.len();

    let workstreams = list_workstreams(config);
    let mut done_uncommitted_steps = 0;
    for ws in &workstreams {
        for step in &ws.steps {
            if step.status == "done" && dirty_files_count > 0 {
                done_uncommitted_steps += 1;
            }
        }
    }

    let uncommitted_steps_warning = if done_uncommitted_steps > 0 {
        Some(format!(
            "{} steps marked done, working tree still dirty ({} files uncommitted). Risk of lost progress!",
            done_uncommitted_steps, dirty_files_count
        ))
    } else {
        None
    };

    let ok = stale_files.is_empty() && uncommitted_steps_warning.is_none();
    let memory_cleanup_status = if ok {
        "healthy".to_string()
    } else if let Some(ref warn) = uncommitted_steps_warning {
        warn.clone()
    } else {
        format!("{} stale files require attention", stale_files.len())
    };

    HeartbeatReport {
        ok,
        scaffold_exists,
        stale_files,
        memory_cleanup_status,
        dirty_files_count,
        uncommitted_steps_warning,
    }
}
