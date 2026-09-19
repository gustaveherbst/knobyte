use std::path::Path;
use crate::wiki::models::{Frontmatter, WikiEntity};

pub fn parse_markdown_entity(file_rel_path: &str, content: &str) -> Option<WikiEntity> {
    let (frontmatter_opt, body) = extract_frontmatter(content);

    let fm = frontmatter_opt.unwrap_or_default();

    let id = fm.id.unwrap_or_else(|| {
        let stem = Path::new(file_rel_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("entity");
        format!("kb_{}", stem)
    });

    let title = fm.title.unwrap_or_else(|| {
        extract_first_h1(&body).unwrap_or_else(|| {
            Path::new(file_rel_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("Untitled")
                .to_string()
        })
    });

    let entity_type = fm.entity_type.unwrap_or_else(|| {
        if file_rel_path.contains("specs") {
            "spec".to_string()
        } else if file_rel_path.contains("patterns") {
            "pattern".to_string()
        } else if file_rel_path.contains("topics") {
            "topic".to_string()
        } else if file_rel_path.contains("context") {
            "architecture".to_string()
        } else {
            "document".to_string()
        }
    });

    let status = fm.status.unwrap_or_else(|| "accepted".to_string());
    let revision = fm.revision.unwrap_or(1);

    let mut grounds_to = Vec::new();
    for g in fm.grounds_to {
        if let Some(s) = g.as_str() {
            grounds_to.push(s.to_string());
        } else if let Some(obj) = g.as_object() {
            if let Some(node_id) = obj.get("node_id").and_then(|v| v.as_str()) {
                grounds_to.push(node_id.to_string());
            }
        }
    }

    // Extract inline grounding anchors from markdown body: <!-- kb-ground: <id> -->
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("<!-- kb-ground:")
            .or_else(|| trimmed.strip_prefix("<!-- grounds:"))
            .or_else(|| trimmed.strip_prefix("<!-- kb-anchor:"))
        {
            if let Some(id_part) = rest.strip_suffix("-->") {
                let clean_id = id_part.trim().to_string();
                if !clean_id.is_empty() && !grounds_to.contains(&clean_id) {
                    grounds_to.push(clean_id);
                }
            }
        }
    }

    let entity_key = format!("{}#0", file_rel_path);

    Some(WikiEntity {
        entity_key,
        id,
        file: file_rel_path.to_string(),
        entity_type,
        title,
        summary: fm.summary,
        body,
        status,
        revision,
        relations: fm.relations,
        grounds_to,
        topics: fm.topics,
    })
}

fn extract_frontmatter(content: &str) -> (Option<Frontmatter>, String) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (None, content.to_string());
    }

    let rest = &trimmed[3..];
    if let Some(end_idx) = rest.find("\n---") {
        let yaml_str = &rest[..end_idx];
        let body_start = end_idx + 4;
        let body = rest[body_start..].trim_start_matches('\n').to_string();

        let fm: Option<Frontmatter> = serde_yaml::from_str(yaml_str).ok();
        (fm, body)
    } else {
        (None, content.to_string())
    }
}

fn extract_first_h1(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(stripped) = trimmed.strip_prefix("# ") {
            return Some(stripped.trim().to_string());
        }
    }
    None
}
