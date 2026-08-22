use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use crate::graph::build::metadata_value;
pub use crate::graph::build::BuildSummary;
use crate::graph::corpus::{scan_corpus, CorpusPolicy, CorpusScan, CoverageReport};
pub use crate::graph::corpus::IndexableFile;
use crate::graph::fingerprint::compute_file_hash;
use crate::graph::grounding::{
    ground_documents, resolve_grounding_ref, scan_scaffold_groundings, RefResolution,
};
use crate::graph::lock::MaintenanceLock;
use crate::graph::maintenance::{
    classify_db_error, cleanup_stale_candidates, rebuild_locked, refresh_locked,
    GraphMaintenanceError, RefreshOutcome,
};
use crate::graph::models::{GraphStatus, Node, ScopedNode};
use crate::graph::schema::{initialize_graph_schema, stored_schema_version, CURRENT_SCHEMA_VERSION};
use crate::progress::IndexProgressBar;

/// Column list matching [`map_node_row`].
pub(crate) const NODE_COLUMNS: &str =
    "id, kind, name, qualified_name, container_id, identity_key, file_path, language, \
     start_line, end_line, start_column, end_column, docstring, signature, visibility, \
     is_exported, is_async, is_static, is_abstract, return_type, body_hash, updated_at";

/// Same as [`NODE_COLUMNS`] with an `n.` table alias.
pub(crate) const NODE_COLUMNS_N: &str = "n.id, n.kind, n.name, n.qualified_name, n.container_id, n.identity_key, n.file_path, n.language, \
     n.start_line, n.end_line, n.start_column, n.end_column, n.docstring, n.signature, n.visibility, \
     n.is_exported, n.is_async, n.is_static, n.is_abstract, n.return_type, n.body_hash, n.updated_at";

/// Edge kinds that mean "source calls / constructs target".
pub const CALL_EDGE_KINDS: [&str; 4] = ["calls", "calls_trait_method", "possible_call", "instantiates"];

/// Edge kinds that are structure, not dependency (never followed by impact analysis).
const STRUCTURAL_EDGE_KINDS: [&str; 2] = ["contains", "exports"];

/// Hard ceilings for impact traversal.
pub const MAX_IMPACT_DEPTH: usize = 8;
pub const MAX_IMPACT_NODES: usize = 500;
/// Default and maximum source lines returned per node by `graph get --source`.
pub const DEFAULT_SOURCE_LINES: usize = 200;
pub const MAX_SOURCE_LINES: usize = 400;

/// All indexable files under `root`, under the project's corpus policy
/// (`.knobyte/config.json` -> `graph`). Files beyond the policy limits are not returned.
pub fn scan_indexable_files(root: &Path) -> (Vec<IndexableFile>, u64) {
    let policy = CorpusPolicy::for_project(root);
    match scan_corpus(root, &policy) {
        Ok(scan) => (scan.files, scan.total_bytes),
        Err(_) => {
            let unbounded = CorpusPolicy {
                max_files: usize::MAX,
                max_total_bytes: u64::MAX,
                ..policy
            };
            scan_corpus(root, &unbounded)
                .map(|s| (s.files, s.total_bytes))
                .unwrap_or_default()
        }
    }
}

fn corpus_error(e: crate::graph::corpus::CorpusLimitError) -> rusqlite::Error {
    GraphMaintenanceError::new("GRAPH_CORPUS_LIMIT_EXCEEDED", e.to_string()).into()
}

/// How impact analysis walks the graph.
#[derive(Debug, Clone, Copy)]
pub struct ImpactOptions {
    /// Maximum traversal depth (clamped to [`MAX_IMPACT_DEPTH`]).
    pub depth: usize,
    /// Follow only call/instantiation edges (transitive callers) instead of every dependency.
    pub callers_only: bool,
}

