use std::fs;
use serde::{Deserialize, Serialize};

use crate::config::KnobyteConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GitAlias {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(default, rename = "gitAliases")]
    pub git_aliases: Vec<GitAlias>,
    pub status: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentMemberSelection {
    #[serde(rename = "memberId")]
    pub member_id: String,
    #[serde(rename = "selectedAt")]
    pub selected_at: String,
}

pub fn list_members(config: &KnobyteConfig) -> Vec<Member> {
    let members_dir = config.members_dir();
    if !members_dir.exists() {
        return Vec::new();
    }

    let mut members = Vec::new();
    if let Ok(entries) = fs::read_dir(members_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(m) = serde_json::from_str::<Member>(&content) {
                        members.push(m);
                    }
                }
            }
        }
    }

    members.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    members
}

pub fn get_member(config: &KnobyteConfig, id: &str) -> Option<Member> {
    let path = config.members_dir().join(format!("{}.json", id));
    if path.exists() {
        if let Ok(content) = fs::read_to_string(path) {
            return serde_json::from_str::<Member>(&content).ok();
        }
    }
    None
}

pub fn get_current_member(config: &KnobyteConfig) -> Option<Member> {
    let local_file = config.local_dir().join("current_member.json");
    if local_file.exists() {
        if let Ok(content) = fs::read_to_string(local_file) {
            if let Ok(selection) = serde_json::from_str::<CurrentMemberSelection>(&content) {
                return get_member(config, &selection.member_id);
            }
        }
    }

    // Fallback: try to match Git config user.email or user.name against member list
    let members = list_members(config);
    if let Some(first_active) = members.into_iter().find(|m| m.status == "active") {
        return Some(first_active);
    }

    // Auto-detect from git config user.name and user.email
    let (git_name, git_email) = detect_git_user(&config.project_root);
    let display_name = git_name.or_else(|| std::env::var("USER").ok()).unwrap_or_else(|| "Agent Contributor".to_string());
    let member_id = display_name.to_lowercase().replace(|c: char| !c.is_alphanumeric(), "-");
    let now = chrono::Utc::now().to_rfc3339();

    let auto_member = Member {
        id: member_id,
        display_name: display_name.clone(),
        git_aliases: vec![GitAlias {
            name: Some(display_name),
            email: git_email,
        }],
        status: "active".to_string(),
        created_at: now.clone(),
        updated_at: now,
    };

    let _ = save_member(config, &auto_member);
    Some(auto_member)
}

pub fn detect_git_user(project_root: &std::path::Path) -> (Option<String>, Option<String>) {
    let name = std::process::Command::new("git")
        .args(["config", "user.name"])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    let email = std::process::Command::new("git")
        .args(["config", "user.email"])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());

    (name, email)
}

pub fn select_current_member(config: &KnobyteConfig, member_id: &str) -> Result<(), String> {
    let member = get_member(config, member_id)
        .ok_or_else(|| format!("Member '{}' not found", member_id))?;

    if member.status != "active" {
        return Err(format!("Cannot select deactivated member '{}'", member_id));
    }

    let local_dir = config.local_dir();
    let _ = fs::create_dir_all(&local_dir);

    let selection = CurrentMemberSelection {
        member_id: member_id.to_string(),
        selected_at: chrono::Utc::now().to_rfc3339(),
    };

    let content = serde_json::to_string_pretty(&selection).map_err(|e| e.to_string())?;
    fs::write(local_dir.join("current_member.json"), content).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn clear_current_member(config: &KnobyteConfig) -> Result<(), String> {
    let local_file = config.local_dir().join("current_member.json");
    if local_file.exists() {
        fs::remove_file(local_file).map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn save_member(config: &KnobyteConfig, member: &Member) -> Result<(), String> {
    let members_dir = config.members_dir();
    fs::create_dir_all(&members_dir).map_err(|e| e.to_string())?;
    let path = members_dir.join(format!("{}.json", member.id));
    let content = serde_json::to_string_pretty(member).map_err(|e| e.to_string())?;
    fs::write(path, content).map_err(|e| e.to_string())?;
    Ok(())
}
