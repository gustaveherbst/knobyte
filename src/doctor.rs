//! `knobyte doctor`: one-screen health summary of drift, the code graph (freshness and
//! extractor coverage), the heartbeat, the event log, the indices and the embedding backend,
//! with next steps. Exits 1 when the drift check reports errors.

use serde::{Deserialize, Serialize};

use crate::config::KnobyteConfig;
use crate::cozo::{embedding_status, EmbeddingStatus};
use crate::drift::run_drift_check;
use crate::graph::inspect_status;
use crate::heartbeat::{check_heartbeat, configured_stale_days};
use crate::wiki::WikiIndex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCoverage {
    /// Source files no extractor handles.
    pub unindexed_total: usize,
    /// `extension (files)` entries, most common first (at most 10).
    pub unindexed: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub overall_health: String,
    pub git_repository: bool,
    pub scaffold_configured: bool,
    pub scaffold_id: String,
    pub mode: String,
    pub graph_db_ready: bool,
    pub graph_nodes: i64,
    pub graph_edges: i64,
    pub wiki_db_ready: bool,
    pub wiki_entities: usize,
    pub cozodb_ready: bool,
    /// Configured embedding backend for the Cozo vector indices.
    pub embedding: EmbeddingStatus,
    pub diagnostics: Vec<String>,
    /// Drift score (0-100) from `knobyte check`.
    #[serde(default)]
    pub drift_score: f64,
    #[serde(default)]
    pub drift_errors: usize,
    #[serde(default)]
    pub drift_warnings: usize,
    /// Code graph freshness: fresh, stale, degraded, corrupt, rebuild_required or missing.
    #[serde(default)]
    pub graph_status: String,
    #[serde(default)]
    pub graph_detail: String,
    /// Command that repairs a non-fresh graph.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_remediation: Option<String>,
    /// Extractor coverage recorded by the last graph build (`None` when unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<DoctorCoverage>,
    #[serde(default)]
    pub heartbeat_ok: bool,
    #[serde(default)]
    pub heartbeat_detail: String,
    #[serde(default)]
    pub event_count: usize,
    #[serde(default)]
    pub next_steps: Vec<String>,
    /// 1 when the drift check reports errors, else 0.
    #[serde(default)]
    pub exit_code: i32,
}

