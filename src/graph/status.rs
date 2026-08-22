//! Read-only graph health: `fresh`, `stale`, `degraded`, `corrupt`, `rebuild_required` or
//! `missing`, with parse health, source changes, coverage, provenance and remediation
//! diagnostics.
//!
//! Inspection never creates or modifies the database. All rows are read inside one read
//! transaction (one consistent snapshot of one publication), and the publication id is read
//! again afterwards: a publication that raced the inspection is re-inspected, and reported as
//! `GRAPH_STATUS_OBSERVATION_RACE` when it keeps changing.
//!
//! Diagnostic codes:
//!
//! | code | severity | effect |
//! |---|---|---|
//! | `GRAPH_INDEX_MISSING` | error | `missing` |
//! | `GRAPH_INDEX_CORRUPT` | error | `corrupt` |
//! | `GRAPH_INDEX_LOCKED` | warning | not inspected (`degraded`): the store is busy |
//! | `GRAPH_INDEX_INVARIANT_FAILED` | error | `corrupt`: rows violate persisted invariants |
//! | `GRAPH_SNAPSHOT_INVALID` | error | `corrupt` |
//! | `GRAPH_SNAPSHOT_SCHEMA_MISMATCH` | error | `corrupt` |
//! | `GRAPH_SNAPSHOT_CONTENT_MISMATCH` | error | `corrupt`: rows disagree with their snapshot |
//! | `GRAPH_INDEX_SCHEMA_INCOMPATIBLE` | error | `rebuild_required` |
//! | `GRAPH_INDEX_REPAIR_AVAILABLE` | warning | older schema upgradable in place (`stale`, recovered by `knobyte graph refresh`), or dangling rows |
//! | `GRAPH_INDEX_REBUILD_REQUIRED` | error | `rebuild_required` |
//! | `GRAPH_SNAPSHOT_LEGACY` | warning | `stale`: published before snapshot provenance |
//! | `GRAPH_SOURCES_CHANGED` | warning | `stale` |
//! | `GRAPH_CORPUS_POLICY_CHANGED` | warning | `stale` |
//! | `GRAPH_EXTRACTOR_CHANGED` | warning | `stale` |
//! | `GRAPH_BUILD_MANIFEST_CHANGED` | warning | `stale`: grammar versions changed |
//! | `GRAPH_INDEX_BRANCH_CHANGED` | warning | `stale`: another branch is checked out |
//! | `GRAPH_INDEX_HEAD_CHANGED` | info | HEAD moved since the build |
//! | `GRAPH_INDEX_ROOT_MISMATCH` | warning | `stale` |
//! | `GRAPH_CORPUS_LIMIT_EXCEEDED` | error | `stale` (unbounded) |
//! | `GRAPH_INDEX_CORPUS_LIMIT_EXCEEDED` | warning | the store exceeds its size bound |
//! | `GRAPH_INDEX_PATH_OUTSIDE_PROJECT` | info | the store lives outside the project |
//! | `GRAPH_INDEX_SIDECAR_ACTIVE` | warning / info | rollback journal or oversized WAL |
//! | `GRAPH_MAINTENANCE_ACTIVE` | info | a rebuild / refresh / repair holds the lock |
//! | `GRAPH_STATUS_OBSERVATION_RACE` | warning | `stale` (unbounded): publications raced |
//! | `GRAPH_PARSE_DEGRADED` | warning | `degraded` |

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};

use crate::graph::build::{file_mtime_ms, metadata_value};
use crate::graph::corpus::{scan_corpus, CorpusPolicy, CoverageReport};
use crate::graph::extractor::EXTRACTOR_VERSION;
use crate::graph::fingerprint::compute_file_hash;
use crate::graph::git_state::repo_state;
use crate::graph::schema::{is_migratable, CURRENT_SCHEMA_VERSION};
use crate::graph::snapshot::{grammar_hash, publication_id_at, read_snapshot, stored_sources, GraphSnapshot};

