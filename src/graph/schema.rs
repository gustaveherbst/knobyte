use rusqlite::{Connection, Result};

pub const CURRENT_SCHEMA_VERSION: i64 = 6;

pub const GRAPH_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS schema_versions (
    version INTEGER PRIMARY KEY,
    applied_at INTEGER NOT NULL,
    description TEXT
);

CREATE TABLE IF NOT EXISTS nodes (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    qualified_name TEXT NOT NULL,
    container_id TEXT,
    identity_key TEXT NOT NULL,
    file_path TEXT NOT NULL,
    language TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    start_column INTEGER NOT NULL,
    end_column INTEGER NOT NULL,
    docstring TEXT,
    signature TEXT,
    visibility TEXT,
    is_exported INTEGER DEFAULT 0,
    is_async INTEGER DEFAULT 0,
    is_static INTEGER DEFAULT 0,
    is_abstract INTEGER DEFAULT 0,
    decorators TEXT,
    type_parameters TEXT,
    return_type TEXT,
    body_hash TEXT,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source TEXT NOT NULL,
    target TEXT NOT NULL,
    kind TEXT NOT NULL,
    metadata TEXT,
    line INTEGER,
    col INTEGER,
    provenance TEXT DEFAULT NULL,
    confidence REAL NOT NULL DEFAULT 1.0,
    resolution_method TEXT,
    evidence TEXT,
    FOREIGN KEY (source) REFERENCES nodes(id) ON DELETE CASCADE,
    FOREIGN KEY (target) REFERENCES nodes(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS files (
    path TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    language TEXT NOT NULL,
    size INTEGER NOT NULL,
    modified_at INTEGER NOT NULL,
    indexed_at INTEGER NOT NULL,
    node_count INTEGER DEFAULT 0,
    errors TEXT,
    parse_status TEXT NOT NULL DEFAULT 'ok' CHECK(parse_status IN ('ok','partial','failed')),
    diagnostic_count INTEGER NOT NULL DEFAULT 0,
    missing_count INTEGER NOT NULL DEFAULT 0,
    error_coverage REAL NOT NULL DEFAULT 0,
    extractor_version TEXT NOT NULL DEFAULT 'knobyte-0.9.1'
);

CREATE TABLE IF NOT EXISTS unresolved_refs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ref_key TEXT NOT NULL UNIQUE,
    from_node_id TEXT NOT NULL,
    reference_name TEXT NOT NULL,
    reference_kind TEXT NOT NULL,
    line INTEGER NOT NULL,
    col INTEGER NOT NULL,
    candidates TEXT,
    file_path TEXT NOT NULL DEFAULT '',
    language TEXT NOT NULL DEFAULT 'unknown',
    receiver TEXT,
    qualifier TEXT,
    import_source TEXT,
    metadata TEXT,
    status TEXT NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','resolved','ambiguous','unresolved')),
    target_id TEXT,
    confidence REAL,
    resolver TEXT,
    FOREIGN KEY (from_node_id) REFERENCES nodes(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS import_bindings (
    binding_key TEXT PRIMARY KEY,
    file_path TEXT NOT NULL,
    local_name TEXT NOT NULL,
    imported_name TEXT NOT NULL,
    module_specifier TEXT NOT NULL,
    resolved_file_path TEXT,
    target_id TEXT,
    is_type_only INTEGER NOT NULL DEFAULT 0,
    metadata TEXT,
    FOREIGN KEY (target_id) REFERENCES nodes(id) ON DELETE SET NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS nodes_fts USING fts5(
    id,
    name,
    qualified_name,
    docstring,
    signature,
    content='nodes',
    content_rowid='rowid'
);

CREATE TRIGGER IF NOT EXISTS nodes_ai AFTER INSERT ON nodes BEGIN
    INSERT INTO nodes_fts(rowid, id, name, qualified_name, docstring, signature)
    VALUES (NEW.rowid, NEW.id, NEW.name, NEW.qualified_name, NEW.docstring, NEW.signature);
END;

CREATE TRIGGER IF NOT EXISTS nodes_ad AFTER DELETE ON nodes BEGIN
    INSERT INTO nodes_fts(nodes_fts, rowid, id, name, qualified_name, docstring, signature)
    VALUES ('delete', OLD.rowid, OLD.id, OLD.name, OLD.qualified_name, OLD.docstring, OLD.signature);
END;

CREATE TRIGGER IF NOT EXISTS nodes_au AFTER UPDATE ON nodes BEGIN
    INSERT INTO nodes_fts(nodes_fts, rowid, id, name, qualified_name, docstring, signature)
    VALUES ('delete', OLD.rowid, OLD.id, OLD.name, OLD.qualified_name, OLD.docstring, OLD.signature);
    INSERT INTO nodes_fts(rowid, id, name, qualified_name, docstring, signature)
    VALUES (NEW.rowid, NEW.id, NEW.name, NEW.qualified_name, NEW.docstring, NEW.signature);
END;

CREATE INDEX IF NOT EXISTS idx_nodes_kind ON nodes(kind);
CREATE INDEX IF NOT EXISTS idx_nodes_name ON nodes(name);
CREATE INDEX IF NOT EXISTS idx_nodes_qualified_name ON nodes(qualified_name);
CREATE INDEX IF NOT EXISTS idx_nodes_file_path ON nodes(file_path);
CREATE INDEX IF NOT EXISTS idx_nodes_language ON nodes(language);
CREATE INDEX IF NOT EXISTS idx_nodes_file_line ON nodes(file_path, start_line);
CREATE INDEX IF NOT EXISTS idx_nodes_lower_name ON nodes(lower(name));
CREATE UNIQUE INDEX IF NOT EXISTS idx_nodes_identity_key ON nodes(identity_key);
CREATE INDEX IF NOT EXISTS idx_nodes_container_id ON nodes(container_id);

CREATE INDEX IF NOT EXISTS idx_edges_kind ON edges(kind);
CREATE INDEX IF NOT EXISTS idx_edges_source_kind ON edges(source, kind);
CREATE INDEX IF NOT EXISTS idx_edges_target_kind ON edges(target, kind);

-- Content-addressed extraction cache used by incremental `graph refresh`: a file whose content
-- hash and extractor version match is not parsed again.
CREATE TABLE IF NOT EXISTS file_extraction_cache (
    path TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    extractor_version TEXT NOT NULL,
    payload TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS project_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Source-chunk full-text index (see graph::chunks): overlapping 80-line windows per file.
CREATE VIRTUAL TABLE IF NOT EXISTS code_chunks USING fts5(
    file_path UNINDEXED,
    content_hash UNINDEXED,
    start_line UNINDEXED,
    end_line UNINDEXED,
    path_terms,
    identifier_terms,
    comment_terms
);

-- Reconciler tier 2 (see graph::reconcile): MinHash sketch of each declaration body and its
-- LSH band buckets.
CREATE TABLE IF NOT EXISTS node_minhash (
    node_id TEXT PRIMARY KEY,
    minhash BLOB NOT NULL,
    token_count INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS node_lsh (
    band INTEGER NOT NULL,
    bucket INTEGER NOT NULL,
    node_id TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_node_lsh_bucket ON node_lsh(band, bucket);

-- Callers + callees of a grounded symbol when its baseline was recorded (reconciler
-- neighbour evidence). Preserved across `graph rebuild`.
CREATE TABLE IF NOT EXISTS _knobyte_grounded_neighbors (
    subject_kind TEXT NOT NULL,
    subject_id   TEXT NOT NULL,
    node_id      TEXT NOT NULL,
    neighbors    TEXT NOT NULL,
    PRIMARY KEY (subject_kind, subject_id, node_id)
);

-- Grounding baselines (see graph::grounding). Preserved across `graph rebuild`.
-- subject_kind='doc', subject_id=scaffold doc path, node_id=grounding ref as written,
-- fingerprint=graph node id the ref resolved to when the baseline was recorded.
CREATE TABLE IF NOT EXISTS _knobyte_grounded_source (
    subject_kind  TEXT NOT NULL DEFAULT 'scaffold',
    subject_id    TEXT NOT NULL,
    node_id       TEXT NOT NULL,
    source        TEXT NOT NULL,
    body_hash     TEXT NOT NULL,
    fingerprint   TEXT NOT NULL,
    scaffold_file TEXT GENERATED ALWAYS AS (
        CASE WHEN subject_kind = 'scaffold' THEN subject_id END
    ) VIRTUAL,
    PRIMARY KEY (subject_kind, subject_id, node_id)
);

CREATE INDEX IF NOT EXISTS idx_grounded_node ON _knobyte_grounded_source(node_id);
CREATE INDEX IF NOT EXISTS idx_grounded_subject ON _knobyte_grounded_source(subject_kind, subject_id);
"#;

/// Tables created by schema versions <= 4 that were never populated or queried.
pub const DROP_UNUSED_TABLES_SQL: &str = r#"
DROP TABLE IF EXISTS lsh_buckets;
DROP TABLE IF EXISTS node_fingerprints;
DROP TABLE IF EXISTS node_aliases;
DROP TABLE IF EXISTS source_chunks;
"#;

/// Tables fully derived from source by a build; replaced as a unit when a candidate is
/// published. Everything else (grounding baselines, schema history) is preserved.
pub const DERIVED_TABLES: [&str; 9] = [
    "edges",
    "unresolved_refs",
    "import_bindings",
    "nodes",
    "files",
    "file_extraction_cache",
    "code_chunks",
    "node_minhash",
    "node_lsh",
];

pub const SCHEMA_DESCRIPTION: &str =
    "Knobyte graph schema v6 (extraction cache, atomic candidate publication)";

/// Open-time initialisation of a live graph database. Creates missing tables but never records
/// a newer schema version over an older database: the version row is written when a build is
/// published, so an older graph keeps reporting `rebuild_required` until rebuilt.
pub fn initialize_graph_schema(conn: &Connection) -> Result<()> {
    let _ = conn.pragma_update(None, "busy_timeout", 5000);
    // A database recorded under another schema version is left exactly as it is: no DDL, no
    // dropped tables, no journal-mode change. Only an explicit rebuild may replace it.
    if let Some(v) = stored_schema_version(conn)? {
        if v != CURRENT_SCHEMA_VERSION {
            return Ok(());
        }
    }
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let _ = conn.pragma_update(None, "foreign_keys", "ON");
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");

    conn.execute_batch(GRAPH_SCHEMA_SQL)?;
    // Schema v5: drop tables that earlier versions created but never populated or read.
    conn.execute_batch(DROP_UNUSED_TABLES_SQL)?;

    // A brand-new database carries the current version from the start.
    let versions: i64 = conn.query_row("SELECT COUNT(*) FROM schema_versions", [], |r| r.get(0))?;
    if versions == 0 {
        record_schema_version(conn)?;
    }
    Ok(())
}

/// The schema version a database records (`MAX(version)`), or `None` when it has no
/// `schema_versions` rows (a brand-new or foreign file). Fails when the file is not a database.
pub fn stored_schema_version(conn: &Connection) -> Result<Option<i64>> {
    let has_table: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_versions'",
        [],
        |r| r.get(0),
    )?;
    if has_table == 0 {
        return Ok(None);
    }
    conn.query_row("SELECT MAX(version) FROM schema_versions", [], |r| r.get::<_, Option<i64>>(0))
}

/// Replace the recorded schema history with the current version (explicit rebuild only).
pub fn reset_schema_version(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM schema_versions", [])?;
    record_schema_version(conn)
}

pub fn record_schema_version(conn: &Connection) -> Result<()> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT OR IGNORE INTO schema_versions (version, applied_at, description) VALUES (?1, ?2, ?3)",
        rusqlite::params![CURRENT_SCHEMA_VERSION, now_ms, SCHEMA_DESCRIPTION],
    )?;
    Ok(())
}

/// Initialise a disposable build candidate: same schema, no WAL, no durability (a crash only
/// loses the candidate, never the live graph).
pub fn initialize_candidate_schema(conn: &Connection) -> Result<()> {
    let _ = conn.pragma_update(None, "journal_mode", "OFF");
    let _ = conn.pragma_update(None, "synchronous", "OFF");
    let _ = conn.pragma_update(None, "foreign_keys", "ON");
    conn.execute_batch(GRAPH_SCHEMA_SQL)?;
    record_schema_version(conn)
}

/// Oldest graph schema version upgraded in place (v4 and v5 share the v6 derived-table
/// layout; they lack tables and columns that are added, and carry unused tables that are
/// dropped). Anything older, and any newer version, needs `graph rebuild`.
pub const MIN_MIGRATABLE_SCHEMA_VERSION: i64 = 4;

/// True when a database recorded under `version` can be upgraded in place.
pub fn is_migratable(version: i64) -> bool {
    (MIN_MIGRATABLE_SCHEMA_VERSION..CURRENT_SCHEMA_VERSION).contains(&version)
}

/// Columns of every ordinary table of `conn`: table -> [(name, type, notnull, default, pk)].
type ColumnInfo = (String, String, bool, Option<String>, bool);

fn table_columns(conn: &Connection) -> Result<std::collections::BTreeMap<String, Vec<ColumnInfo>>> {
    let mut out = std::collections::BTreeMap::new();
    let tables: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND sql NOT LIKE 'CREATE VIRTUAL%' \
             AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'nodes_fts%' AND name NOT LIKE 'code_chunks%'",
        )?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect::<Result<_>>()?
    };
    for t in tables {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{}\")", t))?;
        let cols = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)? != 0,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)? != 0,
                ))
            })?
            .collect::<Result<Vec<_>>>()?;
        out.insert(t, cols);
    }
    Ok(out)
}

