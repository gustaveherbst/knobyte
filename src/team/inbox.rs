use std::fs;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::KnobyteConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxDraft {
    pub id: String,
    pub target: String,
    pub title: String,
    #[serde(rename = "proposedContent")]
    pub proposed_content: String,
    pub reason: String,
    pub author: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxProposal {
    pub id: String,
    pub target: String,
    pub title: String,
    #[serde(rename = "proposedContent")]
    pub proposed_content: String,
    pub reason: String,
    pub author: String,
    pub status: String,
    #[serde(default, rename = "decisionReason", skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
    #[serde(default, rename = "decisionBy", skip_serializing_if = "Option::is_none")]
    pub decision_by: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

pub fn inbox_drafts_dir(config: &KnobyteConfig) -> std::path::PathBuf {
    config.local_dir().join("inbox_drafts")
}

pub fn list_inbox_drafts(config: &KnobyteConfig) -> Vec<InboxDraft> {
    let dir = inbox_drafts_dir(config);
    if !dir.exists() {
        return Vec::new();
    }

    let mut list = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(d) = serde_json::from_str::<InboxDraft>(&content) {
                        list.push(d);
                    }
                }
            }
        }
    }
    list
}

pub fn save_inbox_draft(config: &KnobyteConfig, draft: &InboxDraft) -> Result<(), String> {
    let dir = inbox_drafts_dir(config);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}.json", draft.id));
    let content = serde_json::to_string_pretty(draft).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn delete_inbox_draft(config: &KnobyteConfig, id: &str) -> Result<(), String> {
    let path = inbox_drafts_dir(config).join(format!("{}.json", id));
    if path.exists() {
        fs::remove_file(path).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn list_inbox_proposals(config: &KnobyteConfig) -> Vec<InboxProposal> {
    let dir = config.inbox_dir();
    if !dir.exists() {
        return Vec::new();
    }

    let mut list = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(p) = serde_json::from_str::<InboxProposal>(&content) {
                        list.push(p);
                    }
                }
            }
        }
    }
    list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    list
}

pub fn publish_inbox_draft(config: &KnobyteConfig, draft_id: &str) -> Result<InboxProposal, String> {
    let drafts = list_inbox_drafts(config);
    let draft = drafts.into_iter().find(|d| d.id == draft_id)
        .ok_or_else(|| format!("Draft '{}' not found", draft_id))?;

    let now = chrono::Utc::now().to_rfc3339();
    let proposal = InboxProposal {
        id: format!("prop_{}", Uuid::new_v4()),
        target: draft.target,
        title: draft.title,
        proposed_content: draft.proposed_content,
        reason: draft.reason,
        author: draft.author,
        status: "pending".to_string(),
        decision_reason: None,
        decision_by: None,
        created_at: now.clone(),
        updated_at: now,
    };

    let inbox_dir = config.inbox_dir();
    fs::create_dir_all(&inbox_dir).map_err(|e| e.to_string())?;
    let path = inbox_dir.join(format!("{}.json", proposal.id));
    let content = serde_json::to_string_pretty(&proposal).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| e.to_string())?;

    let _ = delete_inbox_draft(config, draft_id);

    Ok(proposal)
}
