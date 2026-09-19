use serde::{Deserialize, Serialize};
use crate::config::KnobyteConfig;
use crate::graph::GraphEngine;
use crate::wiki::WikiIndex;

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
    pub diagnostics: Vec<String>,
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

    let mut graph_db_ready = false;
    let mut graph_nodes = 0;
    let mut graph_edges = 0;

    let graph_path = config.graph_db_path();
    if graph_path.exists() {
        if let Ok(engine) = GraphEngine::open(&graph_path) {
            if let Ok(st) = engine.status() {
                graph_db_ready = true;
                graph_nodes = st.node_count;
                graph_edges = st.edge_count;
            }
        }
    } else {
        diagnostics.push("Code graph database (graph.db) not built. Run 'knobyte graph rebuild'.".to_string());
    }

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

    let overall_health = if diagnostics.is_empty() {
        "healthy".to_string()
    } else if scaffold_configured {
        "warning".to_string()
    } else {
        "action_required".to_string()
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
        diagnostics,
    }
}
