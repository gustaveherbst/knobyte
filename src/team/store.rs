//! Crash-safe storage primitives for team state.
//!
//! * content revisions (`sha256:<hex>` of exact file bytes),
//! * atomic writes (temp file in the same directory + fsync + rename),
//! * an exclusive cross-process lock file for team mutations,
//! * an intent -> complete journal so an interrupted multi-file apply can be
//!   rolled forward on the next mutation.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::KnobyteConfig;

/// Maximum number of completed journal entries retained for exact replay.
pub const JOURNAL_WINDOW: usize = 256;
/// A lock older than this is considered abandoned by a crashed process.
const STALE_LOCK_AGE: Duration = Duration::from_secs(120);
const LOCK_WAIT: Duration = Duration::from_secs(10);

/// Exact content revision of some bytes.
pub fn revision_of(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("sha256:{}", hex::encode(h.finalize()))
}

/// Revision of a file on disk, `None` when it does not exist.
pub fn file_revision(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|b| revision_of(&b))
}

/// Short hash used inside cursors and ids.
pub fn short_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    hex::encode(h.finalize())[..16].to_string()
}

/// Write `bytes` to `path` atomically: write a sibling temp file, fsync it, then rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent directory"))?;
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let tmp = dir.join(format!(".{}.{}.tmp", name, uuid::Uuid::new_v4().simple()));
    {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Remove a file if present (no error when missing).
pub fn remove_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Serialize a value as pretty JSON with a trailing newline and write it atomically.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = json_bytes(value)?;
    atomic_write(path, &bytes).map_err(|e| format!("Failed to write {}: {}", path.display(), e))
}

/// Canonical on-disk JSON bytes for a stored team record.
pub fn json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let mut s = serde_json::to_string_pretty(value).map_err(|e| e.to_string())?;
    s.push('\n');
    Ok(s.into_bytes())
}