/// Upgrade a graph recorded under an older, migratable schema version to the current one in
/// place: unused tables are dropped, missing tables / indexes / triggers created, missing
/// columns added, the extraction cache (written by an older extractor) cleared, and the
/// current version recorded on top of the existing history. The derived rows are kept: they
/// are re-derived by the next `graph refresh` (the recorded extractor version differs, so the
/// graph reports stale until then). Grounding baselines are untouched.
///
/// Returns the version migrated from, `None` when the database is already current. Fails with
/// `GRAPH_REBUILD_REQUIRED` when the version cannot be migrated (rebuild is the fallback).
pub fn migrate_schema(conn: &Connection) -> Result<Option<i64>> {
    let Some(version) = stored_schema_version(conn)? else {
        return Ok(None);
    };
    if version == CURRENT_SCHEMA_VERSION {
        return Ok(None);
    }
    let rebuild = |why: String| -> rusqlite::Error {
        crate::graph::maintenance::GraphMaintenanceError::new(
            "GRAPH_REBUILD_REQUIRED",
            format!("{} Run `knobyte graph rebuild`.", why),
        )
        .into()
    };
    if !is_migratable(version) {
        return Err(rebuild(format!(
            "The graph index uses schema v{} (current v{}), which cannot be upgraded in place.",
            version, CURRENT_SCHEMA_VERSION
        )));
    }
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(GRAPH_SCHEMA_SQL)?;
    let wanted = table_columns(&reference)?;

    let _ = conn.pragma_update(None, "busy_timeout", 5000);
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        conn.execute_batch(DROP_UNUSED_TABLES_SQL)?;
        // Columns first (indexes of the current schema may name them), then everything else.
        let have = table_columns(conn)?;
        for (table, cols) in &wanted {
            let Some(existing) = have.get(table) else { continue };
            for (name, ty, notnull, default, pk) in cols {
                if existing.iter().any(|c| &c.0 == name) {
                    continue;
                }
                if *pk || (*notnull && default.is_none()) {
                    return Err(rebuild(format!(
                        "The graph index (schema v{}) lacks the required column {}.{}.",
                        version, table, name
                    )));
                }
                let mut def = format!("ALTER TABLE \"{}\" ADD COLUMN \"{}\" {}", table, name, ty);
                if *notnull {
                    def.push_str(" NOT NULL");
                }
                if let Some(d) = default {
                    def.push_str(&format!(" DEFAULT {}", d));
                }
                conn.execute_batch(&def)?;
            }
        }
        conn.execute_batch(GRAPH_SCHEMA_SQL)?;
        conn.execute("DELETE FROM file_extraction_cache", [])?;
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute(
            "INSERT OR REPLACE INTO project_metadata (key, value, updated_at) VALUES ('schema_migrated_from', ?1, ?2)",
            rusqlite::params![version.to_string(), now],
        )?;
        record_schema_version(conn)
    })();
    match result {
        Ok(()) => conn.execute_batch("COMMIT")?,
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    initialize_graph_schema(conn)?;
    Ok(Some(version))
}

/// FTS maintenance triggers on `nodes` (dropped while a candidate is bulk-copied, then the FTS
/// index is rebuilt in one pass).
pub const NODE_FTS_TRIGGERS: [&str; 3] = ["nodes_ai", "nodes_ad", "nodes_au"];
