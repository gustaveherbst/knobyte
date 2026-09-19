use std::fs;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::KnobyteConfig;

pub const RELAY_SCHEMA_VERSION: u32 = 4;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ObservedRepoState {
    pub branch: Option<String>,
    #[serde(rename = "headCommit")]
    pub head_commit: Option<String>,
    #[serde(rename = "dirtyTree")]
    pub dirty_tree: bool,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelayDraft {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub sender: String,
    #[serde(default, rename = "openToTeam")]
    pub open_to_team: bool,
    #[serde(default, rename = "namedRecipients")]
    pub named_recipients: Vec<String>,
    #[serde(default)]
    pub progress: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default, rename = "nextActions")]
    pub next_actions: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relay {
    pub id: String,
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub title: String,
    pub summary: String,
    pub sender: String,
    #[serde(rename = "openToTeam")]
    pub open_to_team: bool,
    #[serde(rename = "namedRecipients")]
    pub named_recipients: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimant: Option<String>,
    pub status: String,
    #[serde(default)]
    pub progress: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default, rename = "nextActions")]
    pub next_actions: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(rename = "observedState")]
    pub observed_state: ObservedRepoState,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(default, rename = "acknowledgedAt", skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<String>,
    #[serde(default, rename = "closedAt", skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
}

pub fn relay_drafts_dir(config: &KnobyteConfig) -> std::path::PathBuf {
    config.local_dir().join("relay_drafts")
}

pub fn list_relay_drafts(config: &KnobyteConfig) -> Vec<RelayDraft> {
    let dir = relay_drafts_dir(config);
    if !dir.exists() {
        return Vec::new();
    }

    let mut list = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(d) = serde_json::from_str::<RelayDraft>(&content) {
                        list.push(d);
                    }
                }
            }
        }
    }
    list
}

pub fn save_relay_draft(config: &KnobyteConfig, draft: &RelayDraft) -> Result<(), String> {
    let dir = relay_drafts_dir(config);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}.json", draft.id));
    let content = serde_json::to_string_pretty(draft).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn delete_relay_draft(config: &KnobyteConfig, id: &str) -> Result<(), String> {
    let path = relay_drafts_dir(config).join(format!("{}.json", id));
    if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn list_relays(config: &KnobyteConfig) -> Vec<Relay> {
    let dir = config.relays_dir();
    if !dir.exists() {
        return Vec::new();
    }

    let mut list = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(r) = serde_json::from_str::<Relay>(&content) {
                        list.push(r);
                    }
                }
            }
        }
    }
    list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    list
}

pub fn get_relay(config: &KnobyteConfig, id: &str) -> Option<Relay> {
    let path = config.relays_dir().join(format!("{}.json", id));
    if path.exists() {
        if let Ok(content) = fs::read_to_string(path) {
            return serde_json::from_str::<Relay>(&content).ok();
        }
    }
    None
}

pub fn publish_relay_draft(config: &KnobyteConfig, draft_id: &str) -> Result<Relay, String> {
    let drafts = list_relay_drafts(config);
    let draft = drafts.into_iter().find(|d| d.id == draft_id)
        .ok_or_else(|| format!("Relay draft '{}' not found", draft_id))?;

    let now = chrono::Utc::now().to_rfc3339();

    let relay = Relay {
        id: format!("relay_{}", Uuid::new_v4()),
        schema_version: RELAY_SCHEMA_VERSION,
        title: draft.title,
        summary: draft.summary,
        sender: draft.sender,
        open_to_team: draft.open_to_team,
        named_recipients: draft.named_recipients,
        claimant: None,
        status: "published".to_string(),
        progress: draft.progress,
        blockers: draft.blockers,
        next_actions: draft.next_actions,
        evidence: draft.evidence,
        observed_state: ObservedRepoState {
            branch: None,
            head_commit: None,
            dirty_tree: false,
            timestamp: now.clone(),
        },
        created_at: now.clone(),
        updated_at: now,
        acknowledged_at: None,
        closed_at: None,
    };

    let relays_dir = config.relays_dir();
    fs::create_dir_all(&relays_dir).map_err(|e| e.to_string())?;
    let path = relays_dir.join(format!("{}.json", relay.id));
    let content = serde_json::to_string_pretty(&relay).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| e.to_string())?;

    let _ = delete_relay_draft(config, draft_id);

    Ok(relay)
}

pub fn acknowledge_relay(config: &KnobyteConfig, relay_id: &str, member_id: &str) -> Result<Relay, String> {
    let mut relay = get_relay(config, relay_id)
        .ok_or_else(|| format!("Relay '{}' not found", relay_id))?;

    if relay.status != "published" {
        return Err(format!("Relay '{}' is already in status '{}'", relay_id, relay.status));
    }

    let now = chrono::Utc::now().to_rfc3339();
    relay.claimant = Some(member_id.to_string());
    relay.status = "acknowledged".to_string();
    relay.acknowledged_at = Some(now.clone());
    relay.updated_at = now;

    let path = config.relays_dir().join(format!("{}.json", relay.id));
    let content = serde_json::to_string_pretty(&relay).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| e.to_string())?;

    Ok(relay)
}

pub fn close_relay(config: &KnobyteConfig, relay_id: &str, member_id: &str) -> Result<Relay, String> {
    let mut relay = get_relay(config, relay_id)
        .ok_or_else(|| format!("Relay '{}' not found", relay_id))?;

    if relay.status == "closed" {
        return Err(format!("Relay '{}' is already closed", relay_id));
    }

    if relay.sender != member_id && relay.claimant.as_deref() != Some(member_id) {
        return Err(format!("Member '{}' is not the sender or claimant of relay '{}'", member_id, relay_id));
    }

    let now = chrono::Utc::now().to_rfc3339();
    relay.status = "closed".to_string();
    relay.closed_at = Some(now.clone());
    relay.updated_at = now;

    let path = config.relays_dir().join(format!("{}.json", relay.id));
    let content = serde_json::to_string_pretty(&relay).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| e.to_string())?;

    Ok(relay)
}
