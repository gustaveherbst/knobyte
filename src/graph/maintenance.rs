//! Crash-safe graph maintenance: rebuild, incremental refresh and repair.
//!
//! Every maintenance run holds the cross-process [`MaintenanceLock`]. Builds never write the
//! live database directly: the graph is built into a disposable candidate file next to it,
//! validated, and then published into the live database in a single SQLite transaction that
//! replaces every derived table at once. A crash (or error) at any point before that commit
//! leaves the live graph exactly as it was; readers in WAL mode keep their consistent snapshot
//! throughout. Grounding baselines and schema history live outside the derived tables and are
//! never touched by a publication.

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::graph::build::{
    load_cache, load_known_files, metadata_value, prepare_files, write_graph, BuildMeta,
    BuildSummary,
};
use crate::graph::corpus::{CorpusScan, CoverageReport};
use crate::graph::extractor::EXTRACTOR_VERSION;
use crate::graph::lock::MaintenanceLock;
use crate::graph::publication::{apply_delta, DeltaStats};
use crate::graph::schema::{
    initialize_candidate_schema, initialize_graph_schema, is_migratable, migrate_schema,
    record_schema_version, reset_schema_version, stored_schema_version, CURRENT_SCHEMA_VERSION,
    DERIVED_TABLES, DROP_UNUSED_TABLES_SQL, GRAPH_SCHEMA_SQL, NODE_FTS_TRIGGERS,
};
use crate::graph::snapshot::{grammar_hash, read_snapshot, write_snapshot};
use crate::progress::IndexProgressBar;

/// A maintenance failure with a stable machine-readable code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphMaintenanceError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_path: Option<String>,
}

impl GraphMaintenanceError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            recovery_path: None,
        }
    }
}

impl fmt::Display for GraphMaintenanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if let Some(p) = &self.recovery_path {
            write!(f, " (previous index retained at {})", p)?;
        }
        Ok(())
    }
}

impl std::error::Error for GraphMaintenanceError {}

impl From<GraphMaintenanceError> for rusqlite::Error {
    fn from(e: GraphMaintenanceError) -> Self {
        rusqlite::Error::UserFunctionError(Box::new(e))
    }
}

/// Cancellation hook polled between maintenance phases (and per file while extracting). When it
/// returns true the run stops with `GRAPH_MAINTENANCE_CANCELLED` before publishing anything:
/// the live graph is left exactly as it was and the candidate is discarded.
pub type CancelHook = Arc<dyn Fn() -> bool + Send + Sync>;

/// Options of a maintenance run.
#[derive(Clone, Default)]
pub struct MaintenanceOptions {
    /// How long to wait for another maintenance run to release the lock (`None`: fail
    /// immediately with `GRAPH_MAINTENANCE_LOCKED`).
    pub lock_timeout: Option<Duration>,
    pub cancel: Option<CancelHook>,
}

impl fmt::Debug for MaintenanceOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MaintenanceOptions")
            .field("lock_timeout", &self.lock_timeout)
            .field("cancel", &self.cancel.is_some())
            .finish()
    }
}

impl MaintenanceOptions {
    pub fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = Some(timeout);
        self
    }

    pub fn with_cancel(mut self, hook: CancelHook) -> Self {
        self.cancel = Some(hook);
        self
    }

    /// Cancel when `flag` becomes true (e.g. set from a signal handler or another thread).
    pub fn with_cancel_flag(self, flag: Arc<AtomicBool>) -> Self {
        self.with_cancel(Arc::new(move || flag.load(Ordering::SeqCst)))
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.as_ref().is_some_and(|c| c())
    }

    /// `GRAPH_MAINTENANCE_CANCELLED` when the hook asks to stop.
    pub(crate) fn check_cancel(&self, phase: &str) -> Result<()> {
        if self.is_cancelled() {
            return Err(cancelled_error(phase));
        }
        Ok(())
    }

    fn lock(&self, db_path: &Path) -> Result<MaintenanceLock> {
        Ok(MaintenanceLock::acquire_within(db_path, self.lock_timeout.unwrap_or(Duration::ZERO))?)
    }
}

pub(crate) fn cancelled_error(phase: &str) -> rusqlite::Error {
    GraphMaintenanceError::new(
        "GRAPH_MAINTENANCE_CANCELLED",
        format!("Graph maintenance was cancelled ({}); the live graph was not changed.", phase),
    )
    .into()
}

