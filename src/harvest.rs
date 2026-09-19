use std::fs;
use std::process::Command;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::KnobyteConfig;
use crate::events::{append_event_full, read_events, EventEntry};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarvestReport {
    pub commits_harvested: usize,
    pub adrs_harvested: usize,
    pub changelogs_harvested: usize,
    pub total_new_events: usize,
}

pub fn harvest_all(config: &KnobyteConfig, limit: usize) -> HarvestReport {
    let existing_events = read_events(config);
    let mut total_new = 0;

    let commits = harvest_git_commits(config, limit, &existing_events);
    total_new += commits;

    let adrs = harvest_adrs(config, &existing_events);
    total_new += adrs;

    let changelogs = harvest_changelog(config, &existing_events);
    total_new += changelogs;

    HarvestReport {
        commits_harvested: commits,
        adrs_harvested: adrs,
        changelogs_harvested: changelogs,
        total_new_events: total_new,
    }
}

pub fn harvest_git_commits(
    config: &KnobyteConfig,
    limit: usize,
    existing_events: &[EventEntry],
) -> usize {
    let output = match Command::new("git")
        .args([
            "log",
            &format!("-n{}", limit),
            "--pretty=format:%H%x09%an%x09%aI%x09%s",
        ])
        .current_dir(&config.project_root)
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return 0,
    };

    let mut harvested = 0;

    for line in output.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 4 {
            continue;
        }

        let full_hash = parts[0].trim();
        let author = parts[1].trim();
        let timestamp = parts[2].trim();
        let subject = parts[3].trim();

        if full_hash.is_empty() || subject.is_empty() {
            continue;
        }

        let short_hash = if full_hash.len() >= 8 { &full_hash[..8] } else { full_hash };
        let tag_short = format!("commit:{}", short_hash);
        let tag_full = format!("commit:{}", full_hash);

        // Check if already harvested
        if existing_events.iter().any(|e| {
            e.tags.contains(&tag_short) || e.tags.contains(&tag_full)
        }) {
            continue;
        }

        // Determine touched files
        let files_output = Command::new("git")
            .args(["diff-tree", "--no-commit-id", "--name-only", "-r", full_hash])
            .current_dir(&config.project_root)
            .output();

        let files: Vec<String> = match files_output {
            Ok(o) if o.status.success() => {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect()
            }
            _ => Vec::new(),
        };

        let sub_lower = subject.to_lowercase();
        let kind = if sub_lower.starts_with("fix") || sub_lower.contains("bug") || sub_lower.contains("resolve") {
            "decision"
        } else if sub_lower.starts_with("feat") || sub_lower.starts_with("add") {
            "discovery"
        } else if sub_lower.contains("refactor") || sub_lower.contains("rfc") || sub_lower.contains("adr") || sub_lower.contains("breaking") {
            "decision"
        } else {
            "note"
        };

        let entry = EventEntry {
            id: Uuid::new_v4().to_string(),
            timestamp: if timestamp.is_empty() { Utc::now().to_rfc3339() } else { timestamp.to_string() },
            kind: kind.to_string(),
            summary: subject.to_string(),
            details: Some(format!("Harvested from git commit {}", short_hash)),
            tags: vec![tag_short, "harvested:git".to_string()],
            files,
            actor: Some(author.to_string()),
            session_id: None,
            supersedes: None,
            superseded_by: None,
            confidence: Some(0.95),
            provenance: Some(format!("git commit {}", short_hash)),
        };

        if append_event_full(config, entry).is_ok() {
            harvested += 1;
        }
    }

    harvested
}

pub fn harvest_adrs(
    config: &KnobyteConfig,
    existing_events: &[EventEntry],
) -> usize {
    let possible_adr_dirs = [
        config.project_root.join("docs").join("adr"),
        config.project_root.join("docs").join("adrs"),
        config.project_root.join("doc").join("adr"),
        config.project_root.join("doc").join("adrs"),
        config.project_root.join("adr"),
    ];

    let mut harvested = 0;

    for dir in &possible_adr_dirs {
        if !dir.is_dir() {
            continue;
        }

        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }

            let rel_path = path.strip_prefix(&config.project_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .to_string();

            let adr_tag = format!("adr:{}", rel_path);
            if existing_events.iter().any(|e| e.tags.contains(&adr_tag)) {
                continue;
            }

            let content = match fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Extract title (first markdown header)
            let title = content.lines()
                .find(|l| l.trim().starts_with("# "))
                .map(|l| l.trim().trim_start_matches('#').trim().to_string())
                .unwrap_or_else(|| path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "ADR".to_string()));

            // Extract excerpt
            let excerpt: String = content.lines()
                .take(15)
                .collect::<Vec<_>>()
                .join("\n");

            let event = EventEntry {
                id: Uuid::new_v4().to_string(),
                timestamp: Utc::now().to_rfc3339(),
                kind: "decision".to_string(),
                summary: format!("ADR: {}", title),
                details: Some(excerpt),
                tags: vec!["adr".to_string(), "harvested:doc".to_string(), adr_tag],
                files: vec![rel_path.clone()],
                actor: Some("team".to_string()),
                session_id: None,
                supersedes: None,
                superseded_by: None,
                confidence: Some(1.0),
                provenance: Some(format!("ADR file {}", rel_path)),
            };

            if append_event_full(config, event).is_ok() {
                harvested += 1;
            }
        }
    }

    harvested
}

pub fn harvest_changelog(
    config: &KnobyteConfig,
    existing_events: &[EventEntry],
) -> usize {
    let changelog_files = [
        config.project_root.join("CHANGELOG.md"),
        config.project_root.join("CHANGES.md"),
    ];

    let mut harvested = 0;

    for path in &changelog_files {
        if !path.is_file() {
            continue;
        }

        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let rel_path = path.strip_prefix(&config.project_root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        let mut current_section: Option<String> = None;
        let mut section_lines = Vec::new();

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("## ") {
                if let Some(sec) = current_section.take() {
                    if record_changelog_section(config, &sec, &section_lines, &rel_path, existing_events) {
                        harvested += 1;
                    }
                    section_lines.clear();
                }
                current_section = Some(trimmed.trim_start_matches('#').trim().to_string());
            } else if current_section.is_some() && !trimmed.is_empty() {
                section_lines.push(trimmed.to_string());
            }
        }

        if let Some(sec) = current_section {
            if record_changelog_section(config, &sec, &section_lines, &rel_path, existing_events) {
                harvested += 1;
            }
        }
    }

    harvested
}

fn record_changelog_section(
    config: &KnobyteConfig,
    section_name: &str,
    lines: &[String],
    rel_path: &str,
    existing_events: &[EventEntry],
) -> bool {
    let tag = format!("changelog:{}", section_name);
    if existing_events.iter().any(|e| e.tags.contains(&tag)) {
        return false;
    }

    let summary = format!("Changelog: {}", section_name);
    let details = lines.join("\n");

    let event = EventEntry {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now().to_rfc3339(),
        kind: "discovery".to_string(),
        summary,
        details: Some(details),
        tags: vec!["changelog".to_string(), "harvested:doc".to_string(), tag],
        files: vec![rel_path.to_string()],
        actor: Some("team".to_string()),
        session_id: None,
        supersedes: None,
        superseded_by: None,
        confidence: Some(0.9),
        provenance: Some(format!("Changelog file {}", rel_path)),
    };

    append_event_full(config, event).is_ok()
}
