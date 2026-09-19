use std::fs;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::KnobyteConfig;
use crate::wiki::parser::parse_markdown_entity;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecItem {
    pub id: String,
    pub title: String,
    pub summary: Option<String>,
    pub status: String,
    pub file: String,
}

pub fn list_specs(config: &KnobyteConfig) -> Vec<SpecItem> {
    let dir = config.specs_dir();
    if !dir.exists() {
        return Vec::new();
    }

    let mut specs = Vec::new();
    for entry in WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("md") {
            let rel_path = path.strip_prefix(&config.scaffold_root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| path.to_string_lossy().to_string());

            if let Ok(content) = fs::read_to_string(path) {
                if let Some(entity) = parse_markdown_entity(&rel_path, &content) {
                    specs.push(SpecItem {
                        id: entity.id,
                        title: entity.title,
                        summary: entity.summary,
                        status: entity.status,
                        file: entity.file,
                    });
                }
            }
        }
    }

    specs.sort_by(|a, b| a.title.cmp(&b.title));
    specs
}