/// Lists of changed paths are truncated to this many entries (the totals stay exact).
const MAX_LISTED_PATHS: usize = 50;
/// Size bound of the graph store (database + WAL).
pub const MAX_INDEX_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// A WAL larger than this outside maintenance is worth checkpointing (`graph repair`).
const LARGE_WAL_BYTES: u64 = 256 * 1024 * 1024;
/// Inspections attempted while publications keep racing them.
const MAX_OBSERVATION_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ParseHealth {
    pub total: usize,
    pub ok: usize,
    pub partial: usize,
    pub failed: usize,
    pub failed_paths: Vec<String>,
    pub partial_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StatusChanges {
    pub total: usize,
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
    pub truncated: bool,
    /// The corpus policy (`graph.ignore`, limits) differs from the one the index was built with.
    pub policy_changed: bool,
    /// The index was produced by a different extractor version.
    pub extractor_changed: bool,
    /// The index was produced with other tree-sitter grammars.
    #[serde(default)]
    pub grammar_changed: bool,
    /// Another branch is checked out than the one the index was built on.
    #[serde(default)]
    pub branch_changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GraphCounts {
    pub files: i64,
    pub nodes: i64,
    pub edges: i64,
    pub unresolved: i64,
}

/// Repository state now, next to what the snapshot recorded.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RepoProvenance {
    pub branch: Option<String>,
    pub head: Option<String>,
    pub indexed_branch: Option<String>,
    pub indexed_head: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub severity: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

fn diag(code: &str, severity: &str, message: impl Into<String>, command: Option<&str>) -> Diagnostic {
    Diagnostic {
        code: code.to_string(),
        severity: severity.to_string(),
        message: message.into(),
        command: command.map(str::to_string),
    }
}

/// Full graph health report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphHealth {
    /// `fresh` | `stale` | `degraded` | `corrupt` | `rebuild_required` | `missing`
    pub status: String,
    /// False when the store could not be opened, so the remaining fields are placeholders.
    pub inspected: bool,
    pub observed_at: String,
    pub db_path: String,
    pub schema_version: Option<i64>,
    pub extractor_version: Option<String>,
    pub last_successful_index_at: Option<String>,
    pub build_mode: Option<String>,
    /// TypeScript resolution mode of the last build: `source`, or the checker's version and the
    /// package path it used (`typescript 6.0.3 from homebrew: /opt/...`), or a fallback reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typescript_compiler: Option<String>,
    pub counts: GraphCounts,
    pub parse_health: ParseHealth,
    pub changes: StatusChanges,
    /// Coverage recorded by the last build (unindexed extensions, skipped files).
    pub coverage: Option<CoverageReport>,
    /// Id of the publication this report describes (`None` before snapshot provenance).
    #[serde(default)]
    pub publication_id: Option<String>,
    /// Snapshot provenance of that publication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<GraphSnapshot>,
    /// Repository branch / HEAD now and when indexed (absent outside git).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<RepoProvenance>,
    pub diagnostics: Vec<Diagnostic>,
}

impl GraphHealth {
    fn new(db_path: &Path, status: &str) -> Self {
        Self {
            status: status.to_string(),
            inspected: false,
            observed_at: chrono::Utc::now().to_rfc3339(),
            db_path: db_path.to_string_lossy().to_string(),
            schema_version: None,
            extractor_version: None,
            last_successful_index_at: None,
            build_mode: None,
            typescript_compiler: None,
            counts: GraphCounts::default(),
            parse_health: ParseHealth::default(),
            changes: StatusChanges::default(),
            coverage: None,
            publication_id: None,
            snapshot: None,
            repo: None,
            diagnostics: Vec::new(),
        }
    }

    /// First remediation command suggested by the diagnostics.
    pub fn next_command(&self) -> Option<&str> {
        self.diagnostics.iter().find_map(|d| d.command.as_deref())
    }

    pub fn has_diagnostic(&self, code: &str) -> bool {
        self.diagnostics.iter().any(|d| d.code == code)
    }
}

const REQUIRED_TABLES: [&str; 6] = [
    "nodes",
    "edges",
    "files",
    "project_metadata",
    "schema_versions",
    "unresolved_refs",
];

/// Inspect the graph at `db_path` for the project at `project_root`, read-only.
pub fn inspect_status(db_path: &Path, project_root: &Path) -> GraphHealth {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let before = publication_id_at(db_path);
        let mut h = inspect_once(db_path, project_root);
        if !h.inspected {
            probe_sidecars(db_path, &mut h);
            return h;
        }
        let after = publication_id_at(db_path);
        if before == after && after == h.publication_id {
            probe_sidecars(db_path, &mut h);
            return h;
        }
        if attempt >= MAX_OBSERVATION_ATTEMPTS {
            h.diagnostics.push(diag(
                "GRAPH_STATUS_OBSERVATION_RACE",
                "warning",
                "The graph was republished while it was being inspected; freshness could not be established. Retry.",
                None,
            ));
            h.changes.truncated = true;
            if matches!(h.status.as_str(), "fresh" | "degraded") {
                h.status = "stale".to_string();
            }
            probe_sidecars(db_path, &mut h);
            return h;
        }
    }
}

