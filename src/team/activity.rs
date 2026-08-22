use std::fs;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::config::KnobyteConfig;
use crate::events::{read_events, EventEntry};
use crate::team::envelope::{paginate, Page, TeamError};
use crate::team::identity::{resolve_actor, ActorRef};
use crate::team::relay::{observe_repo_state, ObservedRepoState};
use crate::team::workflow::{parse_action, Ctx, Plan};

/// Schema of activity records written by the workflow engine. Records without
/// the field are legacy (schema 1) and carry no provenance.
pub const ACTIVITY_SCHEMA_VERSION: u32 = 2;
const MAX_SUBJECTS: usize = 32;

/// Stable reference an immutable activity record points at.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ActivitySubject {
    Entity {
        id: String,
        #[serde(rename = "entityKind")]
        entity_kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    Code {
        #[serde(rename = "symbolId")]
        symbol_id: String,
    },
    File {
        path: String,
    },
    Commit {
        hash: String,
    },
}

impl ActivitySubject {
    pub fn entity(kind: &str, id: &str, title: Option<&str>) -> Self {
        ActivitySubject::Entity { id: id.to_string(), entity_kind: kind.to_string(), title: title.map(|s| s.to_string()) }
    }

    /// Parse `entity:<kind>:<id>`, `code:<symbol>`, `file:<path>` or `commit:<hash>`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let (kind, rest) = spec.split_once(':').ok_or_else(|| {
            format!("Invalid subject '{}': use entity:<kind>:<id>, code:<symbol>, file:<path> or commit:<hash>", spec)
        })?;
        let rest = rest.trim();
        if rest.is_empty() {
            return Err(format!("Invalid subject '{}': empty value", spec));
        }
        match kind {
            "entity" => {
                let (k, id) = rest.split_once(':').ok_or_else(|| format!("Invalid subject '{}': use entity:<kind>:<id>", spec))?;
                Ok(ActivitySubject::entity(k, id, None))
            }
            "code" => Ok(ActivitySubject::Code { symbol_id: rest.to_string() }),
            "file" => Ok(ActivitySubject::File { path: rest.to_string() }),
            "commit" => Ok(ActivitySubject::Commit { hash: rest.to_string() }),
            other => Err(format!("Invalid subject kind '{}'", other)),
        }
    }
}

/// How the record was created: by a workflow operation, or recorded directly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum ActivityOrigin {
    Workflow { operation: String },
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActivityRecord {
    pub id: String,
    pub timestamp: String,
    pub actor: String,
    pub action: String,
    #[serde(rename = "entityKind")]
    pub entity_kind: String,
    #[serde(rename = "entityId")]
    pub entity_id: String,
    #[serde(rename = "entityTitle")]
    pub entity_title: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(default = "legacy_schema", rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(default, rename = "actorRef", skip_serializing_if = "Option::is_none")]
    pub actor_ref: Option<ActorRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subjects: Vec<ActivitySubject>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ActivityOrigin>,
    #[serde(default, rename = "repoState", skip_serializing_if = "Option::is_none")]
    pub repo_state: Option<ObservedRepoState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workstream: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn legacy_schema() -> u32 {
    1
}

fn load_all(config: &KnobyteConfig) -> Vec<ActivityRecord> {
    let dir = config.activity_dir();
    let mut records = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(record) = serde_json::from_str::<ActivityRecord>(&content) {
                        records.push(record);
                    }
                }
            }
        }
    }
    // Newest first; ties broken by id for a deterministic order.
    records.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| b.id.cmp(&a.id)));
    records
}

pub fn list_activity(config: &KnobyteConfig, limit: usize) -> Vec<ActivityRecord> {
    load_all(config).into_iter().take(if limit == 0 { 50 } else { limit }).collect()
}

