use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::KnobyteConfig;

pub const EVENT_KINDS: &[&str] = &["decision", "discovery", "note", "risk", "todo"];

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
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
    /// Where the event came from (e.g. meeting, manual, agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Lifecycle marker (e.g. decided, implemented).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Working directory the event was logged from, relative to the project root (`.` at the root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// Maximum length of a raw `--kind` value before it is rejected outright.
const MAX_KIND_LEN: usize = 16;

/// Validate an event kind: known kinds are accepted case-insensitively and returned in
/// lower case; anything else is refused (never silently stored as a note).
pub fn normalize_event_kind(raw: &str) -> Result<String, String> {
    let usage = || format!("Use one of: {}.", EVENT_KINDS.join(", "));
    if raw.len() > MAX_KIND_LEN {
        return Err(format!("Unknown event kind. {}", usage()));
    }
    let kind = raw.trim().to_ascii_lowercase();
    if EVENT_KINDS.contains(&kind.as_str()) {
        Ok(kind)
    } else {
        Err(format!("Unknown event kind \"{}\". {}", raw, usage()))
    }
}

/// Lexically normalise `path` (resolving `.` and `..`) without touching the filesystem.
fn lexical_normalize(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path` relative to `base` as a forward-slash string (`.` when equal); `../` segments
/// are used when `path` lies outside `base`.
fn relative_posix(base: &Path, path: &Path) -> String {
    let base: Vec<_> = base.components().collect();
    let target: Vec<_> = path.components().collect();
    let common = base.iter().zip(&target).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = Vec::new();
    for _ in common..base.len() {
        parts.push("..".to_string());
    }
    for c in &target[common..] {
        parts.push(c.as_os_str().to_string_lossy().to_string());
    }
    if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

/// A recorded file path normalised relative to the project root (forward slashes).
/// Relative input is resolved against the project root.
pub fn normalize_event_file(project_root: &Path, file: &str) -> String {
    let root = project_root.canonicalize().unwrap_or_else(|_| lexical_normalize(project_root));
    let raw = Path::new(file.trim());
    let joined = if raw.is_absolute() {
        // Resolve symlinked prefixes (e.g. /var -> /private/var) through the deepest
        // existing ancestor, so paths to files that do not exist yet still match the root.
        let raw = lexical_normalize(raw);
        raw.ancestors()
            .find_map(|a| a.canonicalize().ok().map(|c| (a.to_path_buf(), c)))
            .map(|(a, c)| c.join(raw.strip_prefix(&a).unwrap_or(Path::new(""))))
            .unwrap_or(raw)
    } else {
        root.join(raw)
    };
    relative_posix(&root, &lexical_normalize(&joined))
}

/// The actor label recorded on events logged by a person (CLI `log` and the TUI): the
/// selected member's display name, else the configured identity; `None` when unknown.
pub fn logging_actor(config: &KnobyteConfig) -> Option<String> {
    match crate::team::identity::resolve_actor(config).actor {
        crate::team::identity::ActorRef::Unknown => None,
        a => Some(a.label()),
    }
}

/// The current working directory relative to the project root (`.` at the root).
pub fn event_cwd(project_root: &Path) -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let cwd = cwd.canonicalize().unwrap_or(cwd);
    let root = project_root.canonicalize().unwrap_or_else(|_| project_root.to_path_buf());
    Some(relative_posix(&lexical_normalize(&root), &lexical_normalize(&cwd)))
}

pub fn append_event_full(
    config: &KnobyteConfig,
    entry: EventEntry,
) -> std::io::Result<EventEntry> {
    let log_path = config.decisions_log_path();
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }
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
    append_event_with(config, summary, kind, tags, files, actor, None, None)
}

/// Append an event with optional `source` and `status` markers (bounded to 64 bytes each).
#[allow(clippy::too_many_arguments)]
pub fn append_event_with(
    config: &KnobyteConfig,
    summary: &str,
    kind: &str,
    tags: &[String],
    files: &[String],
    actor: Option<&str>,
    source: Option<&str>,
    status: Option<&str>,
) -> std::io::Result<EventEntry> {
    let entry = build_event(config, summary, kind, tags, files, actor, source, status)?;
    append_event_full(config, entry)
}

/// Append an event logged by a person from the CLI or TUI: like [`append_event_with`], and
/// also records the working directory (relative to the project root) it was logged from.
#[allow(clippy::too_many_arguments)]
pub fn append_logged_event(
    config: &KnobyteConfig,
    summary: &str,
    kind: &str,
    tags: &[String],
    files: &[String],
    actor: Option<&str>,
    source: Option<&str>,
    status: Option<&str>,
) -> std::io::Result<EventEntry> {
    let mut entry = build_event(config, summary, kind, tags, files, actor, source, status)?;
    entry.cwd = event_cwd(&config.project_root);
    append_event_full(config, entry)
}

#[allow(clippy::too_many_arguments)]
fn build_event(
    config: &KnobyteConfig,
    summary: &str,
    kind: &str,
    tags: &[String],
    files: &[String],
    actor: Option<&str>,
    source: Option<&str>,
    status: Option<&str>,
) -> std::io::Result<EventEntry> {
    let normalized_kind = normalize_event_kind(kind)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    if summary.trim().is_empty() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "Event message must not be empty"));
    }
    let marker = |v: Option<&str>| -> std::io::Result<Option<String>> {
        match v.map(str::trim).filter(|s| !s.is_empty()) {
            None => Ok(None),
            Some(s) if s.len() <= 64 && !s.chars().any(char::is_control) => Ok(Some(s.to_string())),
            Some(_) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "--source and --status must be at most 64 bytes without control characters",
            )),
        }
    };

    let entry = EventEntry {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now().to_rfc3339(),
        kind: normalized_kind,
        summary: summary.to_string(),
        details: None,
        tags: tags.to_vec(),
        files: files
            .iter()
            .filter(|f| !f.trim().is_empty())
            .map(|f| normalize_event_file(&config.project_root, f))
            .collect(),
        actor: actor.map(|s| s.to_string()),
        session_id: None,
        supersedes: None,
        superseded_by: None,
        confidence: Some(1.0),
        provenance: None,
        source: marker(source)?,
        status: marker(status)?,
        cwd: None,
    };
    Ok(entry)
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

pub const DEFAULT_TIMELINE_LIMIT: usize = 20;
pub const MAX_TIMELINE_LIMIT: usize = 200;

#[derive(Debug, Default, Clone)]
pub struct TimelineFilter {
    pub query: Option<String>,
    pub kind: Option<String>,
    pub file: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub include_superseded: bool,
    pub limit: usize,
}

/// Parse a timeline `--since`: `YYYY-MM-DD`, RFC 3339, or relative `Nd`.
pub fn parse_timeline_since(value: &str) -> Result<DateTime<Utc>, String> {
    let v = value.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(v) {
        return Ok(t.with_timezone(&Utc));
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d") {
        if let Some(t) = d.and_hms_opt(0, 0, 0) {
            return Ok(t.and_utc());
        }
    }
    if let Some(n) = v.strip_suffix('d').and_then(|n| n.parse::<i64>().ok()) {
        if (0..=36500).contains(&n) {
            return Ok(Utc::now() - chrono::Duration::days(n));
        }
    }
    Err(format!("Invalid --since '{}': use YYYY-MM-DD or a relative Nd such as 30d", value))
}

/// Validate a timeline limit (1..=200).
pub fn validate_timeline_limit(limit: usize) -> Result<usize, String> {
    if (1..=MAX_TIMELINE_LIMIT).contains(&limit) {
        Ok(limit)
    } else {
        Err(format!("Timeline limit must be an integer from 1 to {}", MAX_TIMELINE_LIMIT))
    }
}

fn markdown_text_cell(value: &str) -> String {
    let flat = value.replace("\r\n", " ").replace(['\r', '\n'], " ");
    let mut out = String::with_capacity(flat.len());
    for c in flat.chars() {
        if "\\`*_{}[]<>&|~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn markdown_code_cell(value: &str) -> String {
    let flat = value.replace("\r\n", " ").replace(['\r', '\n'], " ");
    flat.split('|')
        .map(|part| {
            if part.is_empty() {
                return String::new();
            }
            let longest = part.split(|c| c != '`').map(|r| r.len()).max().unwrap_or(0);
            let delim = "`".repeat(longest + 1);
            let pad = if part.starts_with('`') || part.ends_with('`') { " " } else { "" };
            format!("{}{}{}{}{}", delim, pad, part, pad, delim)
        })
        .collect::<Vec<_>>()
        .join("\\|")
}

/// Render timeline entries as a Markdown table (for reports and standup notes).
pub fn render_timeline_markdown(resp: &TimelineResponse) -> String {
    let mut out = String::new();
    if resp.entries.is_empty() {
        out.push_str("_No events found._\n");
    } else {
        out.push_str("| Date | Type | Event | Files |\n|---|---|---|---|\n");
        for e in &resp.entries {
            let files = if e.files.is_empty() {
                "—".to_string()
            } else {
                e.files.iter().map(|f| markdown_code_cell(f)).collect::<Vec<_>>().join(", ")
            };
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                markdown_text_cell(e.timestamp.get(..10).unwrap_or(&e.timestamp)),
                e.kind,
                markdown_text_cell(&e.summary),
                files
            ));
        }
    }
    if resp.truncated {
        out.push_str("\n_Some matching events were omitted by the entry limit; narrow the filters._\n");
    }
    out
}