impl Default for ImpactOptions {
    fn default() -> Self {
        Self {
            depth: 3,
            callers_only: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactEntry {
    #[serde(flatten)]
    pub node: Node,
    pub depth: usize,
    /// Edge kind through which this node depends on the previous one.
    pub via: String,
    /// Id of the root this node was reached from.
    pub root: String,
}

/// A scaffold document grounded to an impacted symbol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroundingHit {
    /// Scaffold-relative document path.
    pub doc: String,
    /// Grounding reference as written in the document.
    pub reference: String,
    pub node_id: String,
    pub qualified_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactReport {
    pub target: String,
    pub roots: Vec<Node>,
    pub impacted: Vec<ImpactEntry>,
    /// Scaffold documents grounded to the target or any impacted node.
    pub groundings: Vec<GroundingHit>,
    pub truncated: bool,
}

/// A call site the resolver captured but could not bind to one declaration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnresolvedCallSite {
    pub name: String,
    pub reference_kind: String,
    /// `ambiguous`, `unresolved` or `pending`.
    pub status: String,
    pub file_path: String,
    pub line: i64,
    pub col: i64,
    pub from_node_id: String,
    pub receiver: Option<String>,
    pub qualifier: Option<String>,
}

/// A node with its source text (bounded).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeSource {
    #[serde(flatten)]
    pub node: Node,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub source_start_line: i64,
    pub source_end_line: i64,
    /// The declaration has more lines than were returned.
    pub truncated: bool,
    /// The file changed since it was indexed; the returned lines may not match the node.
    pub stale: bool,
}

pub struct GraphEngine {
    conn: Connection,
    db_path: PathBuf,
    /// False while `db_path` does not exist: the engine then reads an empty in-memory graph and
    /// the file is only created by a build.
    materialized: bool,
}

/// A read transaction pinning one consistent snapshot of the graph: queries made while it is
/// alive all see the same published build, even if a rebuild publishes meanwhile. Nested
/// snapshots (or a snapshot taken while the engine is pinned, see
/// [`GraphEngine::pin_snapshot`]) reuse the enclosing transaction.
pub struct ReadSnapshot<'a> {
    conn: &'a Connection,
    owned: bool,
}

impl Drop for ReadSnapshot<'_> {
    fn drop(&mut self) {
        if self.owned {
            let _ = self.conn.execute_batch("COMMIT");
        }
    }
}

impl GraphEngine {
    /// Open the graph at `db_path` for maintenance and baseline writes.
    ///
    /// Opening never creates the database: when `db_path` does not exist the engine reads an
    /// empty graph and the file is created by the first build ([`GraphEngine::rebuild`],
    /// [`GraphEngine::refresh`]). A database recorded under another schema version is opened
    /// without any schema change. Read-only commands use [`GraphEngine::open_read_only`].
    pub fn open(db_path: &Path) -> Result<Self> {
        if !db_path.exists() {
            let conn = Connection::open_in_memory()?;
            initialize_graph_schema(&conn)?;
            return Ok(Self {
                conn,
                db_path: db_path.to_path_buf(),
                materialized: false,
            });
        }
        let conn = Connection::open(db_path).map_err(|e| classify_db_error(&e))?;
        initialize_graph_schema(&conn).map_err(|e| classify_db_error(&e))?;
        Ok(Self {
            conn,
            db_path: db_path.to_path_buf(),
            materialized: true,
        })
    }

