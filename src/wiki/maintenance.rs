//! Wiki index maintenance: the single-writer lease, cooperative abort, the corpus-unchanged
//! assertion, integrity checks, the normalized dump, `wiki index doctor`, and the seven index
//! states every read surface reports.
//!
//! * **Lease.** A rebuild or refresh holds an exclusive OS lock on `<wiki.db>.lease`; a second
//!   maintainer waits briefly, then fails with `WIKI_INDEX_BUSY`. The lock dies with the
//!   process, so a crash never leaves a stale lease.
//! * **Abort.** A [`MaintenanceContext`] carries an abort flag checked at each phase boundary
//!   (`discover`, `stage`, `parse`, `resolve`, `validate`, `publish`). An abort rolls the
//!   transaction back: nothing half-built is ever published (`OPERATION_INTERRUPTED`).
//! * **Corpus unchanged.** The corpus is observed (paths, sizes, modification times) before
//!   reading and again before publishing; a Markdown edit in between aborts the run rather
//!   than publishing an index of a corpus that no longer exists.
//! * **Bounds.** Discovery is bounded (files, bytes, depth, directory entries); an index file
//!   larger than [`MAX_INDEX_BYTES`] is refused before it is opened; status and doctor
//!   reports carry at most [`MAX_DIAGNOSTICS`] diagnostics.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::wiki::diagnostics::diag;
use crate::wiki::index::{coded_error, WikiIndex, WIKI_INDEX_SCHEMA_VERSION};
use crate::wiki::models::WikiDiagnostic;
use crate::wiki::scope::WikiScope;

/// An index file larger than this is refused before it is opened.
pub const MAX_INDEX_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Diagnostics carried by one status or doctor report.
pub const MAX_DIAGNOSTICS: usize = 100;
/// How long a maintainer waits for another one's lease.
pub const LEASE_WAIT: Duration = Duration::from_secs(5);

/// Maintenance phases, in order.
pub const MAINTENANCE_PHASES: [&str; 6] = [
    "discover", "stage", "parse", "resolve", "validate", "publish",
];

/// The seven states of the wiki index, worst first.
pub const INDEX_STATES: [&str; 7] = [
    "corrupt",
    "rebuild_required",
    "missing",
    "migration_required",
    "stale",
    "degraded",
    "fresh",
];

type ProgressFn = dyn Fn(&str, usize, usize) + Send + Sync;

/// Abort flag and progress callback for one maintenance run.
#[derive(Clone, Default)]
pub struct MaintenanceContext {
    pub abort: Option<Arc<AtomicBool>>,
    pub progress: Option<Arc<ProgressFn>>,
}

impl MaintenanceContext {
    pub fn with_abort(flag: Arc<AtomicBool>) -> Self {
        Self {
            abort: Some(flag),
            progress: None,
        }
    }

    fn aborted(&self) -> bool {
        self.abort
            .as_ref()
            .is_some_and(|f| f.load(Ordering::SeqCst))
    }

    /// A phase boundary: fails with `OPERATION_INTERRUPTED` when aborted, else reports progress.
    pub fn boundary(&self, phase: &str, completed: usize, total: usize) -> rusqlite::Result<()> {
        if self.aborted() {
            return Err(interrupted(phase, "it was aborted"));
        }
        if let Some(p) = &self.progress {
            p(phase, completed, total);
        }
        if self.aborted() {
            return Err(interrupted(phase, "it was aborted"));
        }
        Ok(())
    }
}

pub fn interrupted(phase: &str, why: &str) -> rusqlite::Error {
    coded_error(
        "OPERATION_INTERRUPTED",
        format!(
            "Wiki maintenance was interrupted during {} because {}. Nothing was published; run it again.",
            phase, why
        ),
    )
}

// ---------------------------------------------------------------------------
// Lease
// ---------------------------------------------------------------------------

/// Exclusive maintenance lease on one index (released on drop, or when the process dies).
pub struct MaintenanceLease {
    file: fs::File,
    path: PathBuf,
}

impl MaintenanceLease {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for MaintenanceLease {
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        let _ = self.file.unlock();
    }
}