/// Upper bound of serialized events returned by one MCP timeline/log read.
pub const MAX_TIMELINE_OUTPUT_BYTES: usize = 64 * 1024;
/// Note appended when entries were dropped by the entry or output limit.
pub const TIMELINE_OMITTED_NOTE: &str = "Some matching events were omitted by the entry or 64 KiB output limit; narrow the filters.";

/// Clamp a caller-supplied limit to `1..=MAX_TIMELINE_LIMIT` (`None` -> `default`).
pub fn clamp_timeline_limit(limit: Option<u64>, default: usize) -> usize {
    limit.map(|l| l.min(MAX_TIMELINE_LIMIT as u64) as usize).unwrap_or(default).clamp(1, MAX_TIMELINE_LIMIT)
}

/// Keep events (in order) while their pretty-printed size stays within
/// [`MAX_TIMELINE_OUTPUT_BYTES`] (with headroom for the envelope). Oversized
/// events are skipped, not truncated. Returns the kept events and how many were dropped.
pub fn bound_timeline_output(events: Vec<EventEntry>) -> (Vec<EventEntry>, usize) {
    let budget = MAX_TIMELINE_OUTPUT_BYTES - 512;
    let mut used = 0usize;
    let mut kept = Vec::new();
    let mut dropped = 0usize;
    for e in events {
        let size = serde_json::to_string_pretty(&e).map(|s| s.len() + 2 * s.lines().count() + 4).unwrap_or(usize::MAX);
        if used.saturating_add(size) > budget {
            dropped += 1;
            continue;
        }
        used += size;
        kept.push(e);
    }
    (kept, dropped)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TimelineResponse {
    pub entries: Vec<EventEntry>,
    pub total_matched: usize,
    /// Some matching entries were omitted (entry limit, 64 KiB output cap, or read cap).
    pub truncated: bool,
    /// Older log content beyond the 8 MiB / 10,000-line read window was not searched.
    #[serde(default)]
    pub source_truncated: bool,
}

/// Bytes of the event log read by a timeline query (the newest end of the file).
pub const MAX_TIMELINE_READ_BYTES: u64 = 8 * 1024 * 1024;
/// Log lines read by a timeline query (the newest ones).
pub const MAX_TIMELINE_READ_ENTRIES: usize = 10_000;
/// Maximum number of `--file` filters.
pub const MAX_TIMELINE_FILES: usize = 16;
/// Maximum `--query` length in bytes.
pub const MAX_TIMELINE_QUERY_BYTES: usize = 256;

/// Read at most the newest [`MAX_TIMELINE_READ_BYTES`] / [`MAX_TIMELINE_READ_ENTRIES`] of an
/// event log. Returns the entries and whether older content was skipped.
pub fn read_events_bounded(path: &Path) -> (Vec<EventEntry>, bool) {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = fs::File::open(path) else {
        return (Vec::new(), false);
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut truncated = false;
    let start = len.saturating_sub(MAX_TIMELINE_READ_BYTES);
    if start > 0 {
        truncated = true;
        if file.seek(SeekFrom::Start(start)).is_err() {
            return (Vec::new(), true);
        }
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    if file.take(MAX_TIMELINE_READ_BYTES).read_to_end(&mut buf).is_err() {
        return (Vec::new(), truncated);
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        // The first line is (almost certainly) a partial record.
        lines.remove(0);
    }
    let lines: Vec<&str> = lines.into_iter().filter(|l| !l.trim().is_empty()).collect();
    let skip = lines.len().saturating_sub(MAX_TIMELINE_READ_ENTRIES);
    if skip > 0 {
        truncated = true;
    }
    let events = lines[skip..]
        .iter()
        .filter_map(|l| serde_json::from_str::<EventEntry>(l.trim()).ok())
        .collect();
    (events, truncated)
}

/// Validate a timeline `--query`: non-empty and at most 256 bytes.
pub fn validate_timeline_query(query: &str) -> Result<String, String> {
    if query.trim().is_empty() {
        return Err("--query must not be empty".to_string());
    }
    if query.len() > MAX_TIMELINE_QUERY_BYTES {
        return Err(format!("--query must be at most {} bytes", MAX_TIMELINE_QUERY_BYTES));
    }
    Ok(query.to_string())
}

/// Validate and normalise timeline `--file` filters (at most 16, each non-empty) to
/// project-relative paths matched exactly.
pub fn validate_timeline_files(project_root: &Path, files: &[String]) -> Result<Vec<String>, String> {
    if files.len() > MAX_TIMELINE_FILES {
        return Err(format!("At most {} --file filters are allowed", MAX_TIMELINE_FILES));
    }
    files
        .iter()
        .map(|f| {
            if f.trim().is_empty() {
                Err("--file must not be empty".to_string())
            } else {
                Ok(normalize_event_file(project_root, f))
            }
        })
        .collect()
}

pub fn query_timeline(config: &KnobyteConfig, filter: TimelineFilter) -> TimelineResponse {
    let files: Vec<String> = filter.file.iter().map(|f| normalize_event_file(&config.project_root, f)).collect();
    query_timeline_files(config, filter, &files)
}

/// Timeline query where an entry matches when any of its recorded files equals one of
/// `files` (project-relative, exact). Reads are bounded to the newest 8 MiB / 10,000 log
/// lines and the returned entries to 64 KiB of JSON; `truncated` reports either cap.
pub fn query_timeline_files(config: &KnobyteConfig, filter: TimelineFilter, files: &[String]) -> TimelineResponse {
    let (mut events, source_truncated) = read_events_bounded(&config.decisions_log_path());

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

    if !files.is_empty() {
        // Recorded paths are normally already project-relative; only odd (absolute, dotted
        // or legacy) ones need the filesystem-aware normalisation.
        let normalize = |recorded: &str| -> String {
            if Path::new(recorded).is_absolute() || recorded.starts_with('.') || recorded.contains("/.") || recorded.contains('\\') {
                normalize_event_file(&config.project_root, recorded)
            } else {
                recorded.to_string()
            }
        };
        events.retain(|e| e.files.iter().any(|recorded| files.contains(&normalize(recorded))));
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
    let limit = if filter.limit == 0 { DEFAULT_TIMELINE_LIMIT } else { filter.limit.min(MAX_TIMELINE_LIMIT) };
    let mut truncated = source_truncated || events.len() > limit;

    let mut entries: Vec<EventEntry> = Vec::new();
    let mut output_bytes = 0usize;
    for e in events {
        if entries.len() == limit {
            break;
        }
        let size = serde_json::to_string_pretty(&e).map(|s| s.len()).unwrap_or(0) + 8;
        if output_bytes + size > MAX_TIMELINE_OUTPUT_BYTES - 512 {
            // Keep complete entries only; never shorten a record.
            truncated = true;
            continue;
        }
        output_bytes += size;
        entries.push(e);
    }

    TimelineResponse {
        entries,
        total_matched,
        truncated,
        source_truncated,
    }
}
