use std::fs;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::KnobyteConfig;
use crate::drift::checker::run_drift_check;
use crate::graph::fingerprint::compute_body_hash;
use crate::wiki::parser::parse_markdown_entity;
use crate::wiki::WikiIndex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelocationProposal {
    pub scaffold_file: String,
    pub old_node_id: String,
    pub new_node_id: String,
    pub symbol_name: String,
    pub old_file: Option<String>,
    pub new_file: String,
    pub confidence: f64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    pub proposals: Vec<RelocationProposal>,
    pub relocated_count: usize,
    pub dry_run: bool,
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncAction {
    pub file: String,
    pub recommendation: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    pub actions: Vec<SyncAction>,
    pub clean: bool,
    pub proposals: Vec<RelocationProposal>,
}

/// Find all missing grounded nodes across scaffold markdown files and propose relocations.
pub fn find_grounding_relocations(config: &KnobyteConfig) -> Result<Vec<RelocationProposal>, String> {
    let scaffold_root = &config.scaffold_root;
    if !scaffold_root.exists() {
        return Ok(Vec::new());
    }

    let graph_db_path = config.graph_db_path();
    if !graph_db_path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open(&graph_db_path)
        .map_err(|e| format!("Failed to open graph database: {}", e))?;

    let mut proposals = Vec::new();

    for entry in WalkDir::new(scaffold_root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if ext != "md" {
            continue;
        }

        let rel_path = match path.strip_prefix(scaffold_root) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => path.to_string_lossy().to_string(),
        };

        if rel_path.starts_with("local/") || rel_path.starts_with('.') {
            continue;
        }

        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        if let Some(entity) = parse_markdown_entity(&rel_path, &content) {
            for old_node_id in &entity.grounds_to {
                // Check if node exists in nodes table
                let exists: bool = conn.query_row(
                    "SELECT 1 FROM nodes WHERE id = ?1 LIMIT 1",
                    params![old_node_id],
                    |_| Ok(true),
                ).unwrap_or(false);

                if exists {
                    // Intact! No relocation needed.
                    continue;
                }

                // Node is missing in current code graph. Search for relocation candidate!
                if let Some(proposal) = find_candidate_for_missing_node(&conn, &rel_path, old_node_id) {
                    proposals.push(proposal);
                }
            }
        }
    }

    Ok(proposals)
}