    /// Open an existing graph strictly read-only (`SQLITE_OPEN_READ_ONLY`): nothing is created,
    /// no DDL runs. Fails with `GRAPH_INDEX_MISSING` when there is no database,
    /// `GRAPH_INDEX_CORRUPT` when it is unreadable, and `GRAPH_INDEX_REPAIR_AVAILABLE` when it
    /// records an older schema upgradable in place, and `GRAPH_INDEX_SCHEMA_INCOMPATIBLE` when it
    /// records another schema version.
    pub fn open_read_only(db_path: &Path) -> Result<Self> {
        if !db_path.exists() {
            return Err(GraphMaintenanceError::new(
                "GRAPH_INDEX_MISSING",
                "No code graph has been built for this project. Run `knobyte graph rebuild`.",
            )
            .into());
        }
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| classify_db_error(&e))?;
        let _ = conn.pragma_update(None, "busy_timeout", 5000);
        let version = stored_schema_version(&conn).map_err(|e| classify_db_error(&e))?;
        match version {
            Some(v) if v == CURRENT_SCHEMA_VERSION => {}
            Some(v) if crate::graph::schema::is_migratable(v) => {
                return Err(GraphMaintenanceError::new(
                    "GRAPH_INDEX_REPAIR_AVAILABLE",
                    format!(
                        "The graph index uses schema v{} (this Knobyte reads v{}). Run `knobyte graph refresh` \
                         to upgrade it in place.",
                        v, CURRENT_SCHEMA_VERSION
                    ),
                )
                .into())
            }
            Some(v) => {
                return Err(GraphMaintenanceError::new(
                    "GRAPH_INDEX_SCHEMA_INCOMPATIBLE",
                    format!(
                        "The graph index uses schema v{} (this Knobyte reads v{}). Run `knobyte graph rebuild`.",
                        v, CURRENT_SCHEMA_VERSION
                    ),
                )
                .into())
            }
            None => {
                return Err(GraphMaintenanceError::new(
                    "GRAPH_INDEX_CORRUPT",
                    "The graph database has no schema history. Run `knobyte graph rebuild`.",
                )
                .into())
            }
        }
        Ok(Self {
            conn,
            db_path: db_path.to_path_buf(),
            materialized: true,
        })
    }

    /// Create the database file when it does not exist yet (first build).
    fn materialize(&mut self) -> Result<()> {
        if self.materialized {
            return Ok(());
        }
        if let Some(parent) = self.db_path.parent() {
            let _ = fs::create_dir_all(parent);
            crate::graph::maintenance::ensure_local_gitignore(&self.db_path);
        }
        let conn = Connection::open(&self.db_path)?;
        initialize_graph_schema(&conn)?;
        self.conn = conn;
        self.materialized = true;
        Ok(())
    }

    /// Underlying SQLite connection (read access for callers that need custom queries).
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Begin a consistent read snapshot (see [`ReadSnapshot`]).
    pub fn read_snapshot(&self) -> Result<ReadSnapshot<'_>> {
        if !self.conn.is_autocommit() {
            return Ok(ReadSnapshot { conn: &self.conn, owned: false });
        }
        self.conn.execute_batch("BEGIN DEFERRED")?;
        // The snapshot is taken by the first read.
        if let Err(e) = self
            .conn
            .query_row("SELECT COUNT(*) FROM schema_versions", [], |r| r.get::<_, i64>(0))
        {
            let _ = self.conn.execute_batch("ROLLBACK");
            return Err(e);
        }
        Ok(ReadSnapshot { conn: &self.conn, owned: true })
    }

    /// Pin one consistent snapshot for the rest of this engine's life (an immutable read
    /// session): every later query, from any number of facades, answers from the same
    /// publication. Meant for read-only engines serving one command or request; a pinned
    /// engine cannot run maintenance. Idempotent.
    pub fn pin_snapshot(&self) -> Result<()> {
        if self.conn.is_autocommit() {
            std::mem::forget(self.read_snapshot()?);
        }
        Ok(())
    }

    /// End a pinned snapshot (no-op when none is open).
    pub fn unpin_snapshot(&self) {
        if !self.conn.is_autocommit() {
            let _ = self.conn.execute_batch("COMMIT");
        }
    }

    /// True while queries are pinned to one snapshot.
    pub fn is_pinned(&self) -> bool {
        !self.conn.is_autocommit()
    }

    /// Id of the publication this engine reads (inside a pinned snapshot: the pinned one).
    pub fn publication_id(&self) -> Option<String> {
        crate::graph::snapshot::publication_id(&self.conn)
    }

    pub fn status(&self) -> Result<GraphStatus> {
        let node_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap_or(0);
        let edge_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
            .unwrap_or(0);
        let file_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap_or(0);
        let unresolved_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM unresolved_refs", [], |r| r.get(0))
            .unwrap_or(0);
        let schema_version: i64 = self
            .conn
            .query_row("SELECT MAX(version) FROM schema_versions", [], |r| r.get(0))
            .unwrap_or(0);

        let last_indexed: Option<String> = self.metadata("last_build_time");

        Ok(GraphStatus {
            up_to_date: self.is_up_to_date(),
            schema_version,
            file_count,
            node_count,
            edge_count,
            unresolved_count,
            last_indexed,
        })
    }

    fn metadata(&self, key: &str) -> Option<String> {
        metadata_value(&self.conn, key)
    }

    /// Project root recorded by the last build, if any.
    pub fn indexed_project_root(&self) -> Option<PathBuf> {
        self.metadata("project_root").map(PathBuf::from)
    }

    /// Full health report (see [`crate::graph::status::inspect_status`]) for the indexed root.
    pub fn health(&self) -> crate::graph::status::GraphHealth {
        let root = self
            .indexed_project_root()
            .unwrap_or_else(|| self.db_path.parent().and_then(|p| p.parent()).unwrap_or(Path::new(".")).to_path_buf());
        crate::graph::status::inspect_status(&self.db_path, &root)
    }

    /// True when the graph is `fresh` or only `degraded` by parse errors: every indexed file is
    /// unchanged on disk, no new indexable file appeared, and the corpus policy is unchanged.
    pub fn is_up_to_date(&self) -> bool {
        match self.indexed_project_root() {
            Some(r) if r.exists() => {}
            _ => return false,
        }
        matches!(self.health().status.as_str(), "fresh" | "degraded")
    }

    fn maintenance_lock(&self) -> Result<MaintenanceLock> {
        let lock = MaintenanceLock::acquire(&self.db_path)?;
        cleanup_stale_candidates(&self.db_path);
        Ok(lock)
    }

    /// Full rebuild of the corpus under `root`, published atomically.
    pub fn rebuild(&mut self, root: &Path) -> Result<BuildSummary> {
        self.rebuild_with_progress(root, None)
    }

    pub fn rebuild_with_progress(
        &mut self,
        root: &Path,
        progress: Option<&IndexProgressBar>,
    ) -> Result<BuildSummary> {
        let scan = scan_corpus(root, &CorpusPolicy::for_project(root)).map_err(corpus_error)?;
        self.materialize()?;
        let _lock = self.maintenance_lock()?;
        rebuild_locked(&self.conn, &self.db_path, root, &scan, progress)
    }

    /// Full rebuild of exactly `files`: the graph is built into a candidate and published
    /// atomically, so a failure or crash mid-build leaves the previous graph intact. Grounding
    /// baselines (`_knobyte_grounded_source`) are preserved.
    pub fn rebuild_files(
        &mut self,
        root: &Path,
        files: &[IndexableFile],
        progress: Option<&IndexProgressBar>,
    ) -> Result<BuildSummary> {
        let scan = CorpusScan {
            files: files.to_vec(),
            total_bytes: files.iter().map(|f| f.size).sum(),
            coverage: CoverageReport::default(),
            policy: CorpusPolicy::for_project(root),
        };
        self.materialize()?;
        let _lock = self.maintenance_lock()?;
        rebuild_locked(&self.conn, &self.db_path, root, &scan, progress)
    }

    /// Incremental refresh: only changed files are re-extracted (content-hash cache); the
    /// graph is re-resolved and published atomically. A no-op when nothing changed.
    pub fn refresh(
        &mut self,
        root: &Path,
        progress: Option<&IndexProgressBar>,
    ) -> Result<RefreshOutcome> {
        let scan = scan_corpus(root, &CorpusPolicy::for_project(root)).map_err(corpus_error)?;
        self.materialize()?;
        let _lock = self.maintenance_lock()?;
        refresh_locked(&self.conn, &self.db_path, root, &scan, progress)
    }

    /// Incremental build (alias of [`GraphEngine::refresh`] returning only the summary).
    pub fn build(&mut self, root: &Path) -> Result<BuildSummary> {
        self.build_with_progress(root, None)
    }

    pub fn build_with_progress(
        &mut self,
        root: &Path,
        progress: Option<&IndexProgressBar>,
    ) -> Result<BuildSummary> {
        Ok(self.refresh(root, progress)?.summary)
    }

    /// Build exactly `files` (atomic full build; see [`GraphEngine::rebuild_files`]).
    pub fn build_files(
        &mut self,
        root: &Path,
        files: &[IndexableFile],
        progress: Option<&IndexProgressBar>,
    ) -> Result<BuildSummary> {
        self.rebuild_files(root, files, progress)
    }

    /// Resolve a grounding reference (`kind:path:qualified_name` or a legacy hashed id).
    pub fn resolve_ref(&self, reference: &str) -> Result<RefResolution> {
        resolve_grounding_ref(&self.conn, reference)
    }

    /// Declarations named exactly `name`: a node id, an exact `name`, or an exact
    /// `qualified_name`. Never a fuzzy or suffix match (broad retrieval is `graph scope`).
    pub fn query_where_defined(&self, name: &str) -> Result<Vec<Node>> {
        if let Some(n) = Self::node_by_id(&self.conn, name) {
            if !matches!(n.kind.as_str(), "module" | "file") {
                return Ok(vec![n]);
            }
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM nodes WHERE (name = ?1 OR qualified_name = ?1) \
             AND kind NOT IN ('module', 'file') ORDER BY file_path, start_line, id",
            NODE_COLUMNS
        ))?;
        let rows = stmt.query_map(params![name], Self::map_node)?;
        rows.collect()
    }

    /// Direct callers of node `id` with the line of their first call site, ordered tests and
    /// other low-value paths last, then by call-site line, then by id. One entry per caller.
    pub fn callers_of(&self, id: &str) -> Result<Vec<(Node, Option<i64>)>> {
        self.call_neighbours(id, true)
    }

    /// Direct callees of node `id` (same ordering as [`GraphEngine::callers_of`]).
    pub fn callees_of(&self, id: &str) -> Result<Vec<(Node, Option<i64>)>> {
        self.call_neighbours(id, false)
    }

    fn call_neighbours(&self, id: &str, callers: bool) -> Result<Vec<(Node, Option<i64>)>> {
        let (join, filter) = if callers { ("e.source", "e.target") } else { ("e.target", "e.source") };
        let kinds = CALL_EDGE_KINDS.iter().map(|k| format!("'{}'", k)).collect::<Vec<_>>().join(", ");
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {cols}, MIN(e.line) FROM edges e JOIN nodes n ON n.id = {join} \
             WHERE {filter} = ?1 AND e.kind IN ({kinds}) GROUP BY n.id",
            cols = NODE_COLUMNS_N
        ))?;
        let mut rows: Vec<(Node, Option<i64>)> = stmt
            .query_map(params![id], |r| Ok((map_node_row(r)?, r.get::<_, Option<i64>>(22)?)))?
            .collect::<Result<_>>()?;
        rows.sort_by(|(a, la), (b, lb)| {
            let low = |n: &Node| crate::graph::query_plan::is_low_value_graph_path(&n.file_path);
            low(a)
                .cmp(&low(b))
                .then(la.unwrap_or(i64::MAX).cmp(&lb.unwrap_or(i64::MAX)))
                .then(a.id.cmp(&b.id))
        });
        Ok(rows)
    }

    /// Call sites of `name` the resolver recorded but could not bind (status other than
    /// `resolved`), matched on the called name. Bounded by `limit`; returns (total, rows).
    pub fn unresolved_call_sites(&self, name: &str, limit: usize) -> Result<(usize, Vec<UnresolvedCallSite>)> {
        let mut stmt = self.conn.prepare(
            "SELECT reference_name, reference_kind, status, file_path, line, col, from_node_id, receiver, qualifier \
             FROM unresolved_refs WHERE status <> 'resolved' AND reference_kind = 'call' \
               AND (reference_name = ?1 OR instr(reference_name, ?1) > 0) \
             ORDER BY file_path, line, col, from_node_id",
        )?;
        let all: Vec<UnresolvedCallSite> = stmt
            .query_map(params![name], |r| {
                Ok(UnresolvedCallSite {
                    name: r.get(0)?,
                    reference_kind: r.get(1)?,
                    status: r.get(2)?,
                    file_path: r.get(3)?,
                    line: r.get(4)?,
                    col: r.get(5)?,
                    from_node_id: r.get(6)?,
                    receiver: r.get(7)?,
                    qualifier: r.get(8)?,
                })
            })?
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|u| {
                u.name == name
                    || u.name.strip_suffix(name).is_some_and(|p| p.ends_with('.') || p.ends_with("::"))
            })
            .collect();
        let total = all.len();
        Ok((total, all.into_iter().take(limit).collect()))
    }

    /// Who calls `target_name` (resolved exactly as [`GraphEngine::query_where_defined`]): the
    /// direct callers of every matching declaration, then callers whose call matched several
    /// same-named declarations (ambiguous, not linked). Ordered tests last, then call-site line.
    pub fn query_who_calls(&self, target_name: &str) -> Result<Vec<Node>> {
        let mut roots = self.query_where_defined(target_name)?;
        roots.sort_by(|a, b| a.id.cmp(&b.id));
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for r in &roots {
            for (n, _) in self.callers_of(&r.id)? {
                if seen.insert(n.id.clone()) {
                    out.push(n);
                }
            }
        }
        // Call sites that matched several same-named declarations (`name`, `recv.name`,
        // `path::name`), in call-site order.
        let (_, sites) = self.unresolved_call_sites(target_name, usize::MAX)?;
        for s in sites.iter().filter(|s| s.status == "ambiguous") {
            if seen.contains(&s.from_node_id) {
                continue;
            }
            if let Some(n) = Self::node_by_id(&self.conn, &s.from_node_id) {
                seen.insert(n.id.clone());
                out.push(n);
            }
        }
        Ok(out)
    }

    /// What `source_name` calls: callees of every declaration named exactly `source_name`, or of
    /// the file node when `source_name` is an indexed path.
    pub fn query_what_calls(&self, source_name: &str) -> Result<Vec<Node>> {
        let mut sources = self.query_where_defined(source_name)?;
        let mut file_stmt = self.conn.prepare(&format!(
            "SELECT {} FROM nodes WHERE kind = 'file' AND file_path = ?1",
            NODE_COLUMNS
        ))?;
        sources.extend(file_stmt.query_map(params![source_name], Self::map_node)?.collect::<Result<Vec<_>>>()?);
        sources.sort_by(|a, b| a.id.cmp(&b.id));
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for s in &sources {
            for (n, _) in self.callees_of(&s.id)? {
                if seen.insert(n.id.clone()) {
                    out.push(n);
                }
            }
        }
        Ok(out)
    }

    /// Nodes an impact query starts from: every declaration of a file (when `target` is an
    /// indexed path), a grounding reference, or symbols matching the name.
    pub fn impact_roots(&self, target: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM nodes WHERE file_path = ?1 ORDER BY start_line",
            NODE_COLUMNS
        ))?;
        let file_nodes: Vec<Node> = stmt
            .query_map(params![target], Self::map_node)?
            .collect::<Result<_>>()?;
        if !file_nodes.is_empty() {
            return Ok(file_nodes);
        }
        if target.contains(':') {
            match self.resolve_ref(target)? {
                RefResolution::Resolved(n) => return Ok(vec![*n]),
                RefResolution::Ambiguous(c) => return Ok(c),
                RefResolution::Missing => {}
            }
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {} FROM nodes WHERE (name = ?1 OR qualified_name = ?1) AND kind NOT IN ('file', 'module')              ORDER BY file_path, start_line",
            NODE_COLUMNS
        ))?;
        let rows = stmt.query_map(params![target], Self::map_node)?;
        rows.collect()
    }

    /// Blast radius of `target`: nodes that (transitively) depend on it, breadth-first up to
    /// `opts.depth`, following only call edges when `opts.callers_only`. Structural
    /// `contains`/`exports` edges are never followed.
    pub fn impact(&self, target: &str, opts: ImpactOptions) -> Result<ImpactReport> {
        let _snapshot = self.read_snapshot()?;
        let depth = opts.depth.clamp(1, MAX_IMPACT_DEPTH);
        let roots = self.impact_roots(target)?;
        let kinds_filter = if opts.callers_only {
            format!(
                "e.kind IN ({})",
                CALL_EDGE_KINDS.iter().map(|k| format!("'{}'", k)).collect::<Vec<_>>().join(", ")
            )
        } else {
            format!(
                "e.kind NOT IN ({})",
                STRUCTURAL_EDGE_KINDS.iter().map(|k| format!("'{}'", k)).collect::<Vec<_>>().join(", ")
            )
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {}, e.kind FROM edges e JOIN nodes n ON n.id = e.source WHERE e.target = ?1 AND {}              ORDER BY n.id",
            NODE_COLUMNS_N, kinds_filter
        ))?;
        let mut seen: HashSet<String> = roots.iter().map(|n| n.id.clone()).collect();
        let mut queue: VecDeque<(String, usize, String)> = roots
            .iter()
            .map(|n| (n.id.clone(), 0usize, n.id.clone()))
            .collect();
        let mut impacted = Vec::new();
        let mut truncated = false;
        while let Some((id, d, root)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            let rows: Vec<(Node, String)> = stmt
                .query_map(params![id], |r| Ok((map_node_row(r)?, r.get::<_, String>(22)?)))?
                .collect::<Result<_>>()?;
            for (n, kind) in rows {
                if !seen.insert(n.id.clone()) {
                    continue;
                }
                if roots.len() + impacted.len() >= MAX_IMPACT_NODES {
                    truncated = true;
                    break;
                }
                queue.push_back((n.id.clone(), d + 1, root.clone()));
                impacted.push(ImpactEntry {
                    node: n,
                    depth: d + 1,
                    via: kind,
                    root: root.clone(),
                });
            }
        }
        impacted.sort_by(|a, b| a.depth.cmp(&b.depth).then(a.node.id.cmp(&b.node.id)));
        Ok(ImpactReport {
            target: target.to_string(),
            roots,
            impacted,
            groundings: Vec::new(),
            truncated,
        })
    }

    /// Scaffold documents (under `scaffold_root`) whose grounding references resolve to any of
    /// `node_ids`, read from the committed documents (not the baseline cache).
    pub fn groundings_for(&self, scaffold_root: &Path, node_ids: &HashSet<String>) -> Vec<GroundingHit> {
        let mut hits = Vec::new();
        if node_ids.is_empty() {
            return hits;
        }
        let mut resolved: HashMap<String, Option<Node>> = HashMap::new();
        for doc in scan_scaffold_groundings(scaffold_root) {
            for reference in &doc.refs {
                let node = resolved
                    .entry(reference.clone())
                    .or_insert_with(|| {
                        resolve_grounding_ref(&self.conn, reference)
                            .ok()
                            .and_then(|r| r.node().cloned())
                    })
                    .clone();
                if let Some(n) = node.filter(|n| node_ids.contains(&n.id)) {
                    let hit = GroundingHit {
                        doc: doc.doc.clone(),
                        reference: reference.clone(),
                        node_id: n.id.clone(),
                        qualified_name: n.qualified_name.clone(),
                    };
                    if !hits.contains(&hit) {
                        hits.push(hit);
                    }
                }
            }
        }
        hits.sort_by(|a, b| a.doc.cmp(&b.doc).then(a.reference.cmp(&b.reference)));
        hits
    }

    /// [`GraphEngine::impact`] plus the scaffold documents grounded to the target or any
    /// impacted node.
    pub fn impact_with_groundings(
        &self,
        target: &str,
        opts: ImpactOptions,
        scaffold_root: &Path,
    ) -> Result<ImpactReport> {
        let mut report = self.impact(target, opts)?;
        let ids: HashSet<String> = report
            .roots
            .iter()
            .map(|n| n.id.clone())
            .chain(report.impacted.iter().map(|e| e.node.id.clone()))
            .collect();
        report.groundings = self.groundings_for(scaffold_root, &ids);
        Ok(report)
    }

    /// Nodes by id (or grounding reference), each with up to `max_lines` lines of its source
    /// read from `project_root`.
    pub fn get_with_source(
        &self,
        project_root: &Path,
        ids: &[String],
        max_lines: usize,
    ) -> Result<Vec<NodeSource>> {
        let max_lines = max_lines.clamp(1, MAX_SOURCE_LINES);
        let mut out = Vec::new();
        let mut file_cache: HashMap<String, Option<(String, bool)>> = HashMap::new();
        for id in ids {
            let node = match self.get_nodes(std::slice::from_ref(id))?.into_iter().next() {
                Some(n) => n,
                None => match self.resolve_ref(id)? {
                    RefResolution::Resolved(n) => *n,
                    _ => continue,
                },
            };
            let file = file_cache
                .entry(node.file_path.clone())
                .or_insert_with(|| {
                    if node.file_path.is_empty() {
                        return None;
                    }
                    let bytes = fs::read(project_root.join(&node.file_path)).ok()?;
                    let indexed: Option<String> = self
                        .conn
                        .query_row(
                            "SELECT content_hash FROM files WHERE path = ?1",
                            params![node.file_path],
                            |r| r.get(0),
                        )
                        .optional()
                        .ok()
                        .flatten();
                    let stale = indexed.as_deref() != Some(compute_file_hash(&bytes).as_str());
                    Some((String::from_utf8_lossy(&bytes).to_string(), stale))
                })
                .clone();
            let (source, start, end, truncated, stale) = match file {
                Some((content, stale)) if node.start_line > 0 => {
                    let lines: Vec<&str> = content.lines().collect();
                    let start = (node.start_line.max(1) as usize).min(lines.len().max(1));
                    let end = (node.end_line.max(node.start_line) as usize).min(lines.len());
                    let cap = (start + max_lines - 1).min(end);
                    let text = if start <= cap && start <= lines.len() {
                        lines[start - 1..cap].join("\n")
                    } else {
                        String::new()
                    };
                    (Some(text), start as i64, cap as i64, cap < end, stale)
                }
                _ => (None, node.start_line, node.end_line, false, false),
            };
            out.push(NodeSource {
                node,
                source,
                source_start_line: start,
                source_end_line: end,
                truncated,
                stale,
            });
        }
        Ok(out)
    }

    /// Files that import `target`, using structured import bindings.
    ///
    /// `target` may be a file path (`src/auth.rs`, with or without extension), a file stem
    /// (`auth`), a module specifier (`crate::auth`, `./auth`, `pkg.auth`, `react`) or an imported
    /// symbol name. Returns the importing files' `file` nodes.
    pub fn query_who_imports(&self, target_path: &str) -> Result<Vec<Node>> {
        let target = target_path.trim();
        let mut stmt = self.conn.prepare(
            r#"
            SELECT b.file_path, b.module_specifier, b.imported_name, b.local_name,
                   b.resolved_file_path, t.name, t.qualified_name, t.file_path
            FROM import_bindings b
            LEFT JOIN nodes t ON t.id = b.target_id
            "#,
        )?;
        type Row = (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<Row> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            })?
            .collect::<Result<Vec<_>>>()?;

        fn strip_ext(p: &str) -> String {
            match p.rsplit_once('.') {
                Some((stem, ext)) if !ext.contains('/') && !stem.is_empty() => stem.to_string(),
                _ => p.to_string(),
            }
        }
        fn stem_of(p: &str) -> String {
            let file = p.rsplit('/').next().unwrap_or(p);
            let stem = strip_ext(file);
            if stem == "mod" || stem == "index" || stem == "__init__" {
                // Directory modules are named by their directory.
                p.rsplit('/').nth(1).unwrap_or(&stem).to_string()
            } else {
                stem
            }
        }
        fn last_module_segment(m: &str) -> &str {
            m.rsplit(['/', ':', '.'])
                .find(|s| !s.is_empty())
                .unwrap_or(m)
        }
        let target_no_ext = strip_ext(target);

        let mut importers: Vec<String> = Vec::new();
        for (file, module, imported, local, resolved, t_name, t_qual, t_file) in rows {
            let resolved_file = resolved.or(t_file).filter(|f| !f.is_empty());
            let file_match = resolved_file
                .as_deref()
                .map(|f| f == target || strip_ext(f) == target_no_ext || stem_of(f) == target)
                .unwrap_or(false);
            let module_match = module == target || last_module_segment(&module) == target;
            let symbol_match = imported == target
                || local == target
                || t_name.as_deref() == Some(target)
                || t_qual.as_deref() == Some(target);
            if (file_match || module_match || symbol_match) && !importers.contains(&file) {
                importers.push(file);
            }
        }
        importers.sort();

        let mut nodes = Vec::new();
        let mut node_stmt = self.conn.prepare(&format!(
            "SELECT {} FROM nodes WHERE file_path = ?1 AND kind = 'file'",
            NODE_COLUMNS
        ))?;
        for f in importers {
            if let Some(n) = node_stmt.query_row(params![f], map_node_row).optional()? {
                nodes.push(n);
            }
        }
        Ok(nodes)
    }

    /// Scope candidates for a task with human-readable explanations (see
    /// [`crate::graph::scope::select_scope`] for the ranking).
    pub fn query_scope_explained(&self, task: &str) -> Result<Vec<ScopedNode>> {
        let sel = crate::graph::scope::select_scope(
            &self.conn,
            task,
            &crate::graph::scope::ScopeRequest {
                max_nodes: 25,
                max_files: 6,
                vector_hits: Vec::new(),
            },
        );
        let mut out = Vec::new();
        for c in sel.candidates.iter().chain(sel.tests.iter()) {
            let Some(node) = sel.nodes.get(&c.id) else { continue };
            let mut reasons: Vec<String> = c.explanations.clone();
            for r in &c.reasons {
                if let Some(e) = crate::graph::scope::explain_reason(r) {
                    if !reasons.contains(&e) {
                        reasons.push(e);
                    }
                }
            }
            if reasons.is_empty() {
                reasons = c.reasons.clone();
            }
            out.push(ScopedNode {
                node: node.clone(),
                score: c.score,
                reason: reasons.join("; "),
            });
        }
        Ok(out)
    }

    pub fn query_scope(&self, task: &str) -> Result<Vec<Node>> {
        let scoped = self.query_scope_explained(task)?;
        Ok(scoped.into_iter().map(|s| s.node).collect())
    }

    /// Target and impacted nodes (default [`ImpactOptions`]), ordered by file and line.
    pub fn query_impact(&self, target: &str) -> Result<Vec<Node>> {
        let report = self.impact(target, ImpactOptions::default())?;
        let mut nodes: Vec<Node> = report
            .roots
            .into_iter()
            .chain(report.impacted.into_iter().map(|e| e.node))
            .collect();
        nodes.sort_by(|a, b| a.file_path.cmp(&b.file_path).then(a.start_line.cmp(&b.start_line)));
        Ok(nodes)
    }

    pub fn get_nodes(&self, ids: &[String]) -> Result<Vec<Node>> {
        let mut nodes = Vec::new();
        for id in ids {
            if let Ok(node) = self.conn.query_row(
                r#"
                SELECT id, kind, name, qualified_name, container_id, identity_key, file_path, language,
                       start_line, end_line, start_column, end_column, docstring, signature, visibility,
                       is_exported, is_async, is_static, is_abstract, return_type, body_hash, updated_at
                FROM nodes WHERE id = ?1
                "#,
                params![id],
                Self::map_node,
            ) {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    /// Repair the store in place (see [`crate::graph::maintenance::repair_graph`]).
    pub fn repair(&self) -> Result<crate::graph::maintenance::RepairReport> {
        let root = self
            .indexed_project_root()
            .unwrap_or_else(|| PathBuf::from("."));
        crate::graph::maintenance::repair_graph(&self.db_path, &root)
    }

    /// Re-baseline grounded references ("accept current code"): for every grounding reference in
    /// the scaffold documents, record the current body hash in `_knobyte_grounded_source`.
    /// Only grounded symbols are baselined. The scaffold is the directory holding `graph.db`.
    pub fn ground_all(&self, root: &Path) -> Result<usize> {
        let scaffold = self
            .db_path
            .parent()
            .map(|p| p.to_path_buf())
            .filter(|p| p.exists())
            .unwrap_or_else(|| root.join(".knobyte"));
        self.ground_docs(root, &scaffold)
    }

    /// Like [`GraphEngine::ground_all`] with an explicit scaffold directory.
    pub fn ground_docs(&self, project_root: &Path, scaffold_root: &Path) -> Result<usize> {
        ground_documents(&self.conn, project_root, scaffold_root)
    }

    fn map_node(r: &rusqlite::Row) -> rusqlite::Result<Node> {
        map_node_row(r)
    }
}

/// Map a row selected with [`NODE_COLUMNS`] into a [`Node`].
pub(crate) fn map_node_row(r: &rusqlite::Row) -> rusqlite::Result<Node> {
    Ok(Node {
        id: r.get(0)?,
        kind: r.get(1)?,
        name: r.get(2)?,
        qualified_name: r.get(3)?,
        container_id: r.get(4)?,
        identity_key: r.get(5)?,
        file_path: r.get(6)?,
        language: r.get(7)?,
        start_line: r.get(8)?,
        end_line: r.get(9)?,
        start_column: r.get(10)?,
        end_column: r.get(11)?,
        docstring: r.get(12)?,
        signature: r.get(13)?,
        visibility: r.get(14)?,
        is_exported: r.get::<_, Option<i64>>(15)?.unwrap_or(0) == 1,
        is_async: r.get::<_, Option<i64>>(16)?.unwrap_or(0) == 1,
        is_static: r.get::<_, Option<i64>>(17)?.unwrap_or(0) == 1,
        is_abstract: r.get::<_, Option<i64>>(18)?.unwrap_or(0) == 1,
        return_type: r.get(19)?,
        body_hash: r.get(20)?,
        updated_at: r.get(21)?,
    })
}
