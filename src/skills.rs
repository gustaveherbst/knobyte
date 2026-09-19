use std::fs;
use serde::{Deserialize, Serialize};
use crate::config::KnobyteConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSyncAction {
    pub client: String,
    pub skill_name: String,
    pub path: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSyncReport {
    pub dry_run: bool,
    pub actions: Vec<SkillSyncAction>,
}

pub fn sync_skills(config: &KnobyteConfig, tool: Option<&str>, dry_run: bool) -> Result<SkillSyncReport, String> {
    let clients: Vec<&str> = match tool {
        Some("claude") => vec!["claude"],
        Some("codex") => vec!["codex"],
        _ => vec!["claude", "codex"],
    };

    let mut actions = Vec::new();

    for client in clients {
        let skills_base = match client {
            "claude" => config.project_root.join(".claude").join("skills"),
            _ => config.project_root.join(".agents").join("skills"),
        };

        // Skill 1: knobyte-inbox
        let inbox_skill_dir = skills_base.join("knobyte-inbox");
        let inbox_skill_file = inbox_skill_dir.join("SKILL.md");
        let inbox_content = r#"---
name: knobyte-inbox
description: Propose an addition or correction to project knowledge in Knobyte
---

# Knobyte Inbox Skill

Use this skill when you make a discovery, decide on an architecture pattern, or need to correct existing documentation.

Run:
`knobyte inbox draft save --title "<title>" --target "<target>" --content "<markdown>"`
"#;

        actions.push(SkillSyncAction {
            client: client.to_string(),
            skill_name: "knobyte-inbox".to_string(),
            path: inbox_skill_file.to_string_lossy().to_string(),
            action: if inbox_skill_file.exists() { "update".to_string() } else { "create".to_string() },
        });

        if !dry_run {
            let _ = fs::create_dir_all(&inbox_skill_dir);
            let _ = fs::write(inbox_skill_file, inbox_content);
        }

        // Skill 2: knobyte-relay
        let relay_skill_dir = skills_base.join("knobyte-relay");
        let relay_skill_file = relay_skill_dir.join("SKILL.md");
        let relay_content = r#"---
name: knobyte-relay
description: Package context, progress, and next actions as a durable handoff in Knobyte
---

# Knobyte Relay Skill

Use this skill when completing a session or passing context to another engineer or agent.

Run:
`knobyte relay draft save --title "<title>" --summary "<summary>"`
"#;

        actions.push(SkillSyncAction {
            client: client.to_string(),
            skill_name: "knobyte-relay".to_string(),
            path: relay_skill_file.to_string_lossy().to_string(),
            action: if relay_skill_file.exists() { "update".to_string() } else { "create".to_string() },
        });

        if !dry_run {
            let _ = fs::create_dir_all(&relay_skill_dir);
            let _ = fs::write(relay_skill_file, relay_content);
        }
    }

    Ok(SkillSyncReport { dry_run, actions })
}
