use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::{HeartbeatSettings, KnobyteConfig};
use crate::drift::checkers::staleness::days_since_frontmatter_date;
use crate::drift::markdown::parse_frontmatter;
use crate::team::workstreams::{get_git_state, list_workstreams};

/// Scaffold sub-directories holding content documents that can go stale.
pub const CONTENT_DIRS: &[&str] = &["context", "topics", "patterns", "specs"];

/// Top-level scaffold documents checked for staleness alongside [`CONTENT_DIRS`].
pub const CONTENT_ROOT_FILES: &[&str] = &["ROUTER.md", "AGENTS.md", "HEARTBEAT.md"];

/// Minimum age before a temporary file is considered orphaned and safe to remove.
pub const TEMP_FILE_MIN_AGE: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleFileInfo {
    pub path: String,
    pub age_days: u64,
    /// `last_updated` (frontmatter date) or `mtime` (files without a parseable date).
    #[serde(default = "default_stale_source")]
    pub source: String,
}

fn default_stale_source() -> String {
    "last_updated".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupCandidate {
    pub path: String,
    pub reason: String,
    pub age_minutes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatReport {
    /// Everything is clear; always equal to `heartbeatOk`. `false` whenever
    /// anything is reported: stale files, memory cleanup due, old daily memory files, a
    /// lost-progress warning, or a missing scaffold.
    pub ok: bool,
    /// Knobyte-only: no lost-progress risk. The scaffold exists and there is no
    /// uncommitted-steps warning (done workstream steps with a dirty working tree). Stale
    /// documents and memory housekeeping do not clear this; `ok` covers everything. Serialized
    /// as `progressSafe` (`healthy` alongside `ok: false` read as a contradiction).
    #[serde(default, rename = "progressSafe", alias = "healthy")]
    pub healthy: bool,
    pub scaffold_exists: bool,
    /// Content documents (context/topics/patterns/specs) not modified within the
    /// threshold. Informational only: they do not make the heartbeat unhealthy.
    pub stale_files: Vec<StaleFileInfo>,
    /// Knobyte-only housekeeping state, serialized as `maintenanceStatus`: `lost_progress_risk`,
    /// `temp_files_removable`, `temp_files_removed`, `clear` or `scaffold_missing`. It never
    /// says "healthy": stale files and memory cleanup are reported by `ok` and their own fields.
    #[serde(rename = "maintenanceStatus", alias = "memory_cleanup_status")]
    pub memory_cleanup_status: String,
    #[serde(default, rename = "dirtyFilesCount")]
    pub dirty_files_count: usize,
    #[serde(default, rename = "uncommittedStepsWarning", skip_serializing_if = "Option::is_none")]
    pub uncommitted_steps_warning: Option<String>,
    /// Orphaned temporary files / stale locks that can be removed.
    #[serde(default, rename = "cleanupCandidates")]
    pub cleanup_candidates: Vec<CleanupCandidate>,
    /// Files actually removed (only when cleanup was requested).
    #[serde(default, rename = "cleaned")]
    pub cleaned: Vec<String>,
    #[serde(default, rename = "cleanupApplied")]
    pub cleanup_applied: bool,
    /// Everything clear: no stale scaffold files, no memory cleanup due, no old daily memory
    /// files and no uncommitted-progress warning. The CLI prints `HEARTBEAT_OK` exactly then.
    /// Identical to `ok`; kept for existing consumers.
    #[serde(default, rename = "heartbeatOk")]
    pub heartbeat_ok: bool,
    /// Days after which a scaffold file counts as stale (`heartbeat.staleDays`, default 7).
    #[serde(default, rename = "staleDays")]
    pub stale_days: u64,
    /// `memory/.last-cleanup.json` is older than `heartbeat.memoryCleanupDays`.
    #[serde(default, rename = "memoryCleanupDue")]
    pub memory_cleanup_due: bool,
    /// Daily memory files (`memory/YYYY-MM-DD.md`) older than `heartbeat.dailyMemoryRetentionDays`.
    #[serde(default, rename = "oldDailyMemoryFiles")]
    pub old_daily_memory_files: Vec<String>,
    /// Scanned scaffold files without a parseable `last_updated` (their age falls back to the
    /// file modification time).
    #[serde(default, rename = "filesWithoutLastUpdated")]
    pub files_without_last_updated: usize,
}

fn rel(scaffold_root: &Path, path: &Path) -> String {
    path.strip_prefix(scaffold_root)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().to_string())
}

fn file_age(path: &Path, now: SystemTime) -> Option<Duration> {
    let modified = path.metadata().ok()?.modified().ok()?;
    now.duration_since(modified).ok()
}

/// Scaffold documents checked for staleness: the root documents plus every Markdown file
/// under [`CONTENT_DIRS`].
fn content_files(scaffold_root: &Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = CONTENT_ROOT_FILES
        .iter()
        .map(|f| scaffold_root.join(f))
        .filter(|p| p.is_file())
        .collect();
    for dir in CONTENT_DIRS {
        let base = scaffold_root.join(dir);
        if !base.is_dir() {
            continue;
        }
        for entry in WalkDir::new(&base).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // The pattern index and format guide are Knobyte-owned, not dated content.
            if *dir == "patterns" && (name == "INDEX.md" || name == "README.md") {
                continue;
            }
            if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("md") {
                files.push(path.to_path_buf());
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    files.retain(|f| seen.insert(f.canonicalize().unwrap_or_else(|_| f.clone())));
    files
}

/// Stale documents by `last_updated` (or modification time when a file has none). Returns
/// (stale files, files without a parseable `last_updated`).
fn find_stale_content(scaffold_root: &Path, stale_days: u64, now: SystemTime) -> (Vec<StaleFileInfo>, usize) {
    let today = chrono::DateTime::<chrono::Local>::from(now).date_naive();
    let mut stale = Vec::new();
    let mut without = 0;
    for path in content_files(scaffold_root) {
        let content = fs::read_to_string(&path).unwrap_or_default();
        let last_updated = parse_frontmatter(&content)
            .and_then(|fm| fm.get("last_updated").and_then(|v| v.as_str().map(str::to_string)));
        let (days, source) = match days_since_frontmatter_date(last_updated.as_deref(), today) {
            Some(d) => (Some(d.max(0) as u64), "last_updated"),
            None => {
                without += 1;
                (file_age(&path, now).map(|a| a.as_secs() / 86400), "mtime")
            }
        };
        if let Some(days) = days {
            if days > stale_days {
                stale.push(StaleFileInfo { path: rel(scaffold_root, &path), age_days: days, source: source.to_string() });
            }
        }
    }
    stale.sort_by(|a, b| b.age_days.cmp(&a.age_days).then(a.path.cmp(&b.path)));
    (stale, without)
}

fn days_since_iso(value: &str, now: SystemTime) -> Option<u64> {
    let today = chrono::DateTime::<chrono::Utc>::from(now).date_naive();
    let date = chrono::DateTime::parse_from_rfc3339(value)
        .map(|d| d.with_timezone(&chrono::Utc).date_naive())
        .or_else(|_| chrono::NaiveDate::parse_from_str(value.get(..10).unwrap_or(value), "%Y-%m-%d"))
        .ok()?;
    let days = (today - date).num_days();
    (days >= 0).then_some(days as u64)
}

/// `memory/.last-cleanup.json` (`{"lastCleanup": "<date>"}`) older than the threshold.
fn memory_cleanup_due(project_root: &Path, threshold_days: u64, now: SystemTime) -> bool {
    let Ok(raw) = fs::read_to_string(project_root.join("memory/.last-cleanup.json")) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|v| v.get("lastCleanup").and_then(|d| d.as_str()).map(str::to_string))
        .and_then(|d| days_since_iso(&d, now))
        .is_some_and(|days| days > threshold_days)
}

/// Daily memory files `memory/YYYY-MM-DD.md` older than the retention period.
fn old_daily_memory_files(project_root: &Path, retention_days: u64, now: SystemTime) -> Vec<String> {
    let Ok(rd) = fs::read_dir(project_root.join("memory")) else {
        return Vec::new();
    };
    let today = chrono::DateTime::<chrono::Local>::from(now).date_naive();
    let mut out: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|name| name.len() == 13 && name.ends_with(".md"))
        .filter(|name| {
            days_since_frontmatter_date(Some(&name[..10]), today).is_some_and(|d| d as u64 > retention_days)
        })
        .map(|name| format!("memory/{}", name))
        .collect();
    out.sort();
    out
}

/// Classify a file as a temporary artifact eligible for cleanup. Canonical
/// documents and data (`.md`, `.json`, `.jsonl`, databases) are never eligible.
fn temp_reason(scaffold_root: &Path, path: &Path) -> Option<&'static str> {
    let rel_path = path.strip_prefix(scaffold_root).ok()?;
    let first = rel_path.components().next()?.as_os_str().to_str()?;
    // Never touch derived database storage (sled/SQLite manage their own files).
    if first.starts_with("graph.db") || first.starts_with("wiki.db") || first.starts_with("cozo.db") {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    if name.ends_with(".tmp") || name.ends_with(".temp") || name.ends_with('~') {
        return Some("orphaned temporary file");
    }
    if first == "local" && (name.ends_with(".lock") || name.ends_with(".pid")) {
        return Some("stale lock file");
    }
    None
}

fn find_cleanup_candidates(scaffold_root: &Path, now: SystemTime) -> Vec<CleanupCandidate> {
    let mut out = Vec::new();
    for entry in WalkDir::new(scaffold_root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(reason) = temp_reason(scaffold_root, path) else {
            continue;
        };
        let Some(age) = file_age(path, now) else {
            continue;
        };
        if age < TEMP_FILE_MIN_AGE {
            continue;
        }
        out.push(CleanupCandidate {
            path: rel(scaffold_root, path),
            reason: reason.to_string(),
            age_minutes: age.as_secs() / 60,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Configured staleness threshold (`heartbeat.staleDays` in config.json, default 7).
pub fn configured_stale_days(config: &KnobyteConfig) -> u64 {
    HeartbeatSettings::load(&config.scaffold_root).stale_days
}

/// Read-only heartbeat (never deletes anything). Equivalent to
/// `run_heartbeat(config, stale_threshold_days, false)`.
pub fn check_heartbeat(config: &KnobyteConfig, stale_threshold_days: u64) -> HeartbeatReport {
    run_heartbeat(config, stale_threshold_days, false)
}

/// Run the heartbeat. Reports stale content documents (informational), git /
/// workstream consistency, and orphaned temporary files. When `clean` is true,
/// the temporary files are removed; otherwise they are only reported.
pub fn run_heartbeat(config: &KnobyteConfig, stale_threshold_days: u64, clean: bool) -> HeartbeatReport {
    let scaffold_root = &config.scaffold_root;
    if !scaffold_root.exists() {
        return HeartbeatReport {
            ok: false,
            healthy: false,
            scaffold_exists: false,
            stale_files: Vec::new(),
            memory_cleanup_status: "scaffold_missing".to_string(),
            dirty_files_count: 0,
            uncommitted_steps_warning: None,
            cleanup_candidates: Vec::new(),
            cleaned: Vec::new(),
            cleanup_applied: false,
            heartbeat_ok: false,
            stale_days: stale_threshold_days,
            memory_cleanup_due: false,
            old_daily_memory_files: Vec::new(),
            files_without_last_updated: 0,
        };
    }

    let now = SystemTime::now();
    let settings = HeartbeatSettings::load(scaffold_root);
    let (stale_files, files_without_last_updated) = find_stale_content(scaffold_root, stale_threshold_days, now);
    let memory_cleanup_due = memory_cleanup_due(&config.project_root, settings.memory_cleanup_days, now);
    let old_daily_memory_files =
        old_daily_memory_files(&config.project_root, settings.daily_memory_retention_days, now);
    let cleanup_candidates = find_cleanup_candidates(scaffold_root, now);

    let mut cleaned = Vec::new();
    if clean {
        for c in &cleanup_candidates {
            let path = scaffold_root.join(&c.path);
            // Re-check classification right before deleting.
            if temp_reason(scaffold_root, &path).is_some() && fs::remove_file(&path).is_ok() {
                cleaned.push(c.path.clone());
            }
        }
    }

    // Git and uncommitted workstream check
    let (_head, dirty_files) = get_git_state(&config.project_root);
    let dirty_files_count = dirty_files.len();

    let done_uncommitted_steps = if dirty_files_count > 0 {
        list_workstreams(config)
            .iter()
            .filter(|ws| ws.status == "active")
            .flat_map(|ws| ws.steps.iter())
            .filter(|s| s.status == "done")
            .count()
    } else {
        0
    };

    let uncommitted_steps_warning = if done_uncommitted_steps > 0 {
        Some(format!(
            "{} steps marked done, working tree still dirty ({} files uncommitted). Risk of lost progress!",
            done_uncommitted_steps, dirty_files_count
        ))
    } else {
        None
    };

    let healthy = uncommitted_steps_warning.is_none();
    let pending_cleanup = cleanup_candidates.len() - cleaned.len();
    let memory_cleanup_status = if uncommitted_steps_warning.is_some() {
        "lost_progress_risk"
    } else if pending_cleanup > 0 {
        "temp_files_removable"
    } else if clean && !cleaned.is_empty() {
        "temp_files_removed"
    } else {
        "clear"
    }
    .to_string();

    let heartbeat_ok = healthy && stale_files.is_empty() && !memory_cleanup_due && old_daily_memory_files.is_empty();
    HeartbeatReport {
        heartbeat_ok,
        stale_days: stale_threshold_days,
        memory_cleanup_due,
        old_daily_memory_files,
        files_without_last_updated,
        ok: heartbeat_ok,
        healthy,
        scaffold_exists: true,
        stale_files,
        memory_cleanup_status,
        dirty_files_count,
        uncommitted_steps_warning,
        cleanup_candidates,
        cleaned,
        cleanup_applied: clean,
    }
}
