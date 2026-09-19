use std::fs;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::config::KnobyteConfig;
use crate::wiki::parser::parse_markdown_entity;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftIssue {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    pub message: String,
    pub severity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GroundingHealth {
    pub intact: usize,
    pub changed: usize,
    pub missing: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftReport {
    pub score: f64,
    pub status: String,
    pub file_count: usize,
    pub repo_file_count: usize,
    pub issue_count: usize,
    pub issues: Vec<DriftIssue>,
    pub grounding: GroundingHealth,
}

pub fn run_drift_check(config: &KnobyteConfig) -> DriftReport {
    let mut file_count = 0;
    let mut issues = Vec::new();
    let mut grounding = GroundingHealth::default();

    let scaffold_root = &config.scaffold_root;
    if !scaffold_root.exists() {
        return DriftReport {
            score: 0.0,
            status: "error".to_string(),
            file_count: 0,
            repo_file_count: 0,
            issue_count: 1,
            issues: vec![DriftIssue {
                file: scaffold_root.to_string_lossy().to_string(),
                symbol: None,
                message: "Scaffold directory does not exist. Run 'knobyte setup' first.".to_string(),
                severity: "error".to_string(),
            }],
            grounding,
        };
    }

    let graph_db_path = config.graph_db_path();
    let graph_conn = rusqlite::Connection::open(&graph_db_path).ok();

    let repo_file_count = if let Some(ref conn) = graph_conn {
        conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0) as usize
    } else {
        0
    };

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

        file_count += 1;

        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => {
                issues.push(DriftIssue {
                    file: rel_path.clone(),
                    symbol: None,
                    message: "Failed to read file".to_string(),
                    severity: "error".to_string(),
                });
                continue;
            }
        };

        if let Some(entity) = parse_markdown_entity(&rel_path, &content) {
            for node_id in &entity.grounds_to {
                grounding.total += 1;

                if let Some(ref conn) = graph_conn {
                    // Check if node exists in nodes table
                    let current_hash_res: rusqlite::Result<Option<String>> = conn.query_row(
                        "SELECT body_hash FROM nodes WHERE id = ?1",
                        params![node_id],
                        |r| r.get(0),
                    );

                    match current_hash_res {
                        Ok(current_hash) => {
                            // Check baseline in _knobyte_grounded_source
                            let baseline_hash_res: rusqlite::Result<String> = conn.query_row(
                                "SELECT body_hash FROM _knobyte_grounded_source WHERE node_id = ?1 LIMIT 1",
                                params![node_id],
                                |r| r.get(0),
                            );

                            match baseline_hash_res {
                                Ok(baseline_hash) => {
                                    if let Some(ref cur) = current_hash {
                                        if cur == &baseline_hash {
                                            grounding.intact += 1;
                                        } else {
                                            grounding.changed += 1;
                                            issues.push(DriftIssue {
                                                file: rel_path.clone(),
                                                symbol: Some(node_id.clone()),
                                                message: format!("Code symbol has drifted from baseline: {}", node_id),
                                                severity: "warning".to_string(),
                                            });
                                        }
                                    } else {
                                        grounding.intact += 1;
                                    }
                                }
                                Err(_) => {
                                    grounding.intact += 1;
                                }
                            }
                        }
                        Err(_) => {
                            grounding.missing += 1;
                            issues.push(DriftIssue {
                                file: rel_path.clone(),
                                symbol: Some(node_id.clone()),
                                message: format!("Grounded code node not found in code graph: {}", node_id),
                                severity: "warning".to_string(),
                            });
                        }
                    }
                }
            }
        }
    }

    let (score, status) = if grounding.total == 0 {
        issues.push(DriftIssue {
            file: "scaffold".to_string(),
            symbol: None,
            message: "No grounded code symbols found in scaffold documentation. Score is unmeasured (0.0). Run 'knobyte sync-groundings' to link docs to code.".to_string(),
            severity: "warning".to_string(),
        });
        (0.0, "unmeasured".to_string())
    } else {
        let intact_pct = (grounding.intact as f64 / grounding.total as f64) * 100.0;
        let penalty = (issues.len() as f64) * 10.0;
        let final_score = (intact_pct - penalty).clamp(0.0, 100.0);
        let stat = if final_score >= 80.0 { "healthy" } else { "drifting" };
        (final_score, stat.to_string())
    };

    let issue_count = issues.len();

    DriftReport {
        score,
        status,
        file_count,
        repo_file_count,
        issue_count,
        issues,
        grounding,
    }
}