pub fn lease_path(db_path: &Path) -> PathBuf {
    let mut name = db_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".lease");
    db_path.with_file_name(name)
}

/// Take the maintenance lease, waiting up to `wait` for a holder to finish.
pub fn acquire_lease(db_path: &Path, wait: Duration) -> rusqlite::Result<MaintenanceLease> {
    let path = lease_path(db_path);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| {
            coded_error(
                "WIKI_INDEX_BUSY",
                format!("Cannot open {}: {}", path.display(), e),
            )
        })?;
    let deadline = Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(fs::TryLockError::WouldBlock) => {
                let holder = fs::read_to_string(&path).unwrap_or_default();
                return Err(coded_error(
                    "WIKI_INDEX_BUSY",
                    format!(
                        "Another process holds the wiki index maintenance lease ({}). Retry when it finishes.",
                        if holder.trim().is_empty() { "unknown holder" } else { holder.trim() }
                    ),
                ));
            }
            Err(fs::TryLockError::Error(e)) => {
                return Err(coded_error(
                    "WIKI_INDEX_BUSY",
                    format!("Cannot lock {}: {}", path.display(), e),
                ))
            }
        }
    }
    let mut lease = MaintenanceLease { file, path };
    let _ = lease.file.set_len(0);
    let _ = write!(
        lease.file,
        "pid {} since {}",
        std::process::id(),
        chrono::Utc::now().to_rfc3339()
    );
    Ok(lease)
}

// ---------------------------------------------------------------------------
// Corpus observation
// ---------------------------------------------------------------------------

/// A cheap binding of the corpus: discovered paths with size and modification time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorpusObservation {
    pub revision: String,
    pub files: usize,
}

pub fn observe_corpus(scope: &WikiScope) -> CorpusObservation {
    let discovery = scope.discover_checked();
    let mut h = Sha256::new();
    h.update(b"knobyte-wiki-corpus-v1\0");
    h.update(scope.config_digest().as_bytes());
    for (rel, abs) in &discovery.files {
        h.update(rel.as_bytes());
        h.update(b"\0");
        match fs::metadata(abs) {
            Ok(m) => {
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                h.update(format!("{}:{}\0", m.len(), mtime).as_bytes());
            }
            Err(_) => h.update(b"unreadable\0"),
        }
    }
    if discovery.limit_exceeded {
        h.update(b"limit-exceeded\0");
    }
    CorpusObservation {
        revision: hex::encode(h.finalize()),
        files: discovery.files.len(),
    }
}