pub fn run_doctor(config: &KnobyteConfig) -> DoctorReport {
    let mut diagnostics = Vec::new();

    let git_repo = config.project_root.join(".git").exists();
    if !git_repo {
        diagnostics.push("Project root is not a git repository.".to_string());
    }

    let scaffold_configured = config.scaffold_root.join("config.json").exists();
    if !scaffold_configured {
        diagnostics.push("Scaffold config.json missing. Run 'knobyte setup'.".to_string());
    }

    // Code graph: freshness, counts and coverage.
    let health = inspect_status(&config.graph_db_path(), &config.project_root);
    let graph_db_ready = health.inspected;
    let graph_nodes = health.counts.nodes;
    let graph_edges = health.counts.edges;
    let graph_detail = {
        let c = &health.changes;
        let mut parts = Vec::new();
        if health.inspected {
            parts.push(format!("{} files, {} nodes, {} edges", health.counts.files, health.counts.nodes, health.counts.edges));
        }
        if c.total > 0 {
            parts.push(format!(
                "{} changed source file(s) ({} added, {} modified, {} deleted)",
                c.total,
                c.added.len(),
                c.modified.len(),
                c.deleted.len()
            ));
        }
        if let Some(d) = health.diagnostics.iter().find(|d| d.severity == "error" || d.severity == "warning") {
            parts.push(d.message.clone());
        }
        parts.join("; ")
    };
    let graph_remediation = if health.status == "fresh" { None } else { health.next_command().map(str::to_string) };
    if health.status == "missing" {
        diagnostics.push("Code graph database (graph.db) not built. Run 'knobyte graph rebuild'.".to_string());
    }
    let coverage = health.coverage.as_ref().map(|c| DoctorCoverage {
        unindexed_total: c.unindexed_total,
        unindexed: c.unindexed.iter().take(10).map(|u| format!("{} ({})", u.extension, u.files)).collect(),
        truncated: c.truncated,
    });

    // Wiki index.
    let mut wiki_db_ready = false;
    let mut wiki_entities = 0;
    let wiki_path = config.wiki_db_path();
    if wiki_path.exists() {
        if let Ok(index) = WikiIndex::open(&wiki_path) {
            if let Ok(entities) = index.list() {
                wiki_db_ready = true;
                wiki_entities = entities.len();
            }
        }
    } else {
        diagnostics.push("Wiki search index (wiki.db) not built. Run 'knobyte wiki rebuild-index'.".to_string());
    }

    let cozodb_ready = config.scaffold_root.join("cozo.db").exists();

    let embedding = embedding_status(&config.embedding);
    if !embedding.model_present {
        diagnostics.push(format!(
            "Embedding backend 'model2vec' is configured but model '{}' is not downloaded. Run 'knobyte cozo model pull --model {}' or 'knobyte cozo model use hashed'.",
            embedding.model.as_deref().unwrap_or(""),
            embedding.model.as_deref().unwrap_or("")
        ));
    }

    // Drift.
    let (drift_score, drift_errors, drift_warnings) = if config.scaffold_root.is_dir() {
        let report = run_drift_check(config);
        (report.score, report.count("error"), report.count("warning"))
    } else {
        (0.0, 0, 0)
    };

    // Heartbeat and events.
    let stale_days = configured_stale_days(config);
    let hb = check_heartbeat(config, stale_days);
    let heartbeat_detail = if !hb.scaffold_exists {
        "scaffold missing".to_string()
    } else if hb.heartbeat_ok {
        if hb.files_without_last_updated > 0 && hb.stale_files.is_empty() {
            format!("HEARTBEAT_OK ({} file(s) without last_updated use file age)", hb.files_without_last_updated)
        } else {
            "HEARTBEAT_OK".to_string()
        }
    } else {
        let mut parts = vec![format!("{} stale file(s)", hb.stale_files.len())];
        if !hb.old_daily_memory_files.is_empty() {
            parts.push(format!("{} old memory file(s)", hb.old_daily_memory_files.len()));
        }
        if hb.memory_cleanup_due {
            parts.push("memory cleanup due".into());
        }
        if let Some(w) = &hb.uncommitted_steps_warning {
            parts.push(w.clone());
        }
        parts.join(", ")
    };
    let event_count = crate::events::read_events(config).len();

    // Next steps.
    let mut next_steps = Vec::new();
    if !config.scaffold_root.is_dir() {
        next_steps.push("Run `knobyte setup` to create the scaffold.".to_string());
    }
    if drift_errors > 0 || drift_warnings > 0 {
        next_steps.push("Run `knobyte check` for drift details, then `knobyte sync` for targeted repair.".to_string());
    }
    if coverage.as_ref().is_some_and(|c| c.unindexed_total > 0) {
        next_steps.push("The listed extensions have no extractor; use source search for those languages or `knobyte graph status --json` for the coverage breakdown.".to_string());
    }
    if health.status != "fresh" {
        match &graph_remediation {
            Some(cmd) => next_steps.push(format!("Run `{}` to repair the graph.", cmd)),
            None => next_steps.push("Review the graph diagnostics before using graph-grounded results.".to_string()),
        }
    }
    if hb.scaffold_exists && !hb.heartbeat_ok {
        next_steps.push("Run `knobyte heartbeat` to see stale context or memory cleanup details.".to_string());
    }
    if !wiki_db_ready && config.scaffold_root.is_dir() {
        next_steps.push("Run `knobyte wiki rebuild-index` to build the wiki search index.".to_string());
    }

    let overall_health = if !scaffold_configured {
        "action_required".to_string()
    } else if drift_errors > 0 {
        "error".to_string()
    } else if diagnostics.is_empty() && next_steps.is_empty() {
        "healthy".to_string()
    } else {
        "warning".to_string()
    };

    DoctorReport {
        overall_health,
        git_repository: git_repo,
        scaffold_configured,
        scaffold_id: config.scaffold_id.clone(),
        mode: config.mode.clone(),
        graph_db_ready,
        graph_nodes,
        graph_edges,
        wiki_db_ready,
        wiki_entities,
        cozodb_ready,
        embedding,
        diagnostics,
        drift_score,
        drift_errors,
        drift_warnings,
        graph_status: health.status.clone(),
        graph_detail,
        graph_remediation,
        coverage,
        heartbeat_ok: hb.heartbeat_ok,
        heartbeat_detail,
        event_count,
        next_steps,
        exit_code: if drift_errors > 0 { 1 } else { 0 },
    }
}