/// A structured error for any failure to use the graph database: maintenance errors keep their
/// code, unreadable/malformed files become `GRAPH_INDEX_CORRUPT`, anything else `GRAPH_ERROR`.
pub fn graph_error(e: &rusqlite::Error) -> GraphMaintenanceError {
    if let Some(m) = maintenance_error(e) {
        return m.clone();
    }
    let corrupt = matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(f.code, rusqlite::ErrorCode::NotADatabase | rusqlite::ErrorCode::DatabaseCorrupt)
    );
    if corrupt {
        let mut err = GraphMaintenanceError::new(
            "GRAPH_INDEX_CORRUPT",
            "The graph database is not a readable SQLite database. Run `knobyte graph repair`, or \
             `knobyte graph rebuild` (it retains the damaged file for recovery).",
        );
        err.recovery_path = None;
        return err;
    }
    GraphMaintenanceError::new("GRAPH_ERROR", e.to_string())
}

/// [`graph_error`] as a `rusqlite::Error`.
pub fn classify_db_error(e: &rusqlite::Error) -> rusqlite::Error {
    graph_error(e).into()
}

/// The maintenance error carried by a `rusqlite::Error`, if any.
pub fn maintenance_error(e: &rusqlite::Error) -> Option<&GraphMaintenanceError> {
    match e {
        rusqlite::Error::UserFunctionError(b) => b.downcast_ref::<GraphMaintenanceError>(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Candidate files
// ---------------------------------------------------------------------------

fn db_file_name(db_path: &Path) -> String {
    db_path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "graph.db".to_string())
}

fn candidate_prefix(db_path: &Path) -> String {
    format!("{}.candidate-", db_file_name(db_path))
}

/// A disposable candidate database; removed (with its sidecars) on drop unless published.
pub(crate) struct Candidate {
    pub path: PathBuf,
}

impl Candidate {
    fn new(db_path: &Path) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let name = format!("{}{}-{}", candidate_prefix(db_path), std::process::id(), nanos);
        Self {
            path: db_path.with_file_name(name),
        }
    }
}

fn remove_with_sidecars(path: &Path) {
    let _ = fs::remove_file(path);
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut p = path.as_os_str().to_os_string();
        p.push(suffix);
        let _ = fs::remove_file(PathBuf::from(p));
    }
}

impl Drop for Candidate {
    fn drop(&mut self) {
        remove_with_sidecars(&self.path);
    }
}

/// Corrupt databases moved aside by `graph rebuild` that are kept for recovery (newest first).
pub const CORRUPT_COPIES_RETAINED: usize = 3;

/// Remove all but the newest [`CORRUPT_COPIES_RETAINED`] `graph.db.corrupt-*` copies (with
/// their sidecars). Returns how many were removed.
pub fn prune_corrupt_copies(db_path: &Path) -> usize {
    let prefix = format!("{}.corrupt-", db_file_name(db_path));
    let dir = match db_path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let mut copies: Vec<String> = fs::read_dir(&dir)
        .map(|it| {
            it.flatten()
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| {
                    n.starts_with(&prefix) && !n.ends_with("-wal") && !n.ends_with("-shm") && !n.ends_with("-journal")
                })
                .collect()
        })
        .unwrap_or_default();
    // Names carry a sortable UTC timestamp: newest last.
    copies.sort();
    let excess = copies.len().saturating_sub(CORRUPT_COPIES_RETAINED);
    for name in &copies[..excess] {
        remove_with_sidecars(&dir.join(name));
    }
    excess
}

/// Patterns that keep the local files a graph build produces out of version control: the graph
/// (`graph.db`, WAL/SHM sidecars, lock, build candidates, corrupt copies) and the CozoDB mirror
/// it synchronises.
const GRAPH_GITIGNORE_PATTERNS: [&str; 2] = ["graph.db*", "cozo.db*"];

/// When the graph lives in a `.knobyte/` scaffold directory, make sure that directory's
/// `.gitignore` ignores the graph's local files, so building a graph never leaves untracked
/// files behind. An existing `.gitignore` only gains the missing patterns.
pub(crate) fn ensure_local_gitignore(db_path: &Path) {
    let Some(dir) = db_path.parent() else { return };
    if dir.file_name().is_none_or(|n| n != crate::config::DEFAULT_SCAFFOLD_DIR) || !dir.is_dir() {
        return;
    }
    let path = dir.join(".gitignore");
    let existing = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return,
    };
    let present = |p: &str| existing.lines().any(|l| matches!(l.trim(), "*" | "/*") || l.trim() == p);
    let missing: Vec<&str> = GRAPH_GITIGNORE_PATTERNS.iter().copied().filter(|p| !present(p)).collect();
    if missing.is_empty() {
        return;
    }
    let mut out = existing.clone();
    if out.is_empty() {
        out.push_str("# Knobyte local caches and derived databases\n");
    } else if !out.ends_with('\n') {
        out.push('\n');
    }
    for p in missing {
        out.push_str(p);
        out.push('\n');
    }
    let _ = fs::write(&path, out);
}

