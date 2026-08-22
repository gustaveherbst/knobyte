//! Freshness gating for graph reads.
//!
//! Targeted reads (`graph query`, `graph get`, `impact`) answer only from a graph whose facts
//! still describe the working tree. Before any graph-derived output is produced the store is
//! inspected read-only ([`inspect_status`]):
//!
//! - a store that is missing, corrupt, needs a rebuild, or whose staleness cannot be bounded
//!   (corpus policy or extractor changed, change list truncated, another root) is refused with
//!   one `GRAPH_UNAVAILABLE` record carrying `graphStatus`, `reasonCode` and `recoveryCommand`;
//! - a store whose only shortfall is a known set of changed source files is served with those
//!   files excluded: facts located in them are dropped and the response carries a `status`
//!   record (`graphStatus: "stale"`, `excludedFiles`) instead of claiming to be current;
//! - a store with partially parsed files is served labelled `degraded`.
//!
//! `graph scope` handles drifted files itself (text-only evidence), so it only refuses an
//! unusable store ([`ReadGate::inspect_tolerant`]).

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;

use crate::graph::engine::GraphEngine;
use crate::graph::maintenance::graph_error;
use crate::graph::status::{inspect_status, GraphHealth};

/// The database a read session opened is not the publication its gate inspected.
pub const READER_DATABASE_CHANGED: &str = "GRAPH_INDEX_READER_DATABASE_CHANGED";

/// Why a read was refused (rendered as one `GRAPH_UNAVAILABLE` error).
#[derive(Debug, Clone)]
pub struct Unavailable {
    pub graph_status: String,
    pub reason_code: String,
    pub message: String,
    pub recovery_command: Option<String>,
}

impl Unavailable {
    /// Protocol record (`type: error`, `code: GRAPH_UNAVAILABLE`).
    pub fn record(&self) -> Value {
        let mut v = json!({
            "type": "error",
            "code": "GRAPH_UNAVAILABLE",
            "graphStatus": self.graph_status,
            "reasonCode": self.reason_code,
            "message": self.message,
        });
        if let Some(c) = &self.recovery_command {
            v["recoveryCommand"] = json!(c);
        }
        v
    }

    /// `--json` error object (`{"error": {...}}`) for the non-protocol commands.
    pub fn json_error(&self) -> Value {
        json!({ "error": {
            "code": self.reason_code,
            "graphStatus": self.graph_status,
            "message": self.message,
            "recoveryCommand": self.recovery_command,
        } })
    }

    fn from_health(h: &GraphHealth) -> Self {
        let diag = h
            .diagnostics
            .iter()
            .find(|d| d.severity == "error")
            .or_else(|| h.diagnostics.iter().find(|d| d.severity == "warning"))
            .or_else(|| h.diagnostics.first());
        let recovery = diag
            .and_then(|d| d.command.clone())
            .or_else(|| h.next_command().map(str::to_string));
        Self {
            graph_status: h.status.clone(),
            reason_code: diag.map(|d| d.code.clone()).unwrap_or_else(|| "GRAPH_UNAVAILABLE".into()),
            message: diag
                .map(|d| format!("{} No graph-derived result was returned.", sentence(&d.message)))
                .unwrap_or_else(|| format!("The graph is {}; no graph-derived result was returned.", h.status)),
            recovery_command: recovery,
        }
    }

    fn from_error(e: &rusqlite::Error, status: &str) -> Self {
        let m = graph_error(e);
        let status = if m.code == "GRAPH_INDEX_CORRUPT" {
            "corrupt"
        } else if m.code == "GRAPH_INDEX_MISSING" {
            "missing"
        } else if m.code == "GRAPH_INDEX_SCHEMA_INCOMPATIBLE" {
            "rebuild_required"
        } else if m.code == "GRAPH_INDEX_REPAIR_AVAILABLE" {
            "stale"
        } else {
            status
        };
        let recovery = match m.code.as_str() {
            "GRAPH_INDEX_CORRUPT" => "knobyte graph repair",
            "GRAPH_INDEX_REPAIR_AVAILABLE" => "knobyte graph refresh",
            _ => "knobyte graph rebuild",
        };
        Self {
            graph_status: status.to_string(),
            recovery_command: Some(recovery.into()),
            reason_code: m.code,
            message: m.message,
        }
    }
}