/// Fails with `OPERATION_INTERRUPTED` when the corpus moved since `expected` was observed.
pub fn assert_corpus_unchanged(
    expected: &CorpusObservation,
    scope: &WikiScope,
    phase: &str,
) -> rusqlite::Result<()> {
    if observe_corpus(scope).revision != expected.revision {
        return Err(interrupted(phase, "the wiki Markdown changed while it ran"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Integrity and status
// ---------------------------------------------------------------------------

/// `PRAGMA quick_check` problems (empty when the database is sound).
pub fn quick_check(conn: &Connection) -> Vec<String> {
    let mut out = Vec::new();
    match conn.prepare("PRAGMA quick_check") {
        Ok(mut stmt) => {
            let rows = stmt.query_map([], |r| r.get::<_, String>(0));
            match rows {
                Ok(rows) => {
                    for r in rows {
                        match r {
                            Ok(s) if s == "ok" => {}
                            Ok(s) => out.push(s),
                            Err(e) => out.push(e.to_string()),
                        }
                    }
                }
                Err(e) => out.push(e.to_string()),
            }
        }
        Err(e) => out.push(e.to_string()),
    }
    out
}

/// What the index looks like right now.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexStatus {
    /// One of [`INDEX_STATES`].
    pub state: String,
    pub observed_at: String,
    pub schema_version: Option<u32>,
    /// Digest of the indexed corpus (file paths and content hashes, config digest).
    pub indexed_revision: Option<String>,
    pub indexed_at: Option<String>,
    pub entities: usize,
    /// True when the Markdown or config moved on since the index was built. Reported on its
    /// own because a worse state (`migration_required`) must not hide it.
    #[serde(default)]
    pub stale: bool,
    pub diagnostics: Vec<WikiDiagnostic>,
}

impl IndexStatus {
    /// The state as every adapter reports it (CLI `index status`, `validate --json`,
    /// `doctor`, MCP tools): one vocabulary, one shape.
    pub fn state_value(&self) -> serde_json::Value {
        serde_json::json!({
            "state": self.state,
            "stale": self.stale,
            "readable": self.readable(),
            "schemaVersion": self.schema_version,
            "indexedRevision": self.indexed_revision,
            "indexedAt": self.indexed_at,
        })
    }

    /// Reads are served in these states.
    pub fn readable(&self) -> bool {
        matches!(
            self.state.as_str(),
            "fresh" | "stale" | "degraded" | "migration_required"
        )
    }
}

fn status(
    state: &str,
    schema_version: Option<u32>,
    indexed_revision: Option<String>,
    indexed_at: Option<String>,
    entities: usize,
    mut diagnostics: Vec<WikiDiagnostic>,
) -> IndexStatus {
    diagnostics.truncate(MAX_DIAGNOSTICS);
    IndexStatus {
        state: state.to_string(),
        observed_at: chrono::Utc::now().to_rfc3339(),
        schema_version,
        indexed_revision,
        indexed_at,
        entities,
        stale: state == "stale",
        diagnostics,
    }
}

fn read_schema_version(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM wiki_meta WHERE key = 'schema_version'",
        [],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// Inspect the index and the corpus without creating or changing either. `check_migration`
/// also parses the corpus to detect older Knobyte formats (`migration_required`).
pub fn inspect_index(db_path: &Path, scope: &WikiScope, check_migration: bool) -> IndexStatus {
    if !db_path.exists() {
        return status(
            "missing",
            None,
            None,
            None,
            0,
            vec![diag(
                "WIKI_INDEX_MISSING",
                format!(
                    "No wiki index at {}. Run `knobyte wiki rebuild-index`.",
                    db_path.display()
                ),
                "wiki.db",
            )],
        );
    }
    let size = fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    if size > MAX_INDEX_BYTES {
        return status(
            "corrupt",
            None,
            None,
            None,
            0,
            vec![diag(
                "WIKI_CORPUS_LIMIT_EXCEEDED",
                format!(
                    "The wiki index is {} bytes (bound {}); it was not opened. Rebuild it.",
                    size, MAX_INDEX_BYTES
                ),
                "wiki.db",
            )],
        );
    }
    let conn = match Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(e) => {
            return status(
                "corrupt",
                None,
                None,
                None,
                0,
                vec![diag(
                    "WIKI_INDEX_CORRUPT",
                    format!("The wiki index cannot be opened: {}", e),
                    "wiki.db",
                )],
            )
        }
    };
    let _ = conn.busy_timeout(Duration::from_secs(5));
    let problems = quick_check(&conn);
    if !problems.is_empty() {
        return status(
            "corrupt",
            None,
            None,
            None,
            0,
            vec![diag(
                "WIKI_INDEX_CORRUPT",
                format!(
                    "The wiki index failed its integrity check: {}",
                    problems.join("; ")
                ),
                "wiki.db",
            )],
        );
    }
    let has_meta = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'wiki_meta'",
            [],
            |_| Ok(()),
        )
        .optional()
        .ok()
        .flatten()
        .is_some();
    let version = if has_meta {
        read_schema_version(&conn)
    } else {
        None
    };
    let schema_version = version.as_deref().and_then(|v| v.parse::<u32>().ok());
    if version.as_deref() != Some(WIKI_INDEX_SCHEMA_VERSION) {
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if tables == 0 {
            return status(
                "missing",
                None,
                None,
                None,
                0,
                vec![diag(
                    "WIKI_INDEX_MISSING",
                    "The wiki index has not been built. Run `knobyte wiki rebuild-index`.",
                    "wiki.db",
                )],
            );
        }
        let current: u32 = WIKI_INDEX_SCHEMA_VERSION.parse().unwrap_or(0);
        let newer = schema_version.is_some_and(|v| v > current);
        return status(
            "rebuild_required",
            schema_version,
            None,
            None,
            0,
            vec![diag(
                "WIKI_INDEX_REBUILD_REQUIRED",
                if newer {
                    format!(
                        "The wiki index was built by a newer Knobyte (schema {}; this build reads {}). It is left untouched; upgrade Knobyte, or remove it and run `knobyte wiki rebuild-index`.",
                        version.as_deref().unwrap_or("(unknown)"),
                        WIKI_INDEX_SCHEMA_VERSION
                    )
                } else {
                    format!(
                        "The wiki index was built by schema {}; this build reads {}. Run `knobyte wiki rebuild-index`.",
                        version.as_deref().unwrap_or("(unknown)"),
                        WIKI_INDEX_SCHEMA_VERSION
                    )
                },
                "wiki.db",
            )],
        );
    }
    drop(conn);
    let index = match WikiIndex::open_read_only(db_path) {
        Ok(i) => i,
        Err(e) => {
            let code = crate::wiki::index::error_code(&e)
                .unwrap_or("WIKI_INDEX_CORRUPT")
                .to_string();
            let state = match code.as_str() {
                "WIKI_INDEX_MISSING" => "missing",
                "WIKI_INDEX_REBUILD_REQUIRED" => "rebuild_required",
                _ => "corrupt",
            };
            return status(
                state,
                schema_version,
                None,
                None,
                0,
                vec![diag(&code, e.to_string(), "wiki.db")],
            );
        }
    };
    if !index.is_built() {
        return status(
            "missing",
            schema_version,
            None,
            None,
            0,
            vec![diag(
                "WIKI_INDEX_MISSING",
                "The wiki index has not been built. Run `knobyte wiki rebuild-index`.",
                "wiki.db",
            )],
        );
    }
    let revision = index.indexed_revision().ok();
    let indexed_at = index.meta_value("last_refresh");
    let entities = index.entity_count().unwrap_or(0);
    let mut diags: Vec<WikiDiagnostic> = Vec::new();
    let freshness = index.freshness(&scope.scaffold_root).unwrap_or_default();
    let migration = check_migration && crate::wiki::migrate::migration_required(scope);
    if migration {
        diags.push(diag(
            "WIKI_MIGRATION_REQUIRED",
            "The wiki holds older Knobyte formats (legacy status values, `document` types, derived ids, old grounding shapes or legacy `edges`). Run `knobyte wiki migrate --apply`.",
            "",
        ));
    }
    if freshness.stale {
        let mut changed: Vec<String> = freshness
            .added
            .iter()
            .chain(freshness.changed.iter())
            .chain(freshness.removed.iter())
            .cloned()
            .collect();
        changed.truncate(10);
        diags.push(diag(
            "INDEX_REFRESH_REQUIRED",
            if freshness.config_changed {
                "The wiki configuration changed since the index was built. Run `knobyte wiki rebuild-index`.".to_string()
            } else {
                format!(
                    "The wiki Markdown changed since the index was built ({}). Run `knobyte wiki rebuild-index`.",
                    changed.join(", ")
                )
            },
            "wiki.db",
        ));
        let mut st = status(
            if migration { "migration_required" } else { "stale" },
            schema_version,
            revision,
            indexed_at,
            entities,
            diags,
        );
        st.stale = true;
        return st;
    }
    if migration {
        return status(
            "migration_required",
            schema_version,
            revision,
            indexed_at,
            entities,
            diags,
        );
    }
    let stored_errors = index
        .stored_diagnostics(MAX_DIAGNOSTICS)
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.severity == "error")
        .collect::<Vec<_>>();
    if freshness.limit_exceeded || !stored_errors.is_empty() {
        diags.extend(stored_errors);
        if freshness.limit_exceeded {
            diags.push(diag(
                "WIKI_CORPUS_LIMIT_EXCEEDED",
                "The wiki corpus exceeds a safety bound; the index covers only part of it.",
                "",
            ));
        }
        return status(
            "degraded",
            schema_version,
            revision,
            indexed_at,
            entities,
            diags,
        );
    }
    status(
        "fresh",
        schema_version,
        revision,
        indexed_at,
        entities,
        diags,
    )
}

// ---------------------------------------------------------------------------
// Dump and doctor
// ---------------------------------------------------------------------------

/// `(table, ordered columns, ORDER BY)` of the normalized dump. Wall-clock columns are left
/// out (`wiki_files.indexed_at`), as are the `wiki_meta` keys in [`DUMP_EXCLUDED_META`].
pub const DUMP_TABLES: [(&str, &str, &str); 9] = [
    ("wiki_diagnostics", "file, code, message, line, severity, entity_id, path", "file, line, code, entity_id, path, message, severity"),
    ("wiki_entities", "entity_key, id, shadowed, file, position, type, title, summary, body, status, revision, start_line, end_line, heading_depth, content_hash, metadata_kind, health, aliases, provenance, metadata", "entity_key"),
    ("wiki_files", "path, content_hash, parse_status, entity_count, text_length", "path"),
    ("wiki_groundings", "entity_key, ordinal, node_id, health, state, origin, body_hash, fingerprint", "entity_key, ordinal"),
    ("wiki_meta", "key, value", "key"),
    ("wiki_relations", "source_key, ordinal, type, target_id, target_resolved, note, waived", "source_key, ordinal"),
    ("wiki_revision_marks", "id, revision, content_hash", "id"),
    ("wiki_sources", "entity_key, ordinal, type, ref, note, repository, commit_sha, captured_at, identity", "entity_key, ordinal"),
    ("wiki_topics", "entity_key, ordinal, topic_ref, topic_id", "entity_key, ordinal"),
];

/// Wall-clock or machine-specific `wiki_meta` keys left out of the dump.
pub const DUMP_EXCLUDED_META: [&str; 3] = ["last_refresh", "last_rebuild", "scaffold_root"];

fn value_json(v: rusqlite::types::ValueRef) -> serde_json::Value {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => serde_json::json!(i),
        ValueRef::Real(f) => serde_json::json!(f),
        ValueRef::Text(t) => serde_json::json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => serde_json::json!(hex::encode(b)),
    }
}

