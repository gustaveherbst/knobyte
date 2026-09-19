use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::KnobyteConfig;

pub const EVENT_KINDS: &[&str] = &["decision", "discovery", "note", "risk", "todo"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEntry {
    pub id: String,
    pub timestamp: String,
    pub kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
}

pub fn append_event_full(
    config: &KnobyteConfig,
    entry: EventEntry,
) -> std::io::Result<EventEntry> {
    config.ensure_scaffold_dirs()?;
    let log_path = config.decisions_log_path();
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    let json_line = serde_json::to_string(&entry)?;
    writeln!(file, "{}", json_line)?;

    Ok(entry)
}

pub fn append_event(
    config: &KnobyteConfig,
    summary: &str,
    kind: &str,
    tags: &[String],
    files: &[String],
    actor: Option<&str>,
) -> std::io::Result<EventEntry> {
    let normalized_kind = if EVENT_KINDS.contains(&kind) {
        kind.to_string()
    } else {
        "note".to_string()
    };

    let entry = EventEntry {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now().to_rfc3339(),
        kind: normalized_kind,
        summary: summary.to_string(),
        details: None,
        tags: tags.to_vec(),
        files: files.to_vec(),
        actor: actor.map(|s| s.to_string()),
        session_id: None,
        supersedes: None,
        superseded_by: None,
        confidence: Some(1.0),
        provenance: None,
    };

    append_event_full(config, entry)
}

pub fn supersede_event(
    config: &KnobyteConfig,
    old_id: &str,
    new_id: &str,
) -> std::io::Result<bool> {
    let log_path = config.decisions_log_path();
    let mut events = read_events_from_path(&log_path);
    let mut modified = false;

    for e in &mut events {
        if e.id == old_id {
            e.superseded_by = Some(new_id.to_string());
            modified = true;
            break;
        }
    }

    if modified {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&log_path)?;
        for e in &events {
            let json_line = serde_json::to_string(e)?;
            writeln!(file, "{}", json_line)?;
        }
    }

    Ok(modified)
}

pub fn read_events(config: &KnobyteConfig) -> Vec<EventEntry> {
    read_events_from_path(&config.decisions_log_path())
}

pub fn read_events_from_path(path: &Path) -> Vec<EventEntry> {
    if !path.exists() {
        return Vec::new();
    }

    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };

    let reader = BufReader::new(file);
    let mut events = Vec::new();

    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<EventEntry>(trimmed) {
            events.push(entry);
        }
    }

    events
}

#[derive(Debug, Default, Clone)]
pub struct TimelineFilter {
    pub query: Option<String>,
    pub kind: Option<String>,
    pub file: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub include_superseded: bool,
    pub limit: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TimelineResponse {
    pub entries: Vec<EventEntry>,
    pub total_matched: usize,
    pub truncated: bool,
}

pub fn query_timeline(config: &KnobyteConfig, filter: TimelineFilter) -> TimelineResponse {
    let mut events = read_events(config);

    // Filter out superseded entries unless explicitly requested
    if !filter.include_superseded {
        events.retain(|e| e.superseded_by.is_none());
    }

    // Filter events
    if let Some(ref q) = filter.query {
        let q_lower = q.to_lowercase();
        events.retain(|e| {
            e.summary.to_lowercase().contains(&q_lower)
                || e.tags.iter().any(|t| t.to_lowercase().contains(&q_lower))
                || e.details.as_deref().unwrap_or("").to_lowercase().contains(&q_lower)
        });
    }

    if let Some(ref k) = filter.kind {
        events.retain(|e| e.kind.eq_ignore_ascii_case(k));
    }

    if let Some(ref f) = filter.file {
        events.retain(|e| e.files.iter().any(|file_item| file_item.contains(f)));
    }

    if let Some(since_time) = filter.since {
        events.retain(|e| {
            if let Ok(ts) = DateTime::parse_from_rfc3339(&e.timestamp) {
                ts.with_timezone(&Utc) >= since_time
            } else {
                false
            }
        });
    }

    // Sort newest first
    events.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    let total_matched = events.len();
    let limit = if filter.limit == 0 { 50 } else { filter.limit };
    let truncated = events.len() > limit;

    let entries: Vec<EventEntry> = events.into_iter().take(limit).collect();

    TimelineResponse {
        entries,
        total_matched,
        truncated,
    }
}