/// Canonical (sorted-key, compact) JSON text, independent of serde_json map ordering features.
pub fn canonical_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                write_canonical(&map[*k], out);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(v, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// Revision of a serializable value (hash of its canonical JSON).
pub fn value_revision<T: Serialize>(value: &T) -> String {
    let v = serde_json::to_value(value).unwrap_or(serde_json::Value::Null);
    revision_of(canonical_json(&v).as_bytes())
}

// ---------------------------------------------------------------------------
// Lock
// ---------------------------------------------------------------------------

pub fn lock_path(config: &KnobyteConfig) -> PathBuf {
    config.local_dir().join("team.lock")
}

/// Exclusive team mutation lock. Released on drop.
pub struct TeamLock {
    path: PathBuf,
    token: String,
}

impl TeamLock {
    /// Acquire the lock, waiting up to a few seconds and breaking locks abandoned
    /// by a crashed process.
    pub fn acquire(config: &KnobyteConfig) -> Result<TeamLock, String> {
        Self::acquire_with_timeout(config, LOCK_WAIT)
    }

    pub fn acquire_with_timeout(config: &KnobyteConfig, wait: Duration) -> Result<TeamLock, String> {
        let path = lock_path(config);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let token = format!(
            "{{\"pid\":{},\"token\":\"{}\",\"acquiredAt\":\"{}\"}}\n",
            std::process::id(),
            uuid::Uuid::new_v4(),
            chrono::Utc::now().to_rfc3339()
        );
        let start = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    f.write_all(token.as_bytes()).map_err(|e| e.to_string())?;
                    let _ = f.sync_all();
                    return Ok(TeamLock { path, token });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    if start.elapsed() >= wait {
                        return Err(format!(
                            "Another team operation holds {} ; retry shortly (delete the file only if no knobyte process is running)",
                            path.display()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(e) => return Err(format!("Failed to create lock {}: {}", path.display(), e)),
            }
        }
    }
}

fn lock_is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map(|age| age > STALE_LOCK_AGE)
        .unwrap_or(false)
}

impl Drop for TeamLock {
    fn drop(&mut self) {
        // Only remove the lock if it is still ours.
        if fs::read_to_string(&self.path).map(|c| c == self.token).unwrap_or(false) {
            let _ = fs::remove_file(&self.path);
        }
    }
}

// ---------------------------------------------------------------------------
// Journal
// ---------------------------------------------------------------------------

pub const JOURNAL_INTENT: &str = "intent";
pub const JOURNAL_COMPLETE: &str = "complete";
pub const JOURNAL_CONFLICTED: &str = "conflicted";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalWrite {
    /// Path relative to the scaffold root.
    pub path: String,
    #[serde(rename = "beforeRevision")]
    pub before_revision: Option<String>,
    #[serde(rename = "afterRevision")]
    pub after_revision: Option<String>,
    /// Exact UTF-8 content to write (`None` = delete). Dropped once complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalAppend {
    pub path: String,
    pub line: String,
    /// Identifier contained in the line, used to avoid duplicate appends on recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "operationId")]
    pub operation_id: String,
    pub command: String,
    #[serde(rename = "previewRevision")]
    pub preview_revision: String,
    pub state: String,
    #[serde(rename = "startedAt")]
    pub started_at: String,
    #[serde(default, rename = "completedAt", skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    pub writes: Vec<JournalWrite>,
    #[serde(default)]
    pub appends: Vec<JournalAppend>,
    /// Result payload returned for an exact replay.
    #[serde(default)]
    pub result: serde_json::Value,
}

pub fn journal_dir(config: &KnobyteConfig) -> PathBuf {
    config.local_dir().join("team-journal")
}

fn journal_path(config: &KnobyteConfig, operation_id: &str) -> PathBuf {
    journal_dir(config).join(format!("{}.json", operation_id))
}

pub fn read_journal(config: &KnobyteConfig, operation_id: &str) -> Option<JournalEntry> {
    let content = fs::read_to_string(journal_path(config, operation_id)).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn write_journal(config: &KnobyteConfig, entry: &JournalEntry) -> Result<(), String> {
    write_json_atomic(&journal_path(config, &entry.operation_id), entry)
}

pub fn list_journal(config: &KnobyteConfig) -> Vec<JournalEntry> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(journal_dir(config)) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(c) = fs::read_to_string(&p) {
                    if let Ok(j) = serde_json::from_str::<JournalEntry>(&c) {
                        out.push(j);
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    out
}

/// Outcome of rolling an interrupted journal entry forward.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryReport {
    #[serde(rename = "operationId")]
    pub operation_id: String,
    pub state: String,
    pub detail: String,
}

/// Perform the writes and appends of a journal entry, skipping work already done.
/// Returns `Err(path)` when a file matches neither its before nor after revision.
fn roll_forward(config: &KnobyteConfig, entry: &JournalEntry) -> Result<(), String> {
    for w in &entry.writes {
        let abs = config.scaffold_root.join(&w.path);
        let current = file_revision(&abs);
        if current == w.after_revision {
            continue;
        }
        if current != w.before_revision {
            return Err(w.path.clone());
        }
        match (&w.content, &w.after_revision) {
            (Some(c), Some(_)) => atomic_write(&abs, c.as_bytes()).map_err(|e| e.to_string())?,
            (_, None) => remove_if_exists(&abs).map_err(|e| e.to_string())?,
            (None, Some(_)) => return Err(w.path.clone()),
        }
    }
    for a in &entry.appends {
        let abs = config.scaffold_root.join(&a.path);
        if let Some(marker) = &a.marker {
            if fs::read_to_string(&abs).map(|c| c.contains(marker.as_str())).unwrap_or(false) {
                continue;
            }
        }
        append_line(&abs, &a.line).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}", line.trim_end_matches('\n'))?;
    f.sync_all()
}

/// Execute a journaled multi-file change: record intent, perform every write and
/// append, then mark the entry complete. Caller must hold the [`TeamLock`].
pub fn execute_journaled(config: &KnobyteConfig, mut entry: JournalEntry) -> Result<JournalEntry, String> {
    entry.state = JOURNAL_INTENT.to_string();
    write_journal(config, &entry)?;
    if fail_point(config).as_deref() == Some("after-intent") {
        // Test hook: simulate a crash right after the intent record.
        return Err("simulated crash after journal intent".to_string());
    }
    roll_forward(config, &entry).map_err(|p| format!("Target {} changed during apply", p))?;
    complete_entry(config, &mut entry)?;
    prune_journal(config);
    Ok(entry)
}

fn complete_entry(config: &KnobyteConfig, entry: &mut JournalEntry) -> Result<(), String> {
    entry.state = JOURNAL_COMPLETE.to_string();
    entry.completed_at = Some(chrono::Utc::now().to_rfc3339());
    for w in &mut entry.writes {
        w.content = None;
    }
    write_journal(config, entry)
}

/// Crash simulation for tests: a `local/team-failpoint` file containing
/// `after-intent` aborts the next apply right after its intent record.
fn fail_point(config: &KnobyteConfig) -> Option<String> {
    let path = config.local_dir().join("team-failpoint");
    let v = fs::read_to_string(&path).ok()?.trim().to_string();
    let _ = fs::remove_file(&path);
    Some(v).filter(|s| !s.is_empty())
}

/// Roll forward every interrupted journal entry. Caller must hold the [`TeamLock`].
pub fn recover_interrupted(config: &KnobyteConfig) -> Vec<RecoveryReport> {
    let mut reports = Vec::new();
    for mut entry in list_journal(config) {
        if entry.state != JOURNAL_INTENT {
            continue;
        }
        match roll_forward(config, &entry) {
            Ok(()) => {
                let _ = complete_entry(config, &mut entry);
                reports.push(RecoveryReport {
                    operation_id: entry.operation_id.clone(),
                    state: "recovered".to_string(),
                    detail: format!("Completed interrupted {} ({} file(s))", entry.command, entry.writes.len()),
                });
            }
            Err(path) => {
                entry.state = JOURNAL_CONFLICTED.to_string();
                let _ = write_journal(config, &entry);
                reports.push(RecoveryReport {
                    operation_id: entry.operation_id.clone(),
                    state: "conflicted".to_string(),
                    detail: format!(
                        "Interrupted {} could not be completed: {} was changed by someone else",
                        entry.command, path
                    ),
                });
            }
        }
    }
    reports
}

fn prune_journal(config: &KnobyteConfig) {
    let all = list_journal(config);
    let complete: Vec<&JournalEntry> = all.iter().filter(|e| e.state == JOURNAL_COMPLETE).collect();
    if complete.len() <= JOURNAL_WINDOW {
        return;
    }
    for e in &complete[..complete.len() - JOURNAL_WINDOW] {
        let _ = fs::remove_file(journal_path(config, &e.operation_id));
    }
}