/// A diagnostic message as a sentence: raw errors ("file is not a database") get a full stop so
/// a following sentence does not run into them.
fn sentence(message: &str) -> String {
    let m = message.trim_end();
    if m.ends_with(['.', '!', '?']) {
        m.to_string()
    } else {
        format!("{}.", m)
    }
}

/// What a read may trust, from one read-only status inspection.
#[derive(Debug, Clone)]
pub struct ReadGate {
    pub health: GraphHealth,
    /// `source-drift` and/or `parse-degraded`; empty when the graph is fresh.
    pub degradations: Vec<&'static str>,
    /// Project-relative files whose indexed facts no longer describe the working tree (added,
    /// modified or deleted since the build). Facts in them are never returned.
    pub drifted: BTreeSet<String>,
}

impl ReadGate {
    /// Gate for targeted reads: refuses unless the staleness is exactly known.
    pub fn inspect(db_path: &Path, root: &Path) -> Result<Self, Unavailable> {
        Self::classify(inspect_status(db_path, root), false)
    }

    /// Gate for `graph scope`: refuses only an unusable store (missing, corrupt, rebuild).
    pub fn inspect_tolerant(db_path: &Path, root: &Path) -> Result<Self, Unavailable> {
        Self::classify(inspect_status(db_path, root), true)
    }

    fn classify(h: GraphHealth, tolerant: bool) -> Result<Self, Unavailable> {
        // An older schema awaiting its in-place upgrade (`stale`) is not readable as-is.
        if !h.inspected
            || h.schema_version.is_some_and(|v| v != crate::graph::schema::CURRENT_SCHEMA_VERSION)
        {
            return Err(Unavailable::from_health(&h));
        }
        match h.status.as_str() {
            "fresh" => Ok(Self { health: h, degradations: Vec::new(), drifted: BTreeSet::new() }),
            "degraded" => Ok(Self { health: h, degradations: vec!["parse-degraded"], drifted: BTreeSet::new() }),
            "stale" => {
                let c = &h.changes;
                let unbounded_reason = h
                    .diagnostics
                    .iter()
                    .any(|d| matches!(d.code.as_str(), "GRAPH_INDEX_ROOT_MISMATCH" | "GRAPH_CORPUS_LIMIT_EXCEEDED"))
                    || c.policy_changed
                    || c.extractor_changed
                    || c.grammar_changed
                    || c.truncated;
                if unbounded_reason && !tolerant {
                    return Err(Unavailable::from_health(&h));
                }
                let drifted: BTreeSet<String> =
                    c.added.iter().chain(&c.modified).chain(&c.deleted).cloned().collect();
                let mut degradations = Vec::new();
                if h.parse_health.partial > 0 || h.parse_health.failed > 0 {
                    degradations.push("parse-degraded");
                }
                if !drifted.is_empty() || unbounded_reason {
                    degradations.push("source-drift");
                }
                Ok(Self { health: h, degradations, drifted })
            }
            _ => Err(Unavailable::from_health(&h)),
        }
    }

    /// Open the engine strictly read-only for this gate, as an immutable read session: the
    /// engine is pinned to one snapshot (every query of the command answers from it) and that
    /// snapshot is proven to be the publication this gate inspected
    /// (`GRAPH_INDEX_READER_DATABASE_CHANGED` otherwise; [`ReadGate::open_session`] retries).
    pub fn open_engine(&self, db_path: &Path) -> Result<GraphEngine, Unavailable> {
        let engine =
            GraphEngine::open_read_only(db_path).map_err(|e| Unavailable::from_error(&e, &self.health.status))?;
        engine
            .pin_snapshot()
            .map_err(|e| Unavailable::from_error(&e, &self.health.status))?;
        self.verify(&engine)?;
        Ok(engine)
    }

    /// Inspect, then open an immutable read session on the inspected publication. A
    /// publication landing between the inspection and the open is re-inspected once.
    pub fn open_session(db_path: &Path, root: &Path, tolerant: bool) -> Result<(GraphEngine, ReadGate), Unavailable> {
        let mut last = None;
        for _ in 0..2 {
            let gate = if tolerant { Self::inspect_tolerant(db_path, root) } else { Self::inspect(db_path, root) }?;
            match gate.open_engine(db_path) {
                Ok(engine) => return Ok((engine, gate)),
                Err(u) if u.reason_code == READER_DATABASE_CHANGED => last = Some(u),
                Err(u) => return Err(u),
            }
        }
        Err(last.expect("two attempts"))
    }

