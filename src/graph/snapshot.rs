//! Graph snapshot provenance (`graph_snapshot_v1` in `project_metadata`).
//!
//! Every publication records what the published rows were built from: a unique publication
//! id, the repository branch and HEAD, the schema / extractor / grammar versions, the corpus
//! policy, and a digest of the indexed sources (sorted path + content hash) with the parse
//! totals. `graph status` validates the snapshot against the stored rows (a disagreement means
//! the store was altered outside a publication), compares it with the working tree (branch
//! switch, grammar change) and uses the publication id to detect a publication racing an
//! inspection; read sessions use it to prove they answer from the inspected publication.

use rusqlite::{params, Connection, OptionalExtension, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

use crate::graph::extractor::EXTRACTOR_VERSION;
use crate::graph::git_state::repo_state;
use crate::graph::schema::CURRENT_SCHEMA_VERSION;

pub const SNAPSHOT_KEY: &str = "graph_snapshot_v1";
pub const PUBLICATION_ID_KEY: &str = "publication_id";
pub const SNAPSHOT_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotParseHealth {
    pub total: usize,
    pub ok: usize,
    pub partial: usize,
    pub failed: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphSnapshot {
    pub version: u32,
    pub publication_id: String,
    pub indexed_at: String,
    pub indexed_branch: Option<String>,
    pub indexed_head: Option<String>,
    pub schema_version: i64,
    pub extractor_version: String,
    pub grammar_hash: String,
    pub policy_hash: String,
    pub source_corpus_digest: String,
    pub source_count: usize,
    pub parse_health: SnapshotParseHealth,
}

/// Identity of the tree-sitter grammars the extractors parse with (ABI version and symbol /
/// field tables). A grammar upgrade changes it, so a graph built with other grammars is stale.
pub fn grammar_hash() -> String {
    let langs: [(&str, tree_sitter::Language); 7] = [
        ("rust", tree_sitter_rust::LANGUAGE.into()),
        ("typescript", tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        ("tsx", tree_sitter_typescript::LANGUAGE_TSX.into()),
        ("javascript", tree_sitter_javascript::LANGUAGE.into()),
        ("python", tree_sitter_python::LANGUAGE.into()),
        ("csharp", tree_sitter_c_sharp::LANGUAGE.into()),
        ("swift", tree_sitter_swift::LANGUAGE.into()),
    ];
    let mut h = Sha256::new();
    h.update(b"knobyte-grammars-1\0");
    for (name, lang) in langs {
        h.update(
            format!(
                "{}:{}:{}:{}\n",
                name,
                lang.abi_version(),
                lang.node_kind_count(),
                lang.field_count()
            )
            .as_bytes(),
        );
    }
    hex::encode(h.finalize())[..16].to_string()
}

/// Digest, count and parse totals of the `files` rows of `conn`.
pub fn stored_sources(conn: &Connection) -> Result<(String, usize, SnapshotParseHealth)> {
    let mut stmt = conn.prepare("SELECT path, content_hash, parse_status FROM files ORDER BY path, content_hash")?;
    let mut rows = stmt.query([])?;
    let mut h = Sha256::new();
    h.update(b"knobyte-source-corpus-1\0");
    let mut ph = SnapshotParseHealth::default();
    while let Some(r) = rows.next()? {
        let (path, hash, status): (String, String, String) = (r.get(0)?, r.get(1)?, r.get(2)?);
        h.update(path.as_bytes());
        h.update([0]);
        h.update(hash.as_bytes());
        h.update([0]);
        ph.total += 1;
        match status.as_str() {
            "partial" => ph.partial += 1,
            "failed" => ph.failed += 1,
            _ => ph.ok += 1,
        }
    }
    Ok((hex::encode(h.finalize()), ph.total, ph))
}

fn new_publication_id(digest: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut h = Sha256::new();
    h.update(format!("{}:{}:{}", digest, std::process::id(), nanos).as_bytes());
    hex::encode(h.finalize())[..24].to_string()
}

/// Build the snapshot describing the rows of `conn` (a candidate or the live graph) and store
/// it, with a fresh publication id, in `conn`'s `project_metadata`.
pub fn write_snapshot(conn: &Connection, root: &Path, policy_hash: &str) -> Result<GraphSnapshot> {
    let (digest, count, parse_health) = stored_sources(conn)?;
    let repo = repo_state(root).unwrap_or_default();
    let snapshot = GraphSnapshot {
        version: SNAPSHOT_VERSION,
        publication_id: new_publication_id(&digest),
        indexed_at: chrono::Utc::now().to_rfc3339(),
        indexed_branch: repo.branch,
        indexed_head: repo.head,
        schema_version: CURRENT_SCHEMA_VERSION,
        extractor_version: EXTRACTOR_VERSION.to_string(),
        grammar_hash: grammar_hash(),
        policy_hash: policy_hash.to_string(),
        source_corpus_digest: digest,
        source_count: count,
        parse_health,
    };
    store_snapshot(conn, &snapshot)?;
    Ok(snapshot)
}

/// Store `snapshot` (and its publication id) in `conn`'s `project_metadata`.
pub fn store_snapshot(conn: &Connection, snapshot: &GraphSnapshot) -> Result<()> {
    let now = chrono::Utc::now().timestamp_millis();
    let json = serde_json::to_string(snapshot).unwrap_or_default();
    for (k, v) in [(SNAPSHOT_KEY, json.as_str()), (PUBLICATION_ID_KEY, snapshot.publication_id.as_str())] {
        conn.execute(
            "INSERT OR REPLACE INTO project_metadata (key, value, updated_at) VALUES (?1, ?2, ?3)",
            params![k, v, now],
        )?;
    }
    Ok(())
}

/// The stored snapshot: `Ok(None)` for a graph published before snapshots existed,
/// `Err(reason)` when the stored value is not a valid snapshot.
pub fn read_snapshot(conn: &Connection) -> std::result::Result<Option<GraphSnapshot>, String> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM project_metadata WHERE key = ?1",
            params![SNAPSHOT_KEY],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some(raw) = raw else { return Ok(None) };
    let s: GraphSnapshot =
        serde_json::from_str(&raw).map_err(|e| format!("The graph snapshot metadata is not valid: {}", e))?;
    if s.version != SNAPSHOT_VERSION {
        return Err(format!("Unsupported graph snapshot version {}.", s.version));
    }
    if s.publication_id.is_empty() || s.source_corpus_digest.len() != 64 {
        return Err("The graph snapshot metadata is incomplete.".to_string());
    }
    Ok(Some(s))
}

/// The publication id of `conn` (`None` before snapshots existed).
pub fn publication_id(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM project_metadata WHERE key = ?1",
        params![PUBLICATION_ID_KEY],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}

/// The publication id of the database at `db_path`, read through a short-lived read-only
/// connection (`None` when unreadable or before snapshots existed).
pub fn publication_id_at(db_path: &Path) -> Option<String> {
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()?;
    let _ = conn.pragma_update(None, "busy_timeout", 5000);
    publication_id(&conn)
}