/// Remove candidates left behind by crashed maintenance runs. Only call while holding the
/// maintenance lock (so no candidate can belong to a live run).
pub fn cleanup_stale_candidates(db_path: &Path) -> usize {
    let prefix = candidate_prefix(db_path);
    let dir = match db_path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let mut removed = 0;
    if let Ok(entries) = fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(&prefix)
                && !name.ends_with("-wal")
                && !name.ends_with("-shm")
                && !name.ends_with("-journal")
            {
                remove_with_sidecars(&e.path());
                removed += 1;
            }
        }
    }
    removed
}

/// Build a complete graph into a fresh candidate next to `db_path`, extracting every file.
pub(crate) fn build_candidate(
    db_path: &Path,
    root: &Path,
    scan: &CorpusScan,
    mode: &str,
    progress: Option<&IndexProgressBar>,
    opts: &MaintenanceOptions,
) -> Result<(Candidate, BuildSummary)> {
    let candidate = Candidate::new(db_path);
    let mut conn = Connection::open(&candidate.path)?;
    initialize_candidate_schema(&conn)?;
    let prepared = prepare_files(&scan.files, &Default::default(), &Default::default(), progress, opts.cancel.as_ref());
    opts.check_cancel("extracting sources")?;
    let summary = {
        let policy_hash = scan.policy.hash();
        let meta = BuildMeta {
            root,
            policy_hash: &policy_hash,
            coverage: &scan.coverage,
            mode,
            live: None,
            cancel: opts.cancel.as_ref(),
        };
        write_graph(&mut conn, prepared, &meta, progress)?
    };
    drop(conn);
    validate_candidate(&candidate.path)?;
    Ok((candidate, summary))
}

/// A candidate is publishable only when it is a structurally sound graph of this version.
fn validate_candidate(path: &Path) -> Result<()> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(GraphMaintenanceError::new(
            "GRAPH_CANDIDATE_INVALID",
            format!("The graph candidate failed validation ({}); the live graph was not changed.", check),
        )
        .into());
    }
    let version: i64 = conn.query_row("SELECT MAX(version) FROM schema_versions", [], |r| r.get(0))?;
    let built: Option<String> = conn
        .query_row(
            "SELECT value FROM project_metadata WHERE key = 'last_build_time'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if version != CURRENT_SCHEMA_VERSION || built.is_none() {
        return Err(GraphMaintenanceError::new(
            "GRAPH_CANDIDATE_INVALID",
            "The graph candidate is incomplete; the live graph was not changed.",
        )
        .into());
    }
    Ok(())
}