/// Parse `--since`: RFC 3339, `YYYY-MM-DD`, or a relative `Nd` / `Nh`.
pub fn parse_since(value: &str) -> Result<chrono::DateTime<chrono::Utc>, TeamError> {
    let v = value.trim();
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(v) {
        return Ok(t.with_timezone(&chrono::Utc));
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).expect("midnight").and_utc());
    }
    let (num, unit) = v.split_at(v.len().saturating_sub(1));
    if let Ok(n) = num.parse::<i64>() {
        if (0..=36500).contains(&n) {
            match unit {
                "d" => return Ok(chrono::Utc::now() - chrono::Duration::days(n)),
                "h" => return Ok(chrono::Utc::now() - chrono::Duration::hours(n)),
                _ => {}
            }
        }
    }
    Err(TeamError::usage(format!(
        "Invalid --since '{}': use an RFC 3339 timestamp, YYYY-MM-DD, or a relative Nd/Nh such as 30d",
        value
    )))
}

fn at_or_after(ts: &str, since: &chrono::DateTime<chrono::Utc>) -> bool {
    chrono::DateTime::parse_from_rfc3339(ts).map(|t| t.with_timezone(&chrono::Utc) >= *since).unwrap_or(false)
}

/// Paged activity listing (`--since`, `--cursor`, `--limit`).
pub fn list_activity_page(
    config: &KnobyteConfig,
    since: Option<&str>,
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<Page<ActivityRecord>, TeamError> {
    let since_t = since.map(parse_since).transpose()?;
    let mut all = load_all(config);
    if let Some(s) = &since_t {
        all.retain(|r| at_or_after(&r.timestamp, s));
    }
    paginate(all, |r| r.id.clone(), &format!("since={}", since.unwrap_or("")), cursor, limit)
}

pub fn get_activity(config: &KnobyteConfig, id: &str) -> Option<ActivityRecord> {
    crate::team::validate_entity_id(id).ok()?;
    let path = config.activity_dir().join(format!("{}.json", id));
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

/// Legacy direct recorder (kept for callers outside the workflow engine). The
/// record is written atomically and carries the current repository state.
#[allow(clippy::too_many_arguments)]
pub fn record_activity(
    config: &KnobyteConfig,
    actor: &str,
    action: &str,
    entity_kind: &str,
    entity_id: &str,
    entity_title: &str,
    summary: &str,
    metadata: Option<Value>,
) -> Result<ActivityRecord, String> {
    let record = ActivityRecord {
        id: Uuid::new_v4().to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        actor: actor.to_string(),
        action: action.to_string(),
        entity_kind: entity_kind.to_string(),
        entity_id: entity_id.to_string(),
        entity_title: entity_title.to_string(),
        summary: summary.to_string(),
        metadata,
        schema_version: ACTIVITY_SCHEMA_VERSION,
        actor_ref: None,
        subjects: if entity_id.is_empty() { Vec::new() } else { vec![ActivitySubject::entity(entity_kind, entity_id, Some(entity_title))] },
        origin: Some(ActivityOrigin::Custom),
        repo_state: Some(observe_repo_state(&config.project_root)),
        workstream: None,
        label: None,
    };
    let path = config.activity_dir().join(format!("{}.json", record.id));
    crate::team::store::write_json_atomic(&path, &record)?;
    Ok(record)
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ActivityInput {
    pub action: String,
    pub summary: String,
    #[serde(default, rename = "entityKind")]
    pub entity_kind: Option<String>,
    #[serde(default, rename = "entityId")]
    pub entity_id: Option<String>,
    #[serde(default, rename = "entityTitle")]
    pub entity_title: Option<String>,
    #[serde(default)]
    pub subjects: Vec<ActivitySubject>,
    #[serde(default)]
    pub workstream: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordAction {
    activity: ActivityInput,
}

fn valid_action_name(a: &str) -> bool {
    !a.is_empty()
        && a.len() <= 64
        && a.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

pub(crate) fn plan_record(ctx: &mut Ctx, action: &Value) -> Result<Plan, TeamError> {
    let RecordAction { activity } = parse_action(action)?;
    if !valid_action_name(&activity.action) {
        return Err(TeamError::validation("Activity action must be 1-64 characters of letters, digits, '.', '_' or '-'"));
    }
    if activity.summary.trim().is_empty() || activity.summary.len() > 2048 {
        return Err(TeamError::validation("Activity summary must be 1-2048 bytes"));
    }
    if activity.subjects.len() > MAX_SUBJECTS {
        return Err(TeamError::validation(format!("At most {} subjects are allowed", MAX_SUBJECTS)));
    }
    if let Some(ws) = &activity.workstream {
        crate::team::validate_entity_id(ws).map_err(TeamError::validation)?;
    }
    let id = ctx.ids.id("activity", || Uuid::new_v4().to_string())?;
    let entity_kind = activity.entity_kind.clone().unwrap_or_else(|| "general".to_string());
    let entity_id = activity.entity_id.clone().unwrap_or_default();
    let record = ActivityRecord {
        id: id.clone(),
        timestamp: ctx.now(),
        actor: ctx.actor_id(),
        action: activity.action.clone(),
        entity_kind,
        entity_title: activity.entity_title.clone().unwrap_or_else(|| {
            if entity_id.is_empty() { "Repository".to_string() } else { entity_id.clone() }
        }),
        entity_id,
        summary: activity.summary.clone(),
        metadata: None,
        schema_version: ACTIVITY_SCHEMA_VERSION,
        actor_ref: Some(ctx.authority.actor.clone()),
        subjects: activity.subjects.clone(),
        origin: Some(ActivityOrigin::Custom),
        repo_state: Some(ctx.authority.repo_state.clone()),
        workstream: activity.workstream.clone(),
        label: activity.label.clone(),
    };
    let mut plan = Plan::default();
    let rel = ctx.rel(&ctx.config.activity_dir().join(format!("{}.json", id)));
    plan.write_json(rel, "canonical", &record, "Record activity")?;
    plan.summary = format!("Record activity '{}'", record.action);
    plan.result = serde_json::to_value(&record).unwrap_or(Value::Null);
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Merged timeline (activity + decisions.jsonl)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineItem {
    /// `activity` or `log`.
    pub source: String,
    pub id: String,
    pub timestamp: String,
    pub actor: Option<String>,
    pub kind: String,
    pub summary: String,
    #[serde(default, rename = "repoState", skip_serializing_if = "Option::is_none")]
    pub repo_state: Option<ObservedRepoState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ActivityRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<EventEntry>,
}

/// Merge canonical activity with the decision/event log into one newest-first page.
pub fn activity_timeline(
    config: &KnobyteConfig,
    source: Option<&str>,
    since: Option<&str>,
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<Page<TimelineItem>, TeamError> {
    if let Some(s) = source {
        if s != "activity" && s != "log" {
            return Err(TeamError::usage("--source must be 'activity' or 'log'"));
        }
    }
    let since_t = since.map(parse_since).transpose()?;
    let mut items = Vec::new();
    if source != Some("log") {
        for a in load_all(config) {
            items.push(TimelineItem {
                source: "activity".to_string(),
                id: a.id.clone(),
                timestamp: a.timestamp.clone(),
                actor: Some(a.actor.clone()),
                kind: a.action.clone(),
                summary: a.summary.clone(),
                repo_state: a.repo_state.clone(),
                activity: Some(a),
                event: None,
            });
        }
    }
    if source != Some("activity") {
        for e in read_events(config) {
            items.push(TimelineItem {
                source: "log".to_string(),
                id: e.id.clone(),
                timestamp: e.timestamp.clone(),
                actor: e.actor.clone(),
                kind: e.kind.clone(),
                summary: e.summary.clone(),
                repo_state: None,
                activity: None,
                event: Some(e),
            });
        }
    }
    if let Some(s) = &since_t {
        items.retain(|i| at_or_after(&i.timestamp, s));
    }
    items.sort_by(|a, b| b.timestamp.cmp(&a.timestamp).then_with(|| b.id.cmp(&a.id)));
    paginate(
        items,
        |i| format!("{}:{}", i.source, i.id),
        &format!("source={};since={}", source.unwrap_or(""), since.unwrap_or("")),
        cursor,
        limit,
    )
}

/// The actor id this checkout records activity under.
pub fn default_actor(config: &KnobyteConfig) -> String {
    resolve_actor(config).actor.id()
}
