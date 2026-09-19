use std::fs;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::KnobyteConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    pub metadata: Option<serde_json::Value>,
}

pub fn list_activity(config: &KnobyteConfig, limit: usize) -> Vec<ActivityRecord> {
    let dir = config.activity_dir();
    if !dir.exists() {
        return Vec::new();
    }

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

    records.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    records.into_iter().take(if limit == 0 { 50 } else { limit }).collect()
}

pub fn record_activity(
    config: &KnobyteConfig,
    actor: &str,
    action: &str,
    entity_kind: &str,
    entity_id: &str,
    entity_title: &str,
    summary: &str,
    metadata: Option<serde_json::Value>,
) -> Result<ActivityRecord, String> {
    let dir = config.activity_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

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
    };

    let path = dir.join(format!("{}.json", record.id));
    let content = serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| e.to_string())?;

    Ok(record)
}
