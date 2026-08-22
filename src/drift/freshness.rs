//! Code-graph freshness as seen by `knobyte check`: whether grounding can be verified against
//! the graph, and which source files changed since it was built.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::KnobyteConfig;
use crate::graph::fingerprint::compute_file_hash;
use crate::graph::status::{inspect_status, GraphHealth};
use crate::graph::{scan_corpus, CorpusPolicy, GraphEngine};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphState {
    /// Every indexed file is unchanged and no new source file appeared.
    Fresh,
    /// Source files changed since the last build, or the extractor / corpus policy changed.
    Stale,
    /// No graph has been built.
    Missing,
    /// The graph was built for a different project root, or has no recorded root.
    RebuildRequired,
    /// The graph database could not be opened or read.
    Degraded,
    /// The graph database failed its integrity check.
    Corrupt,
}

impl GraphState {
    pub fn as_str(&self) -> &'static str {
        match self {
            GraphState::Fresh => "fresh",
            GraphState::Stale => "stale",
            GraphState::Missing => "missing",
            GraphState::RebuildRequired => "rebuild_required",
            GraphState::Degraded => "degraded",
            GraphState::Corrupt => "corrupt",
        }
    }
}

/// Graph freshness plus the source changes that make it stale.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphFreshness {
    pub status: GraphState,
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub modified: Vec<String>,
    #[serde(default)]
    pub deleted: Vec<String>,
    /// Human-readable detail (diagnostic) for non-fresh states.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Command that repairs the state, when one applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
    /// Why no grounding can be trusted although source files may be unchanged (extractor or
    /// corpus policy changed since the build).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whole_graph: Option<String>,
    /// Indexed files that failed to parse (their symbols are absent from the graph).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unparsed: Vec<String>,
}

impl GraphFreshness {
    fn simple(status: GraphState, detail: &str, remediation: Option<&str>) -> Self {
        Self {
            status,
            added: Vec::new(),
            modified: Vec::new(),
            deleted: Vec::new(),
            detail: Some(detail.to_string()),
            remediation: remediation.map(str::to_string),
            whole_graph: None,
            unparsed: Vec::new(),
        }
    }

    pub fn change_count(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }

    /// Modified or deleted indexed files (whose snapshot nodes cannot be trusted).
    pub fn changed_files(&self) -> BTreeSet<&str> {
        self.modified
            .iter()
            .chain(self.deleted.iter())
            .map(String::as_str)
            .collect()
    }

    /// One-line summary, e.g. `graph stale · 3 source changes (1 added, 2 modified, 0 deleted)`.
    pub fn summary(&self) -> String {
        let mut s = format!("graph {}", self.status.as_str());
        if let Some(reason) = &self.whole_graph {
            s.push_str(&format!(" · {}", reason));
        } else if self.status == GraphState::Stale {
            let n = self.change_count();
            s.push_str(&format!(
                " · {} source change{} ({} added, {} modified, {} deleted)",
                n,
                if n == 1 { "" } else { "s" },
                self.added.len(),
                self.modified.len(),
                self.deleted.len()
            ));
        }
        if let Some(cmd) = &self.remediation {
            s.push_str(&format!(" · run `{}`", cmd));
        }
        s
    }
}

fn file_mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64
        })
        .unwrap_or(0)
}

/// Inspect graph freshness for `config` (read-only: the database is never created, opened for
/// writing or migrated).
pub fn inspect_graph(config: &KnobyteConfig) -> GraphFreshness {
    let db = config.graph_db_path();
    if !db.exists() {
        return GraphFreshness::simple(
            GraphState::Missing,
            "No code graph has been built.",
            Some("knobyte graph rebuild"),
        );
    }
    let health = inspect_status(&db, &config.project_root);
    if let Some(f) = blocking_state(&health) {
        return f;
    }
    let engine = match GraphEngine::open(&db) {
        Ok(e) => e,
        Err(e) => {
            return GraphFreshness::simple(
                GraphState::Degraded,
                &format!("Graph database could not be opened: {}", e),
                Some("knobyte graph rebuild"),
            )
        }
    };
    from_health(&engine, &config.project_root, health)
}

/// Inspect freshness of an open graph against `project_root`, consistently with
/// `knobyte graph status` (schema, rebuild marker, extractor, corpus policy, root, sources).
pub fn inspect_engine(engine: &GraphEngine, project_root: &Path) -> GraphFreshness {
    let health = inspect_status(engine.db_path(), project_root);
    if let Some(f) = blocking_state(&health) {
        return f;
    }
    from_health(engine, project_root, health)
}