/// Deterministic, normalized dump of an index: one `table<TAB>[values]` line per row, rows
/// ordered by each table's key (never by rowid). Two indexes holding the same content dump to
/// the same bytes however they were written.
pub fn dump_index(conn: &Connection) -> rusqlite::Result<String> {
    let mut out = String::new();
    for (table, columns, order) in DUMP_TABLES {
        let exists = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            continue;
        }
        let filter = if table == "wiki_meta" {
            format!(
                " WHERE key NOT IN ({})",
                DUMP_EXCLUDED_META
                    .iter()
                    .map(|k| format!("'{}'", k))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            String::new()
        };
        let sql = format!(
            "SELECT {} FROM {}{} ORDER BY {}",
            columns, table, filter, order
        );
        let mut stmt = conn.prepare(&sql)?;
        let n = stmt.column_count();
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let mut values = Vec::with_capacity(n);
            for i in 0..n {
                values.push(value_json(r.get_ref(i)?));
            }
            out.push_str(table);
            out.push('\t');
            out.push_str(&serde_json::Value::Array(values).to_string());
            out.push('\n');
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct DumpDiff {
    pub consistent: bool,
    /// Rows the live index holds that a clean rebuild does not (at most [`MAX_DIAGNOSTICS`]).
    pub only_in_index: Vec<String>,
    /// Rows a clean rebuild holds that the live index does not.
    pub only_in_rebuild: Vec<String>,
    pub truncated: bool,
}

/// Tables that carry history a clean rebuild cannot reproduce (`wiki_revision_marks` records
/// the content hash an entity had when each revision was first seen): dumped, never diffed.
pub const HISTORY_TABLES: [&str; 1] = ["wiki_revision_marks"];

fn derived_row(line: &str) -> bool {
    !HISTORY_TABLES
        .iter()
        .any(|t| line.strip_prefix(t).is_some_and(|r| r.starts_with('\t')))
}

/// Line-set difference of two dumps (history tables excluded).
pub fn diff_dumps(live: &str, rebuilt: &str) -> DumpDiff {
    use std::collections::BTreeSet;
    let a: BTreeSet<&str> = live.lines().filter(|l| derived_row(l)).collect();
    let b: BTreeSet<&str> = rebuilt.lines().filter(|l| derived_row(l)).collect();
    let mut only_a: Vec<String> = a.difference(&b).map(|s| s.to_string()).collect();
    let mut only_b: Vec<String> = b.difference(&a).map(|s| s.to_string()).collect();
    let truncated = only_a.len() > MAX_DIAGNOSTICS || only_b.len() > MAX_DIAGNOSTICS;
    only_a.truncate(MAX_DIAGNOSTICS);
    only_b.truncate(MAX_DIAGNOSTICS);
    DumpDiff {
        consistent: only_a.is_empty() && only_b.is_empty(),
        only_in_index: only_a,
        only_in_rebuild: only_b,
        truncated,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DoctorReport {
    pub status: IndexStatus,
    pub quick_check: Vec<String>,
    /// Comparison with a clean rebuild of the current corpus (absent when the index is
    /// unreadable or the rebuild failed).
    pub diff: Option<DumpDiff>,
    pub diagnostics: Vec<WikiDiagnostic>,
}

/// `knobyte wiki index doctor`: integrity check, state, and a diff of the live index against
/// a clean rebuild of the current corpus built next to it (then removed). Read-only for the
/// live index.
pub fn doctor(db_path: &Path, scope: &WikiScope) -> DoctorReport {
    let status_now = inspect_index(db_path, scope, true);
    let mut diagnostics = Vec::new();
    let mut quick = Vec::new();
    // An unreadable index (missing, corrupt, rebuild_required — including one written by a
    // newer schema) is reported, never dumped or compared.
    let live_dump = if db_path.exists() && status_now.readable() {
        match Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => {
                quick = quick_check(&c);
                dump_index(&c).ok()
            }
            Err(_) => None,
        }
    } else {
        None
    };
    let diff = live_dump.and_then(|live| {
        let mut name = db_path
            .file_name()
            .map(|n| n.to_os_string())
            .unwrap_or_default();
        name.push(format!(".doctor-{}", std::process::id()));
        let tmp = db_path.with_file_name(name);
        let cleanup = |p: &Path| {
            for suffix in ["", "-wal", "-shm", "-journal", ".lease"] {
                let mut s = p.as_os_str().to_os_string();
                s.push(suffix);
                let _ = fs::remove_file(PathBuf::from(s));
            }
        };
        cleanup(&tmp);
        let rebuilt = WikiIndex::open_for_rebuild(&tmp).and_then(|mut i| {
            i.rebuild(&scope.scaffold_root)?;
            dump_index(i.connection())
        });
        cleanup(&tmp);
        match rebuilt {
            Ok(rebuilt) => Some(diff_dumps(&live, &rebuilt)),
            Err(e) => {
                diagnostics.push(diag(
                    crate::wiki::index::error_code(&e).unwrap_or("WIKI_INDEX_CORRUPT"),
                    format!("A clean rebuild for comparison failed: {}", e),
                    "wiki.db",
                ));
                None
            }
        }
    });
    if let Some(d) = &diff {
        if !d.consistent {
            diagnostics.push(diag(
                "INDEX_REFRESH_REQUIRED",
                format!(
                    "The index differs from a clean rebuild ({} row(s) only in the index, {} only in the rebuild). Run `knobyte wiki rebuild-index`.",
                    d.only_in_index.len(),
                    d.only_in_rebuild.len()
                ),
                "wiki.db",
            ));
        }
    }
    diagnostics.extend(status_now.diagnostics.iter().cloned());
    diagnostics.truncate(MAX_DIAGNOSTICS);
    DoctorReport {
        status: status_now,
        quick_check: quick,
        diff,
        diagnostics,
    }
}