fn find_candidate_for_missing_node(
    conn: &Connection,
    scaffold_file: &str,
    old_node_id: &str,
) -> Option<RelocationProposal> {
    // 1. Check _knobyte_grounded_source for baseline code and hash
    let baseline_info: Result<(String, String, String), _> = conn.query_row(
        "SELECT source, body_hash, subject_id FROM _knobyte_grounded_source WHERE node_id = ?1 LIMIT 1",
        params![old_node_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    );

    if let Ok((source, body_hash, old_file)) = baseline_info {
        // Strategy A: Match by exact body hash
        let mut stmt = conn.prepare(
            "SELECT id, file_path, name, kind FROM nodes WHERE body_hash = ?1 LIMIT 5"
        ).ok()?;

        let mut hash_matches = Vec::new();
        if let Ok(rows) = stmt.query_map(params![body_hash], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
        }) {
            for row in rows.flatten() {
                hash_matches.push(row);
            }
        }

        if let Some((new_id, new_file, sym_name, kind)) = hash_matches.into_iter().next() {
            return Some(RelocationProposal {
                scaffold_file: scaffold_file.to_string(),
                old_node_id: old_node_id.to_string(),
                new_node_id: new_id,
                symbol_name: sym_name,
                old_file: Some(old_file),
                new_file,
                confidence: 1.0,
                reason: format!("Exact AST body hash match for {} symbol", kind),
            });
        }

        // Strategy B: Extract symbol name from source and match by name & kind
        if let Some((kind, sym_name)) = extract_symbol_name(&source) {
            let mut name_matches = Vec::new();
            if let Ok(mut stmt) = conn.prepare(
                "SELECT id, file_path, name, kind FROM nodes WHERE name = ?1 AND kind = ?2 LIMIT 5"
            ) {
                if let Ok(rows) = stmt.query_map(params![sym_name, kind], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
                }) {
                    for row in rows.flatten() {
                        name_matches.push(row);
                    }
                }
            }

            if name_matches.len() == 1 {
                let (new_id, new_file, name, k) = name_matches.remove(0);
                return Some(RelocationProposal {
                    scaffold_file: scaffold_file.to_string(),
                    old_node_id: old_node_id.to_string(),
                    new_node_id: new_id,
                    symbol_name: name,
                    old_file: Some(old_file),
                    new_file,
                    confidence: 0.92,
                    reason: format!("Unique matching {} symbol '{}' found in relocated file", k, sym_name),
                });
            }
        }
    }

    // Strategy C: If old_node_id contains readable symbol name (e.g. kind:file:name or kind:name)
    if let Some((_kind, name)) = parse_readable_node_id(old_node_id) {
        let mut name_matches = Vec::new();
        if let Ok(mut stmt) = conn.prepare(
            "SELECT id, file_path, name, kind FROM nodes WHERE name = ?1 LIMIT 5"
        ) {
            if let Ok(rows) = stmt.query_map(params![name], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
            }) {
                for row in rows.flatten() {
                    name_matches.push(row);
                }
            }
        }

        if name_matches.len() == 1 {
            let (new_id, new_file, name, k) = name_matches.remove(0);
            return Some(RelocationProposal {
                scaffold_file: scaffold_file.to_string(),
                old_node_id: old_node_id.to_string(),
                new_node_id: new_id,
                symbol_name: name.clone(),
                old_file: None,
                new_file,
                confidence: 0.88,
                reason: format!("Unique code graph symbol match for '{}' ({})", name, k),
            });
        }
    }

    None
}