/// Atomically replace every derived table of the live database with the candidate's content.
/// With `reset` (an explicit rebuild) the build metadata and the recorded schema history are
/// replaced too, so a database left by another schema version becomes exactly current.
pub(crate) fn publish_candidate(live: &Connection, candidate: &Path, reset: bool) -> Result<()> {
    live.execute(
        "ATTACH DATABASE ?1 AS cand",
        params![candidate.to_string_lossy().to_string()],
    )?;
    let result = (|| -> Result<()> {
        live.execute_batch("BEGIN IMMEDIATE")?;
        live.execute_batch("DROP TABLE IF EXISTS main.nodes_fts;")?;
        for t in DERIVED_TABLES {
            live.execute_batch(&format!("DROP TABLE IF EXISTS main.{};", t))?;
        }
        if reset {
            live.execute_batch("DROP TABLE IF EXISTS main.project_metadata;")?;
            live.execute_batch(DROP_UNUSED_TABLES_SQL)?;
        }
        live.execute_batch(GRAPH_SCHEMA_SQL)?;
        for trig in NODE_FTS_TRIGGERS {
            live.execute_batch(&format!("DROP TRIGGER IF EXISTS main.{};", trig))?;
        }
        for t in [
            "nodes", "files", "file_extraction_cache", "import_bindings", "edges", "unresolved_refs",
            "code_chunks", "node_minhash", "node_lsh",
        ] {
            live.execute_batch(&format!("INSERT INTO main.{t} SELECT * FROM cand.{t};"))?;
        }
        live.execute_batch("INSERT INTO main.nodes_fts(nodes_fts) VALUES('rebuild');")?;
        // Recreates the FTS triggers (everything else already exists).
        live.execute_batch(GRAPH_SCHEMA_SQL)?;
        live.execute_batch(
            "INSERT OR REPLACE INTO main.project_metadata (key, value, updated_at) \
             SELECT key, value, updated_at FROM cand.project_metadata;",
        )?;
        if reset {
            reset_schema_version(live)?;
        } else {
            record_schema_version(live)?;
        }
        live.execute_batch("COMMIT")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = live.execute_batch("ROLLBACK");
    }
    let _ = live.execute_batch("DETACH DATABASE cand");
    result?;
    let _ = live.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
    Ok(())
}

/// Publish the candidate as a row delta (see [`crate::graph::publication`]): one
/// `BEGIN IMMEDIATE` transaction that writes only the rows that differ.
pub(crate) fn publish_candidate_delta(live: &Connection, candidate: &Path) -> Result<DeltaStats> {
    live.execute(
        "ATTACH DATABASE ?1 AS cand",
        params![candidate.to_string_lossy().to_string()],
    )?;
    let result = (|| -> Result<DeltaStats> {
        live.execute_batch("BEGIN IMMEDIATE")?;
        let stats = apply_delta(live)?;
        record_schema_version(live)?;
        live.execute_batch("COMMIT")?;
        Ok(stats)
    })();
    if result.is_err() {
        let _ = live.execute_batch("ROLLBACK");
    }
    let _ = live.execute_batch("DETACH DATABASE cand");
    let stats = result?;
    let _ = live.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
    Ok(stats)
}

/// How a refresh was published.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PublicationReport {
    /// `delta` (changed rows only) or `full` (every derived table replaced).
    pub mode: String,
    pub duration_ms: u128,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<DeltaStats>,
    /// Why a delta was not used, when it was attempted and failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

/// Forces refreshes to publish in full (diagnostics and comparisons).
pub const FULL_PUBLICATION_ENV: &str = "KNOBYTE_GRAPH_FULL_PUBLICATION";

/// Delta publication with a full publication as fallback.
fn publish_refresh(live: &Connection, candidate: &Path) -> Result<PublicationReport> {
    let started = Instant::now();
    if std::env::var_os(FULL_PUBLICATION_ENV).is_some() {
        publish_candidate(live, candidate, false)?;
        return Ok(PublicationReport {
            mode: "full".to_string(),
            duration_ms: started.elapsed().as_millis(),
            delta: None,
            fallback_reason: Some(format!("{} is set", FULL_PUBLICATION_ENV)),
        });
    }
    match publish_candidate_delta(live, candidate) {
        Ok(stats) => Ok(PublicationReport {
            mode: "delta".to_string(),
            duration_ms: started.elapsed().as_millis(),
            delta: Some(stats),
            fallback_reason: None,
        }),
        Err(e) => {
            publish_candidate(live, candidate, false)?;
            Ok(PublicationReport {
                mode: "full".to_string(),
                duration_ms: started.elapsed().as_millis(),
                delta: None,
                fallback_reason: Some(e.to_string()),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Live database access
// ---------------------------------------------------------------------------

/// Open the live database for maintenance; fails when it is unreadable or malformed.
fn open_live_checked(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path).map_err(|e| classify_db_error(&e))?;
    let check: String = conn
        .query_row("PRAGMA quick_check", [], |r| r.get(0))
        .map_err(|e| classify_db_error(&e))?;
    if check != "ok" {
        return Err(GraphMaintenanceError::new(
            "GRAPH_INDEX_CORRUPT",
            format!("The graph database failed its integrity check: {}", check),
        )
        .into());
    }
    initialize_graph_schema(&conn)?;
    Ok(conn)
}

type GroundingRow = (String, String, String, String, String, String);

/// Move a corrupt live database aside (retained for recovery) and start a fresh one, carrying
/// over whatever grounding baselines can still be read.
fn quarantine_corrupt(db_path: &Path) -> Result<(Connection, PathBuf)> {
    let mut rows: Vec<GroundingRow> = Vec::new();
    if let Ok(old) = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
        if let Ok(mut stmt) = old.prepare(
            "SELECT subject_kind, subject_id, node_id, source, body_hash, fingerprint FROM _knobyte_grounded_source",
        ) {
            if let Ok(it) = stmt.query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
            }) {
                rows = it.flatten().collect();
            }
        }
    }
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    let recovery = db_path.with_file_name(format!("{}.corrupt-{}", db_file_name(db_path), stamp));
    fs::rename(db_path, &recovery).map_err(|e| {
        GraphMaintenanceError::new(
            "GRAPH_INDEX_CORRUPT",
            format!("Could not move the corrupt graph database aside: {}", e),
        )
    })?;
    for suffix in ["-wal", "-shm"] {
        let mut from = db_path.as_os_str().to_os_string();
        from.push(suffix);
        let mut to = recovery.as_os_str().to_os_string();
        to.push(suffix);
        let _ = fs::rename(PathBuf::from(from), PathBuf::from(to));
    }
    // Repeated rebuilds over damaged files must not accumulate copies without bound.
    prune_corrupt_copies(db_path);
    let conn = Connection::open(db_path)?;
    initialize_graph_schema(&conn)?;
    for r in rows {
        let _ = conn.execute(
            "INSERT OR REPLACE INTO _knobyte_grounded_source (subject_kind, subject_id, node_id, source, body_hash, fingerprint) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![r.0, r.1, r.2, r.3, r.4, r.5],
        );
    }
    Ok((conn, recovery))
}

// ---------------------------------------------------------------------------
// Rebuild
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebuildOutcome {
    #[serde(flatten)]
    pub summary: BuildSummary,
    pub coverage: CoverageReport,
    /// Where a corrupt previous index was retained, if one was replaced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_path: Option<String>,
}

/// Full rebuild of the graph at `db_path` from `scan`, published atomically. A corrupt live
/// database is moved aside (and reported) rather than blocking the rebuild.
pub fn rebuild_graph(
    db_path: &Path,
    root: &Path,
    scan: &CorpusScan,
    progress: Option<&IndexProgressBar>,
) -> Result<RebuildOutcome> {
    rebuild_graph_with(db_path, root, scan, progress, &MaintenanceOptions::default())
}

/// [`rebuild_graph`] with a lock timeout and a cancellation hook.
pub fn rebuild_graph_with(
    db_path: &Path,
    root: &Path,
    scan: &CorpusScan,
    progress: Option<&IndexProgressBar>,
    opts: &MaintenanceOptions,
) -> Result<RebuildOutcome> {
    if let Some(parent) = db_path.parent() {
        let _ = fs::create_dir_all(parent);
        ensure_local_gitignore(db_path);
    }
    let _lock = opts.lock(db_path)?;
    cleanup_stale_candidates(db_path);
    opts.check_cancel("before rebuild")?;
    let (live, recovery) = match open_live_checked(db_path) {
        Ok(c) => (c, None),
        Err(_) => {
            let (c, p) = quarantine_corrupt(db_path)?;
            (c, Some(p))
        }
    };
    let summary = rebuild_locked_with(&live, db_path, root, scan, progress, opts)?;
    Ok(RebuildOutcome {
        summary,
        coverage: scan.coverage.clone(),
        recovery_path: recovery.map(|p| p.to_string_lossy().to_string()),
    })
}

/// Rebuild with the lock already held and the live connection open.
pub(crate) fn rebuild_locked(
    live: &Connection,
    db_path: &Path,
    root: &Path,
    scan: &CorpusScan,
    progress: Option<&IndexProgressBar>,
) -> Result<BuildSummary> {
    rebuild_locked_with(live, db_path, root, scan, progress, &MaintenanceOptions::default())
}

fn rebuild_locked_with(
    live: &Connection,
    db_path: &Path,
    root: &Path,
    scan: &CorpusScan,
    progress: Option<&IndexProgressBar>,
    opts: &MaintenanceOptions,
) -> Result<BuildSummary> {
    let (candidate, summary) = build_candidate(db_path, root, scan, "rebuild", progress, opts)?;
    opts.check_cancel("before publishing")?;
    if let Some(p) = progress {
        p.set_phase("Publishing graph...");
    }
    publish_candidate(live, &candidate.path, true)?;
    Ok(summary)
}

// ---------------------------------------------------------------------------
// Incremental refresh
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SourceChanges {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl SourceChanges {
    pub fn total(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshOutcome {
    #[serde(flatten)]
    pub summary: BuildSummary,
    /// `noop` (nothing changed, live graph untouched), `incremental` (only changed files
    /// re-extracted) or `full` (no usable previous index).
    pub mode: String,
    pub published: bool,
    pub changes: SourceChanges,
    pub coverage: CoverageReport,
    /// How the rows were published (absent for `noop`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<PublicationReport>,
    /// A `noop` refresh that only re-recorded the snapshot provenance (branch switched, HEAD
    /// moved or a pre-snapshot graph) without touching any derived row.
    #[serde(default)]
    pub provenance_updated: bool,
    /// Schema version an older graph was upgraded from in place by this refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated_from: Option<i64>,
}

/// Incremental refresh: re-extract only files whose content changed (content-hash extraction
/// cache), then re-resolve the whole graph so cross-file edges stay exact, and publish
/// atomically as a row delta. A refresh that would publish nothing leaves the live graph
/// untouched. An older migratable schema is upgraded in place first.
pub fn refresh_graph(
    db_path: &Path,
    root: &Path,
    scan: &CorpusScan,
    progress: Option<&IndexProgressBar>,
) -> Result<RefreshOutcome> {
    refresh_graph_with(db_path, root, scan, progress, &MaintenanceOptions::default())
}

/// [`refresh_graph`] with a lock timeout and a cancellation hook.
pub fn refresh_graph_with(
    db_path: &Path,
    root: &Path,
    scan: &CorpusScan,
    progress: Option<&IndexProgressBar>,
    opts: &MaintenanceOptions,
) -> Result<RefreshOutcome> {
    if let Some(parent) = db_path.parent() {
        let _ = fs::create_dir_all(parent);
        ensure_local_gitignore(db_path);
    }
    let _lock = opts.lock(db_path)?;
    cleanup_stale_candidates(db_path);
    opts.check_cancel("before refresh")?;
    let live = open_live_checked(db_path).map_err(|e| {
        let cause = graph_error(&e);
        let code = if cause.code == "GRAPH_INDEX_CORRUPT" {
            "GRAPH_INDEX_CORRUPT"
        } else {
            "GRAPH_REBUILD_REQUIRED"
        };
        rusqlite::Error::from(GraphMaintenanceError::new(
            code,
            format!(
                "The graph database cannot be refreshed ({}). Run `knobyte graph rebuild`.",
                cause.message
            ),
        ))
    })?;
    refresh_locked_with(&live, db_path, root, scan, progress, opts)
}

pub(crate) fn refresh_locked(
    live: &Connection,
    db_path: &Path,
    root: &Path,
    scan: &CorpusScan,
    progress: Option<&IndexProgressBar>,
) -> Result<RefreshOutcome> {
    refresh_locked_with(live, db_path, root, scan, progress, &MaintenanceOptions::default())
}

fn rebuild_required(schema: i64) -> rusqlite::Error {
    GraphMaintenanceError::new(
        "GRAPH_REBUILD_REQUIRED",
        format!(
            "The graph index uses schema v{} (current v{}) and cannot be refreshed incrementally. \
             Run `knobyte graph rebuild`.",
            schema, CURRENT_SCHEMA_VERSION
        ),
    )
    .into()
}

fn refresh_locked_with(
    live: &Connection,
    db_path: &Path,
    root: &Path,
    scan: &CorpusScan,
    progress: Option<&IndexProgressBar>,
    opts: &MaintenanceOptions,
) -> Result<RefreshOutcome> {
    // An older schema that shares the current layout is upgraded in place; anything else
    // (newer, or too old) is refused before anything is written: only an explicit rebuild may
    // replace it.
    let stored = stored_schema_version(live).unwrap_or(Some(0));
    let mut migrated_from = None;
    if let Some(v) = stored.filter(|v| *v != CURRENT_SCHEMA_VERSION) {
        if !is_migratable(v) {
            return Err(rebuild_required(v));
        }
        if let Some(p) = progress {
            p.set_phase(&format!("Upgrading graph schema v{} to v{}...", v, CURRENT_SCHEMA_VERSION));
        }
        migrated_from = migrate_schema(live)?;
    }
    let built = metadata_value(live, "last_build_time").is_some();
    if !built {
        let summary = rebuild_locked_with(live, db_path, root, scan, progress, opts)?;
        return Ok(RefreshOutcome {
            summary,
            mode: "full".to_string(),
            published: true,
            changes: SourceChanges {
                added: scan.files.iter().map(|f| f.rel_path.clone()).collect(),
                ..Default::default()
            },
            coverage: scan.coverage.clone(),
            publication: Some(PublicationReport { mode: "full".to_string(), ..Default::default() }),
            provenance_updated: false,
            migrated_from,
        });
    }
    if metadata_value(live, "rebuild_required").as_deref() == Some("1") {
        return Err(rebuild_required(CURRENT_SCHEMA_VERSION));
    }

    let known = load_known_files(live);
    let cache = load_cache(live);
    let prepared_probe =
        crate::graph::build::prepare_files(&scan.files, &known, &cache, None, opts.cancel.as_ref());
    opts.check_cancel("extracting sources")?;
    let mut changes = SourceChanges::default();
    let mut seen = std::collections::HashSet::new();
    let mut all_cached = true;
    for p in &prepared_probe {
        seen.insert(p.rel_path.clone());
        match known.get(&p.rel_path) {
            None => changes.added.push(p.rel_path.clone()),
            Some(k) if k.content_hash != p.content_hash => changes.modified.push(p.rel_path.clone()),
            Some(_) => {}
        }
        if !p.from_cache {
            all_cached = false;
        }
    }
    for path in known.keys() {
        if !seen.contains(path) {
            changes.deleted.push(path.clone());
        }
    }
    changes.added.sort();
    changes.modified.sort();
    changes.deleted.sort();

    let policy_hash = scan.policy.hash();
    let policy_same = metadata_value(live, "corpus_policy_hash").as_deref() == Some(&policy_hash);
    let extractor_same = metadata_value(live, "extractor_version").as_deref() == Some(EXTRACTOR_VERSION);
    let coverage_json = serde_json::to_string(&scan.coverage).unwrap_or_default();
    let coverage_same = metadata_value(live, "coverage").as_deref() == Some(coverage_json.as_str());
    // Switching the TypeScript type-checker mode (or its configuration inputs) re-resolves.
    let ts_mode_same = metadata_value(live, "typescript_compiler").unwrap_or_else(|| "source".to_string())
        == crate::graph::ts_compiler::mode_key(root);
    let snapshot = read_snapshot(live).ok().flatten();
    let grammar_same = snapshot.as_ref().is_none_or(|s| s.grammar_hash == grammar_hash());
    if changes.total() == 0 && all_cached && policy_same && extractor_same && coverage_same && grammar_same && ts_mode_same {
        // Nothing is re-extracted or published. Only the snapshot provenance is re-recorded
        // when it no longer describes the working tree (another branch / HEAD, or a graph
        // published before snapshots existed).
        let repo = crate::graph::git_state::repo_state(root).unwrap_or_default();
        let provenance_current = snapshot
            .as_ref()
            .is_some_and(|s| s.indexed_branch == repo.branch && s.indexed_head == repo.head);
        let mut provenance_updated = false;
        if !provenance_current {
            live.execute_batch("BEGIN IMMEDIATE")?;
            match write_snapshot(live, root, &policy_hash) {
                Ok(_) => live.execute_batch("COMMIT")?,
                Err(e) => {
                    let _ = live.execute_batch("ROLLBACK");
                    return Err(e);
                }
            }
            provenance_updated = true;
        }
        let mut summary = crate::graph::build::graph_totals(live);
        summary.files_reused = prepared_probe.len();
        return Ok(RefreshOutcome {
            summary,
            mode: "noop".to_string(),
            published: false,
            changes,
            coverage: scan.coverage.clone(),
            publication: None,
            provenance_updated,
            migrated_from,
        });
    }

    // Unchanged files come straight from the cache; only changed ones are parsed. The probe
    // already did that work, so the candidate is written from it directly.
    if let Some(p) = progress {
        for f in &scan.files {
            p.inc_file(&f.rel_path, f.size, 0);
        }
    }
    let candidate = Candidate::new(db_path);
    let summary = {
        let mut conn = Connection::open(&candidate.path)?;
        initialize_candidate_schema(&conn)?;
        let meta = BuildMeta {
            root,
            policy_hash: &policy_hash,
            coverage: &scan.coverage,
            mode: "refresh",
            live: Some(live),
            cancel: opts.cancel.as_ref(),
        };
        write_graph(&mut conn, prepared_probe, &meta, progress)?
    };
    validate_candidate(&candidate.path)?;
    opts.check_cancel("before publishing")?;
    if let Some(p) = progress {
        p.set_phase("Publishing graph...");
    }
    let publication = publish_refresh(live, &candidate.path)?;
    Ok(RefreshOutcome {
        summary,
        mode: "incremental".to_string(),
        published: true,
        changes,
        coverage: scan.coverage.clone(),
        publication: Some(publication),
        provenance_updated: false,
        migrated_from,
    })
}

// ---------------------------------------------------------------------------
// Repair
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepairReport {
    /// WAL frames that were pending and have been checkpointed into the database.
    pub recovered_wal_frames: i64,
    pub integrity_before: String,
    pub integrity_after: String,
    pub reindexed: bool,
    pub fts_rebuilt: bool,
    pub orphan_edges_removed: usize,
    pub orphan_refs_removed: usize,
    pub dangling_bindings_cleared: usize,
    pub stale_candidates_removed: usize,
    pub schema_version: i64,
    /// Older schema version upgraded in place by this repair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated_from: Option<i64>,
    /// Graph status after the repair.
    pub status: String,
}

/// Repair the store in place, under the maintenance lock: recover pending WAL, rebuild indexes
/// and the FTS index when they are inconsistent, upgrade an older migratable schema, remove
/// dangling rows and stale candidates. A database that cannot be repaired losslessly
/// (unreadable, or a schema that cannot be upgraded in place) is left untouched and reported
/// as `GRAPH_INDEX_NOT_REPAIRABLE`: run `knobyte graph rebuild`.
pub fn repair_graph(db_path: &Path, root: &Path) -> Result<RepairReport> {
    repair_graph_with(db_path, root, &MaintenanceOptions::default())
}

/// [`repair_graph`] with a lock timeout and a cancellation hook (polled before each step).
pub fn repair_graph_with(db_path: &Path, root: &Path, opts: &MaintenanceOptions) -> Result<RepairReport> {
    if !db_path.exists() {
        return Err(GraphMaintenanceError::new(
            "GRAPH_INDEX_MISSING",
            "No graph database exists yet. Run `knobyte graph rebuild`.",
        )
        .into());
    }
    let _lock = opts.lock(db_path)?;
    let mut report = RepairReport {
        stale_candidates_removed: cleanup_stale_candidates(db_path),
        ..Default::default()
    };
    let not_repairable = |msg: String| -> rusqlite::Error {
        GraphMaintenanceError::new(
            "GRAPH_INDEX_NOT_REPAIRABLE",
            format!("{} Run `knobyte graph rebuild` (it retains the current file for recovery).", msg),
        )
        .into()
    };
    opts.check_cancel("before repair")?;
    let conn = Connection::open(db_path)
        .map_err(|e| not_repairable(format!("The graph database cannot be opened: {}.", e)))?;
    let _ = conn.pragma_update(None, "busy_timeout", 5000);

    // 1. Recover pending WAL content.
    if let Ok((_busy, log, ckpt)) = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
    }) {
        report.recovered_wal_frames = ckpt.max(0).min(log.max(0));
    }

    // 2. Structural integrity.
    opts.check_cancel("before the integrity check")?;
    report.integrity_before = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
        .unwrap_or_else(|e| e.to_string());
    if report.integrity_before != "ok" {
        if conn.execute_batch("REINDEX;").is_ok() {
            report.reindexed = true;
        }
        let _ = conn.execute_batch("INSERT INTO nodes_fts(nodes_fts) VALUES('rebuild');");
        report.integrity_after = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap_or_else(|e| e.to_string());
        if report.integrity_after != "ok" {
            return Err(not_repairable(format!(
                "The graph database is damaged beyond in-place repair ({}).",
                report.integrity_after
            )));
        }
    }

    // 3. Schema lineage: an older layout-compatible version is upgraded in place.
    opts.check_cancel("before the schema upgrade")?;
    let schema: i64 = conn
        .query_row("SELECT MAX(version) FROM schema_versions", [], |r| r.get(0))
        .unwrap_or(0);
    if schema != CURRENT_SCHEMA_VERSION {
        if !is_migratable(schema) {
            return Err(not_repairable(format!(
                "The graph index uses schema v{} (current v{}), which cannot be upgraded losslessly.",
                schema, CURRENT_SCHEMA_VERSION
            )));
        }
        report.migrated_from = migrate_schema(&conn).map_err(|e| not_repairable(graph_error(&e).message))?;
    }
    initialize_graph_schema(&conn)?;
    report.schema_version = CURRENT_SCHEMA_VERSION;

    // 4. FTS consistency and dangling rows, in one transaction.
    opts.check_cancel("before removing dangling rows")?;
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let tx_result = (|| -> Result<()> {
        if conn
            .execute_batch("INSERT INTO nodes_fts(nodes_fts, rank) VALUES('integrity-check', 1);")
            .is_err()
        {
            conn.execute_batch("INSERT INTO nodes_fts(nodes_fts) VALUES('rebuild');")?;
            report.fts_rebuilt = true;
        }
        report.orphan_edges_removed = conn.execute(
            "DELETE FROM edges WHERE source NOT IN (SELECT id FROM nodes) OR target NOT IN (SELECT id FROM nodes)",
            [],
        )?;
        report.orphan_refs_removed = conn.execute(
            "DELETE FROM unresolved_refs WHERE from_node_id NOT IN (SELECT id FROM nodes)",
            [],
        )?;
        report.dangling_bindings_cleared = conn.execute(
            "UPDATE import_bindings SET target_id = NULL WHERE target_id IS NOT NULL AND target_id NOT IN (SELECT id FROM nodes)",
            [],
        )?;
        Ok(())
    })();
    match tx_result {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    if report.integrity_after.is_empty() {
        report.integrity_after = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap_or_else(|e| e.to_string());
    }
    let _ = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    drop(conn);
    report.status = crate::graph::status::inspect_status(db_path, root).status;
    Ok(report)
}
