use rusqlite::{Connection, Result};

pub const CURRENT_SCHEMA_VERSION: i64 = 4;

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

CREATE TABLE IF NOT EXISTS node_aliases (
    alias_id TEXT PRIMARY KEY,
    canonical_node_id TEXT NOT NULL,
    match_method TEXT NOT NULL,
    confidence REAL NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (canonical_node_id) REFERENCES nodes(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS source_chunks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    file_path TEXT NOT NULL,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    content_hash TEXT NOT NULL,
    path_terms TEXT NOT NULL,
    identifier_terms TEXT NOT NULL,
    comment_terms TEXT NOT NULL,
    UNIQUE(file_path, start_line, end_line)
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

CREATE TABLE IF NOT EXISTS project_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS node_fingerprints (
    ref          INTEGER PRIMARY KEY,
    node_id      TEXT NOT NULL UNIQUE REFERENCES nodes(id) ON DELETE CASCADE,
    minhash      BLOB NOT NULL,
    neighbors    TEXT NOT NULL,
    token_count  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS lsh_buckets (
    band      INTEGER NOT NULL,
    band_hash INTEGER NOT NULL,
    ref       INTEGER NOT NULL REFERENCES node_fingerprints(ref) ON DELETE CASCADE,
    PRIMARY KEY (band, band_hash, ref)
) WITHOUT ROWID;

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

pub fn initialize_graph_schema(conn: &Connection) -> Result<()> {
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let _ = conn.pragma_update(None, "foreign_keys", "ON");
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");
    let _ = conn.pragma_update(None, "busy_timeout", 5000);

    conn.execute_batch(GRAPH_SCHEMA_SQL)?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT OR IGNORE INTO schema_versions (version, applied_at, description) VALUES (?1, ?2, ?3)",
        rusqlite::params![CURRENT_SCHEMA_VERSION, now_ms, "Knobyte 0.9.1 Graph Schema"],
    )?;

    Ok(())
}