/// True when `path` is a clean project-relative path (no root, no `..`).
fn contained_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.starts_with('\\')
        && !path.contains('\\')
        && Path::new(path).components().all(|c| matches!(c, Component::Normal(_)))
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(f, _)
        if matches!(f.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
}

fn locked(db_path: &Path, message: String) -> GraphHealth {
    let mut h = GraphHealth::new(db_path, "degraded");
    h.diagnostics.push(diag("GRAPH_INDEX_LOCKED", "warning", message, None));
    h
}

/// Size, WAL / journal and maintenance-lock probes (never block, never write).
fn probe_sidecars(db_path: &Path, h: &mut GraphHealth) {
    if !db_path.exists() {
        return;
    }
    let len = |suffix: &str| {
        let mut p = db_path.as_os_str().to_os_string();
        p.push(suffix);
        std::fs::metadata(std::path::PathBuf::from(p)).map(|m| m.len()).ok()
    };
    let db_len = len("").unwrap_or(0);
    let wal_len = len("-wal").unwrap_or(0);
    if db_len + wal_len > MAX_INDEX_BYTES {
        h.diagnostics.push(diag(
            "GRAPH_INDEX_CORPUS_LIMIT_EXCEEDED",
            "warning",
            format!(
                "The graph store is {} bytes, above its {} byte bound; consider narrowing the corpus (graph.ignore).",
                db_len + wal_len,
                MAX_INDEX_BYTES
            ),
            None,
        ));
    }
    let maintenance = maintenance_active(db_path);
    if maintenance {
        h.diagnostics.push(diag(
            "GRAPH_MAINTENANCE_ACTIVE",
            "info",
            "A graph rebuild, refresh or repair is running; this report describes the graph published before it.",
            None,
        ));
    }
    if len("-journal").is_some_and(|n| n > 0) && !maintenance {
        h.diagnostics.push(diag(
            "GRAPH_INDEX_SIDECAR_ACTIVE",
            "warning",
            "A rollback journal is next to the graph database: a write was interrupted. Repair recovers it.",
            Some("knobyte graph repair"),
        ));
    } else if wal_len > LARGE_WAL_BYTES && !maintenance {
        h.diagnostics.push(diag(
            "GRAPH_INDEX_SIDECAR_ACTIVE",
            "info",
            format!("The graph write-ahead log holds {} bytes not yet checkpointed.", wal_len),
            Some("knobyte graph repair"),
        ));
    }
}

/// Whether another process holds the maintenance lock (probed with a shared, non-blocking
/// lock attempt on the existing lock file; nothing is created).
fn maintenance_active(db_path: &Path) -> bool {
    let path = crate::graph::lock::lock_path(db_path);
    let Ok(file) = std::fs::File::open(&path) else { return false };
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(_) => false,
    }
}