fn first_diagnostic(health: &GraphHealth) -> String {
    health
        .diagnostics
        .iter()
        .find(|d| d.severity == "error")
        .or_else(|| health.diagnostics.first())
        .map(|d| d.message.clone())
        .unwrap_or_else(|| format!("The code graph is {}.", health.status))
}

/// States in which the graph cannot be used for grounding at all.
fn blocking_state(health: &GraphHealth) -> Option<GraphFreshness> {
    let state = match health.status.as_str() {
        "missing" => GraphState::Missing,
        "corrupt" => GraphState::Corrupt,
        "rebuild_required" => GraphState::RebuildRequired,
        _ => return None,
    };
    let fallback = if state == GraphState::Corrupt {
        "knobyte graph repair"
    } else {
        "knobyte graph rebuild"
    };
    Some(GraphFreshness::simple(
        state,
        &first_diagnostic(health),
        Some(health.next_command().unwrap_or(fallback)),
    ))
}

fn from_health(engine: &GraphEngine, project_root: &Path, health: GraphHealth) -> GraphFreshness {
    let conn = engine.connection();
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    match engine.indexed_project_root() {
        Some(root) if canon(&root) == canon(project_root) => {}
        Some(root) => {
            return GraphFreshness::simple(
                GraphState::RebuildRequired,
                &format!(
                    "The code graph was built for {}, not this project.",
                    root.display()
                ),
                Some("knobyte graph rebuild"),
            )
        }
        None => {
            return GraphFreshness::simple(
                GraphState::RebuildRequired,
                "The code graph has no recorded project root.",
                Some("knobyte graph rebuild"),
            )
        }
    }

    // Exact change lists (the status report truncates long lists).
    let (added, modified, deleted) = if health.changes.truncated {
        match full_changes(conn, project_root) {
            Some(c) => c,
            None => {
                return GraphFreshness::simple(
                    GraphState::Degraded,
                    "Graph database could not be read.",
                    Some("knobyte graph rebuild"),
                )
            }
        }
    } else {
        (
            health.changes.added.clone(),
            health.changes.modified.clone(),
            health.changes.deleted.clone(),
        )
    };

    let whole_graph = if health.changes.extractor_changed {
        Some(format!(
            "the graph was built by extractor {} (current {})",
            health.extractor_version.as_deref().unwrap_or("unknown"),
            crate::graph::extractor::EXTRACTOR_VERSION
        ))
    } else if health.changes.policy_changed {
        Some("the graph corpus policy (graph.ignore / limits) changed since the last build".to_string())
    } else {
        None
    };

    let unparsed: Vec<String> = conn
        .prepare("SELECT path FROM files WHERE parse_status = 'failed' ORDER BY path")
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(0))
                .map(|it| it.flatten().collect())
        })
        .unwrap_or_default();

    let stale = whole_graph.is_some() || !(added.is_empty() && modified.is_empty() && deleted.is_empty());
    GraphFreshness {
        status: if stale {
            GraphState::Stale
        } else {
            GraphState::Fresh
        },
        detail: if let Some(reason) = &whole_graph {
            Some(format!("Every grounding needs a refresh: {}.", reason))
        } else {
            stale.then(|| "Source files changed since the code graph was built.".to_string())
        },
        remediation: stale.then(|| "knobyte graph refresh".to_string()),
        added,
        modified,
        deleted,
        whole_graph,
        unparsed,
    }
}

type ChangeLists = (Vec<String>, Vec<String>, Vec<String>);

fn full_changes(conn: &rusqlite::Connection, project_root: &Path) -> Option<ChangeLists> {
    let indexed: HashMap<String, (String, i64)> = conn
        .prepare("SELECT path, content_hash, modified_at FROM files")
        .and_then(|mut stmt| {
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
            rows.collect::<rusqlite::Result<HashMap<_, _>>>()
        })
        .ok()?;
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    for (path, (hash, mtime)) in &indexed {
        let full = project_root.join(path);
        if !full.exists() {
            deleted.push(path.clone());
            continue;
        }
        if file_mtime_ms(&full) != *mtime {
            match fs::read(&full) {
                Ok(bytes) if compute_file_hash(&bytes) == *hash => {}
                _ => modified.push(path.clone()),
            }
        }
    }
    let policy = CorpusPolicy::for_project(project_root);
    let mut added: Vec<String> = scan_corpus(project_root, &policy)
        .map(|scan| scan.files.into_iter().map(|f| f.rel_path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !indexed.contains_key(p))
        .collect();
    added.sort();
    modified.sort();
    deleted.sort();
    Some((added, modified, deleted))
}