/// Apply proposed relocations to markdown files and update grounding baseline in SQLite.
pub fn apply_grounding_relocations(
    config: &KnobyteConfig,
    proposals: &[RelocationProposal],
) -> Result<usize, String> {
    if proposals.is_empty() {
        return Ok(0);
    }

    let graph_db_path = config.graph_db_path();
    let conn = Connection::open(&graph_db_path)
        .map_err(|e| format!("Failed to open graph database: {}", e))?;

    let mut applied = 0;

    for prop in proposals {
        let file_path = config.scaffold_root.join(&prop.scaffold_file);
        if !file_path.exists() {
            continue;
        }

        let content = fs::read_to_string(&file_path)
            .map_err(|e| format!("Failed to read {}: {}", file_path.display(), e))?;

        if !content.contains(&prop.old_node_id) {
            continue;
        }

        // Replace occurrences of old_node_id with new_node_id in markdown
        let updated_content = content.replace(&prop.old_node_id, &prop.new_node_id);
        fs::write(&file_path, updated_content)
            .map_err(|e| format!("Failed to write {}: {}", file_path.display(), e))?;

        // Update _knobyte_grounded_source in graph.db
        let _ = conn.execute(
            "DELETE FROM _knobyte_grounded_source WHERE node_id = ?1",
            params![prop.old_node_id],
        );

        // Fetch new node's location and body hash
        let new_node_info: Result<(String, i64, i64, Option<String>), _> = conn.query_row(
            "SELECT file_path, start_line, end_line, body_hash FROM nodes WHERE id = ?1",
            params![prop.new_node_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        );

        if let Ok((rel_path, start_line, end_line, body_hash)) = new_node_info {
            let full_code_path = config.project_root.join(&rel_path);
            if let Ok(code_content) = fs::read_to_string(&full_code_path) {
                let lines: Vec<&str> = code_content.lines().collect();
                let start_idx = (start_line.saturating_sub(1)) as usize;
                let end_idx = end_line as usize;
                if start_idx < lines.len() && end_idx <= lines.len() && start_idx < end_idx {
                    let source = lines[start_idx..end_idx].join("\n");
                    let hash = body_hash.unwrap_or_else(|| compute_body_hash(&source));
                    let _ = conn.execute(
                        r#"
                        INSERT OR REPLACE INTO _knobyte_grounded_source (
                            subject_kind, subject_id, node_id, source, body_hash, fingerprint
                        ) VALUES ('scaffold', ?1, ?2, ?3, ?4, '')
                        "#,
                        params![rel_path, prop.new_node_id, source, hash],
                    );
                }
            }
        }

        applied += 1;
    }

    // Rebuild wiki index to reflect the relocated anchors
    let wiki_db_path = config.wiki_db_path();
    if let Ok(mut wiki) = WikiIndex::open(&wiki_db_path) {
        let _ = wiki.rebuild(&config.scaffold_root);
    }

    Ok(applied)
}

/// Full synchronization workflow: finds relocations, optionally applies them, and returns summary.
pub fn sync_groundings(config: &KnobyteConfig, dry_run: bool) -> Result<SyncResult, String> {
    let proposals = find_grounding_relocations(config)?;

    if dry_run {
        let count = proposals.len();
        return Ok(SyncResult {
            proposals,
            relocated_count: count,
            dry_run: true,
            success: true,
            message: format!("Dry-run: found {} grounding anchor(s) eligible for relocation", count),
        });
    }

    let relocated = apply_grounding_relocations(config, &proposals)?;

    Ok(SyncResult {
        proposals,
        relocated_count: relocated,
        dry_run: false,
        success: true,
        message: format!("Successfully relocated and healed {} grounding anchor(s)", relocated),
    })
}

pub fn plan_sync(config: &KnobyteConfig, include_warnings: bool) -> SyncReport {
    let report = run_drift_check(config);
    let mut actions = Vec::new();

    let proposals = find_grounding_relocations(config).unwrap_or_default();

    for issue in report.issues {
        if issue.severity == "error" || include_warnings {
            let matching_prop = proposals.iter().find(|p| {
                issue.symbol.as_deref() == Some(&p.old_node_id) || issue.file == p.scaffold_file
            });

            let recommendation = if let Some(p) = matching_prop {
                format!(
                    "Auto-relocate symbol '{}' from {} -> {} (confidence: {:.0}%)",
                    p.symbol_name,
                    p.old_file.as_deref().unwrap_or("previous location"),
                    p.new_file,
                    p.confidence * 100.0
                )
            } else {
                format!("Update {} to reflect recent code modifications", issue.file)
            };

            actions.push(SyncAction {
                file: issue.file.clone(),
                recommendation,
                prompt: format!(
                    "Scaffold file '{}' has drifted: {}. Please review the current implementation and update the markdown documentation accordingly.",
                    issue.file, issue.message
                ),
            });
        }
    }

    let clean = actions.is_empty();
    SyncReport { actions, clean, proposals }
}

fn extract_symbol_name(source: &str) -> Option<(String, String)> {
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('#') || trimmed.starts_with('*') {
            continue;
        }

        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        for (i, &t) in tokens.iter().enumerate() {
            if (t == "fn" || t == "def" || t == "function") && i + 1 < tokens.len() {
                let name = tokens[i + 1].split('(').next()?.split('<').next()?.trim();
                if !name.is_empty() {
                    return Some(("function".to_string(), name.to_string()));
                }
            } else if (t == "struct" || t == "class" || t == "enum" || t == "trait" || t == "interface" || t == "type") && i + 1 < tokens.len() {
                let name = tokens[i + 1].split('{').next()?.split('<').next()?.split('(').next()?.trim();
                let clean = name.trim_end_matches(';').trim_end_matches(':').trim();
                if !clean.is_empty() {
                    return Some((t.to_string(), clean.to_string()));
                }
            }
        }
    }
    None
}

fn parse_readable_node_id(node_id: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = node_id.split(':').collect();
    if parts.len() >= 3 {
        // kind:path:name
        let kind = parts[0].to_string();
        let name = parts[parts.len() - 1].to_string();
        Some((kind, name))
    } else if parts.len() == 2 && !parts[1].chars().all(|c| c.is_ascii_hexdigit()) {
        // kind:name
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        None
    }
}