fn inspect_once(db_path: &Path, project_root: &Path) -> GraphHealth {
    if !db_path.exists() {
        let mut h = GraphHealth::new(db_path, "missing");
        h.diagnostics.push(diag(
            "GRAPH_INDEX_MISSING",
            "error",
            "No code graph has been built for this project.",
            Some("knobyte graph rebuild"),
        ));
        return h;
    }
    let conn = match Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(e) if is_busy(&e) => return locked(db_path, format!("The graph database is busy: {}", e)),
        Err(e) => return corrupt(db_path, format!("The graph database cannot be opened: {}", e)),
    };
    let _ = conn.pragma_update(None, "busy_timeout", 5000);
    match conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)) {
        Ok(s) if s == "ok" => {}
        Ok(s) => return corrupt(db_path, format!("The graph database failed its integrity check: {}", s)),
        Err(e) if is_busy(&e) => return locked(db_path, format!("The graph database is busy: {}", e)),
        Err(e) => return corrupt(db_path, format!("The graph database is not readable: {}", e)),
    }
    // Every read below sees one publication.
    let _ = conn.execute_batch("BEGIN DEFERRED");
    let h = inspect_rows(&conn, db_path, project_root);
    let _ = conn.execute_batch("COMMIT");
    h
}

fn inspect_rows(conn: &Connection, db_path: &Path, project_root: &Path) -> GraphHealth {
    let tables: HashSet<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table')")
        .and_then(|mut s| {
            s.query_map([], |r| r.get::<_, String>(0))
                .map(|it| it.flatten().collect())
        })
        .unwrap_or_default();
    let missing_tables: Vec<&str> = REQUIRED_TABLES
        .iter()
        .copied()
        .filter(|t| !tables.contains(*t))
        .collect();
    if !missing_tables.is_empty() {
        return corrupt(
            db_path,
            format!("The graph database is missing tables: {}", missing_tables.join(", ")),
        );
    }

    let mut h = GraphHealth::new(db_path, "fresh");
    h.inspected = true;
    h.schema_version = conn
        .query_row("SELECT MAX(version) FROM schema_versions", [], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    h.extractor_version = metadata_value(conn, "extractor_version");
    h.last_successful_index_at =
        metadata_value(conn, "last_successful_index_at").or_else(|| metadata_value(conn, "last_build_time"));
    h.build_mode = metadata_value(conn, "build_mode");
    h.typescript_compiler = metadata_value(conn, "typescript_compiler_status");
    h.coverage = metadata_value(conn, "coverage").and_then(|c| serde_json::from_str(&c).ok());
    h.publication_id = crate::graph::snapshot::publication_id(conn);
    let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0);
    h.counts = GraphCounts {
        files: count("SELECT COUNT(*) FROM files"),
        nodes: count("SELECT COUNT(*) FROM nodes"),
        edges: count("SELECT COUNT(*) FROM edges"),
        unresolved: count("SELECT COUNT(*) FROM unresolved_refs"),
    };

    // Store location (informational: a configured path may live elsewhere).
    let canon_root = project_root.canonicalize().unwrap_or_else(|_| project_root.to_path_buf());
    if let Ok(canon_db) = db_path.canonicalize() {
        if !canon_db.starts_with(&canon_root) {
            h.diagnostics.push(diag(
                "GRAPH_INDEX_PATH_OUTSIDE_PROJECT",
                "info",
                format!("The graph database {} is outside the project root {}.", canon_db.display(), canon_root.display()),
                None,
            ));
        }
    }

    if metadata_value(conn, "last_build_time").is_none() {
        h.status = "missing".to_string();
        h.diagnostics.push(diag(
            "GRAPH_INDEX_MISSING",
            "error",
            "The graph database exists but no build has completed.",
            Some("knobyte graph rebuild"),
        ));
        return h;
    }

    let schema = h.schema_version.unwrap_or(0);
    if schema != CURRENT_SCHEMA_VERSION {
        let newer = schema > CURRENT_SCHEMA_VERSION;
        if !newer && is_migratable(schema) {
            // Recoverable by `knobyte graph refresh` (no rebuild): `stale`, consistent with its
            // recovery command. Reads still refuse an older layout (see `ReadGate`).
            h.status = "stale".to_string();
            h.diagnostics.push(diag(
                "GRAPH_INDEX_REPAIR_AVAILABLE",
                "warning",
                format!(
                    "The graph index uses schema v{}; this Knobyte reads v{}. It upgrades in place \
                     (`knobyte graph refresh` upgrades and re-derives; `knobyte graph repair` only \
                     upgrades), keeping grounding baselines.",
                    schema, CURRENT_SCHEMA_VERSION
                ),
                Some("knobyte graph refresh"),
            ));
            return h;
        }
        h.status = "rebuild_required".to_string();
        h.diagnostics.push(diag(
            "GRAPH_INDEX_SCHEMA_INCOMPATIBLE",
            "error",
            if newer {
                format!(
                    "The graph index uses schema v{} written by a newer Knobyte (this one reads v{}).",
                    schema, CURRENT_SCHEMA_VERSION
                )
            } else {
                format!(
                    "The graph index uses schema v{}; this Knobyte requires v{}.",
                    schema, CURRENT_SCHEMA_VERSION
                )
            },
            Some("knobyte graph rebuild"),
        ));
        return h;
    }
    if metadata_value(conn, "rebuild_required").as_deref() == Some("1") {
        h.status = "rebuild_required".to_string();
        h.diagnostics.push(diag(
            "GRAPH_INDEX_REBUILD_REQUIRED",
            "error",
            "The graph index was marked as requiring a rebuild.",
            Some("knobyte graph rebuild"),
        ));
        return h;
    }

    // Persisted invariants: every indexed path is project-relative.
    let escaping: Vec<String> = conn
        .prepare("SELECT path FROM files")
        .and_then(|mut s| s.query_map([], |r| r.get::<_, String>(0)).map(|it| it.flatten().collect::<Vec<_>>()))
        .unwrap_or_default()
        .into_iter()
        .filter(|p| !contained_relative(p))
        .take(5)
        .collect();
    if !escaping.is_empty() {
        h.status = "corrupt".to_string();
        h.diagnostics.push(diag(
            "GRAPH_INDEX_INVARIANT_FAILED",
            "error",
            format!("Indexed paths escape the project root: {}.", escaping.join(", ")),
            Some("knobyte graph rebuild"),
        ));
        return h;
    }

    // Snapshot provenance: must describe exactly the stored rows.
    let snapshot = match read_snapshot(conn) {
        Ok(s) => s,
        Err(e) => {
            h.status = "corrupt".to_string();
            h.diagnostics.push(diag("GRAPH_SNAPSHOT_INVALID", "error", e, Some("knobyte graph rebuild")));
            return h;
        }
    };
    if let Some(s) = &snapshot {
        if s.schema_version != schema {
            h.status = "corrupt".to_string();
            h.diagnostics.push(diag(
                "GRAPH_SNAPSHOT_SCHEMA_MISMATCH",
                "error",
                format!("The graph snapshot records schema v{}, but the store records v{}.", s.schema_version, schema),
                Some("knobyte graph rebuild"),
            ));
            return h;
        }
        match stored_sources(conn) {
            Ok((digest, n, ph)) if digest == s.source_corpus_digest && n == s.source_count && ph == s.parse_health => {}
            _ => {
                h.status = "corrupt".to_string();
                h.diagnostics.push(diag(
                    "GRAPH_SNAPSHOT_CONTENT_MISMATCH",
                    "error",
                    "The indexed sources or parse totals disagree with the graph snapshot: the store was changed outside a publication.",
                    Some("knobyte graph rebuild"),
                ));
                return h;
            }
        }
    }

    // Parse health.
    let mut ph = ParseHealth::default();
    if let Ok(mut stmt) = conn.prepare("SELECT path, parse_status FROM files ORDER BY path") {
        if let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
            for (path, st) in rows.flatten() {
                ph.total += 1;
                match st.as_str() {
                    "failed" => {
                        ph.failed += 1;
                        if ph.failed_paths.len() < MAX_LISTED_PATHS {
                            ph.failed_paths.push(path);
                        }
                    }
                    "partial" => {
                        ph.partial += 1;
                        if ph.partial_paths.len() < MAX_LISTED_PATHS {
                            ph.partial_paths.push(path);
                        }
                    }
                    _ => ph.ok += 1,
                }
            }
        }
    }
    h.parse_health = ph;

    // Source changes against the corpus as the policy defines it now.
    let policy = CorpusPolicy::for_project(project_root);
    let indexed_root = metadata_value(conn, "project_root");
    let mut changes = StatusChanges {
        policy_changed: metadata_value(conn, "corpus_policy_hash").as_deref() != Some(policy.hash().as_str()),
        extractor_changed: h.extractor_version.as_deref() != Some(EXTRACTOR_VERSION),
        grammar_changed: snapshot.as_ref().is_some_and(|s| s.grammar_hash != grammar_hash()),
        ..Default::default()
    };
    let indexed: HashMap<String, (String, i64, i64)> = conn
        .prepare("SELECT path, content_hash, modified_at, size FROM files")
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))
                .map(|it| it.flatten().collect())
        })
        .unwrap_or_default();
    let root_matches = indexed_root
        .as_deref()
        .map(|r| Path::new(r) == canon_root)
        .unwrap_or(false);
    if !root_matches {
        h.diagnostics.push(diag(
            "GRAPH_INDEX_ROOT_MISMATCH",
            "warning",
            format!(
                "The graph was built for {} (inspecting {}).",
                indexed_root.as_deref().unwrap_or("an unknown root"),
                project_root.display()
            ),
            Some("knobyte graph rebuild"),
        ));
    }
    match scan_corpus(project_root, &policy) {
        Ok(scan) => {
            let mut seen = HashSet::new();
            for f in &scan.files {
                seen.insert(f.rel_path.as_str());
                match indexed.get(&f.rel_path) {
                    None => changes.added.push(f.rel_path.clone()),
                    Some((hash, mtime, size)) => {
                        if file_mtime_ms(&f.path) == *mtime && f.size as i64 == *size {
                            continue;
                        }
                        let same = std::fs::read(&f.path)
                            .map(|b| compute_file_hash(&b) == *hash)
                            .unwrap_or(false);
                        if !same {
                            changes.modified.push(f.rel_path.clone());
                        }
                    }
                }
            }
            for p in indexed.keys() {
                if !seen.contains(p.as_str()) {
                    changes.deleted.push(p.clone());
                }
            }
        }
        Err(e) => {
            h.diagnostics.push(diag("GRAPH_CORPUS_LIMIT_EXCEEDED", "error", e.to_string(), None));
            changes.truncated = true;
        }
    }
    changes.added.sort();
    changes.modified.sort();
    changes.deleted.sort();
    changes.total = changes.added.len() + changes.modified.len() + changes.deleted.len();
    for list in [&mut changes.added, &mut changes.modified, &mut changes.deleted] {
        if list.len() > MAX_LISTED_PATHS {
            list.truncate(MAX_LISTED_PATHS);
            changes.truncated = true;
        }
    }

    // Repository provenance.
    if let Some(repo) = repo_state(project_root) {
        let (indexed_branch, indexed_head) = snapshot
            .as_ref()
            .map(|s| (s.indexed_branch.clone(), s.indexed_head.clone()))
            .unwrap_or_default();
        if let Some(s) = &snapshot {
            // A detached HEAD keeps the branch the index was built on comparable only by HEAD.
            if repo.branch.is_some() && s.indexed_branch != repo.branch {
                changes.branch_changed = true;
                h.diagnostics.push(diag(
                    "GRAPH_INDEX_BRANCH_CHANGED",
                    "warning",
                    format!(
                        "The checked-out branch ({}) differs from the indexed branch ({}).",
                        repo.branch.as_deref().unwrap_or("detached"),
                        s.indexed_branch.as_deref().unwrap_or("detached or unknown")
                    ),
                    Some("knobyte graph refresh"),
                ));
            } else if s.indexed_head != repo.head {
                h.diagnostics.push(diag(
                    "GRAPH_INDEX_HEAD_CHANGED",
                    "info",
                    "Repository HEAD differs from the commit recorded by the last graph publication.",
                    None,
                ));
            }
        }
        h.repo = Some(RepoProvenance { branch: repo.branch, head: repo.head, indexed_branch, indexed_head });
    }

    let legacy = snapshot.is_none();
    let stale = changes.total > 0
        || changes.policy_changed
        || changes.extractor_changed
        || changes.grammar_changed
        || changes.branch_changed
        || legacy
        || !root_matches;
    if changes.total > 0 {
        h.diagnostics.push(diag(
            "GRAPH_SOURCES_CHANGED",
            "warning",
            format!(
                "{} source file(s) changed since the last build ({} added, {} modified, {} deleted).",
                changes.total,
                changes.added.len(),
                changes.modified.len(),
                changes.deleted.len()
            ),
            Some("knobyte graph refresh"),
        ));
    }
    if changes.policy_changed {
        h.diagnostics.push(diag(
            "GRAPH_CORPUS_POLICY_CHANGED",
            "warning",
            "The graph corpus policy (graph.ignore / limits) changed since the last build.",
            Some("knobyte graph refresh"),
        ));
    }
    if changes.extractor_changed {
        h.diagnostics.push(diag(
            "GRAPH_EXTRACTOR_CHANGED",
            "warning",
            format!(
                "The index was produced by extractor {} (current {}).",
                h.extractor_version.as_deref().unwrap_or("unknown"),
                EXTRACTOR_VERSION
            ),
            Some("knobyte graph refresh"),
        ));
    }
    if changes.grammar_changed {
        h.diagnostics.push(diag(
            "GRAPH_BUILD_MANIFEST_CHANGED",
            "warning",
            "Graph build inputs changed (grammar): the index was parsed with other tree-sitter grammars.",
            Some("knobyte graph refresh"),
        ));
    }
    if legacy {
        h.diagnostics.push(diag(
            "GRAPH_SNAPSHOT_LEGACY",
            "warning",
            "The graph was published before snapshot provenance was recorded; refresh to record it.",
            Some("knobyte graph refresh"),
        ));
    }
    h.changes = changes;
    let degraded = h.parse_health.partial > 0 || h.parse_health.failed > 0;
    if degraded {
        h.diagnostics.push(diag(
            "GRAPH_PARSE_DEGRADED",
            "warning",
            format!(
                "{} file(s) parsed partially and {} failed; their symbols may be incomplete.",
                h.parse_health.partial, h.parse_health.failed
            ),
            None,
        ));
    }

    // Dangling rows a repair removes (a hint: reads skip them).
    let orphans = count(
        "SELECT (SELECT COUNT(*) FROM edges WHERE source NOT IN (SELECT id FROM nodes) OR target NOT IN (SELECT id FROM nodes)) \
         + (SELECT COUNT(*) FROM unresolved_refs WHERE from_node_id NOT IN (SELECT id FROM nodes))",
    );
    if orphans > 0 {
        h.diagnostics.push(diag(
            "GRAPH_INDEX_REPAIR_AVAILABLE",
            "warning",
            format!("{} edge(s) or reference(s) point at missing nodes; repair removes them.", orphans),
            Some("knobyte graph repair"),
        ));
    }

    h.snapshot = snapshot;
    h.status = if stale {
        "stale"
    } else if degraded {
        "degraded"
    } else {
        "fresh"
    }
    .to_string();
    h
}

fn corrupt(db_path: &Path, message: String) -> GraphHealth {
    let mut h = GraphHealth::new(db_path, "corrupt");
    h.diagnostics.push(diag(
        "GRAPH_INDEX_CORRUPT",
        "error",
        message,
        Some("knobyte graph repair"),
    ));
    h.diagnostics.push(diag(
        "GRAPH_INDEX_CORRUPT",
        "info",
        "If repair fails, `knobyte graph rebuild` replaces the index and retains the damaged file.",
        Some("knobyte graph rebuild"),
    ));
    h
}