    /// Prove that `engine` (inside a read snapshot) reads the publication this gate inspected.
    pub fn verify(&self, engine: &GraphEngine) -> Result<(), Unavailable> {
        if engine.publication_id() == self.health.publication_id {
            return Ok(());
        }
        Err(Unavailable {
            graph_status: self.health.status.clone(),
            reason_code: READER_DATABASE_CHANGED.to_string(),
            message: "The graph was republished after its freshness was inspected; no graph-derived result was returned. Retry."
                .to_string(),
            recovery_command: None,
        })
    }

    /// True when facts located in `file_path` must not be returned.
    pub fn is_drifted(&self, file_path: &str) -> bool {
        !file_path.is_empty() && self.drifted.contains(file_path)
    }

    pub fn source_drifted(&self) -> bool {
        self.degradations.contains(&"source-drift")
    }

    /// The response-level `status` record declaring a non-fresh answer; empty when fresh, so a
    /// fresh response is unchanged.
    pub fn status_records(&self) -> Vec<Value> {
        if self.degradations.is_empty() {
            return Vec::new();
        }
        let drifted = self.source_drifted();
        let reason = self.health.diagnostics.iter().find(|d| {
            d.code == if drifted { "GRAPH_SOURCES_CHANGED" } else { "GRAPH_PARSE_DEGRADED" }
        });
        let mut reasons: Vec<&str> = self.degradations.clone();
        reasons.sort();
        let mut v = json!({
            "type": "status",
            "graphStatus": if drifted { "stale" } else { "degraded" },
            "reasons": reasons,
            "message": reason.map(|d| d.message.clone()).unwrap_or_else(|| if drifted {
                "Some indexed files changed after this index was built and were excluded.".to_string()
            } else {
                "Some files could not be parsed completely when this index was built.".to_string()
            }),
            "trusted": ["definitions", "containment", "source"],
            "recoveryCommand": "knobyte graph refresh",
        });
        if let Some(r) = reason {
            v["reasonCode"] = json!(r.code);
        }
        if self.degradations.contains(&"parse-degraded") {
            v["incomplete"] = json!(["files that did not parse completely"]);
            v["partialFiles"] = json!(self.health.parse_health.partial);
            v["failedFiles"] = json!(self.health.parse_health.failed);
            if !self.health.parse_health.failed_paths.is_empty() {
                v["failedPaths"] = json!(self.health.parse_health.failed_paths);
            }
        }
        if !self.drifted.is_empty() {
            v["excludedFiles"] = json!(self.drifted.iter().collect::<Vec<_>>());
            v["excludedFileCount"] = json!(self.drifted.len());
        }
        vec![v]
    }

    /// Summary warning for a drifted answer.
    pub fn warning(&self) -> Option<String> {
        if !self.source_drifted() {
            return None;
        }
        Some(format!(
            "{} indexed file(s) changed since the last build and were excluded from this answer. Run `knobyte graph refresh`.",
            self.drifted.len()
        ))
    }

    /// `TARGET_SOURCE_DRIFTED` when every node `target` resolved to lies in an excluded file.
    pub fn target_drifted(&self, target: &str, file_paths: &[&str]) -> Option<Value> {
        if file_paths.is_empty() || !file_paths.iter().all(|p| self.is_drifted(p)) {
            return None;
        }
        let files: BTreeSet<&str> = file_paths.iter().copied().collect();
        Some(json!({
            "type": "error",
            "code": "TARGET_SOURCE_DRIFTED",
            "target": target,
            "filePaths": files,
            "message": "The target's file changed since the last build; its indexed facts no longer describe it.",
            "recoveryCommand": "knobyte graph refresh",
        }))
    }

    /// Coverage context for `TARGET_NOT_FOUND`: emitted only when it changes the record's
    /// meaning (nothing indexed, or recognised source files left unindexed).
    pub fn not_found_coverage(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        let files = self.health.counts.files;
        let cov = self.health.coverage.as_ref().filter(|c| c.unindexed_total > 0 || c.truncated);
        if files > 0 && cov.is_none() {
            return m;
        }
        m.insert("filesIndexed".into(), json!(files));
        if let Some(c) = cov {
            let by_ext: serde_json::Map<String, Value> =
                c.unindexed.iter().map(|e| (e.extension.clone(), json!(e.files))).collect();
            m.insert(
                "unindexedSources".into(),
                json!({ "total": c.unindexed_total, "byExtension": by_ext, "truncated": c.truncated, "observedAt": "last-build" }),
            );
        }
        m
    }
}
