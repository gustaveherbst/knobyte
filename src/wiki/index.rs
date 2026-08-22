//! The derived wiki index (`.knobyte/wiki.db`): entities, relations, groundings (with health),
//! sources, topic memberships and an FTS5 table. The Markdown is the source of truth; the
//! index is refreshed incrementally by file content hash and can always be rebuilt.

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use crate::graph::fingerprint::compute_file_hash;
use crate::graph::grounding::{
    resolve_baseline, resolve_grounding_ref, CommittedBaseline, CommittedIndex, DocRef, RefOrigin,
    RefResolution,
};
use crate::wiki::models::{
    CommittedGrounding, EntityRelation, Provenance, WikiDiagnostic, WikiEntity, WikiSource,
    GROUNDING_ORIGIN_ANCHOR, GROUNDING_ORIGIN_FRONTMATTER, HEALTH_STATES,
};
use crate::wiki::parser::parse_markdown_file_with;
use crate::wiki::maintenance::MaintenanceContext;
use crate::wiki::scope::WikiScope;

/// Bumped whenever the table layout changes. A mismatch makes read paths report
/// `WIKI_INDEX_REBUILD_REQUIRED`; only an explicit rebuild resets an older index, and an index
/// from a newer schema is never touched.
/// 3: `wiki_groundings` carries the committed baseline (`body_hash`, `fingerprint`, `origin`).
pub const WIKI_INDEX_SCHEMA_VERSION: &str = "3";

pub const DEFAULT_RESULT_LIMIT: usize = 50;
pub const MAX_RESULT_LIMIT: usize = 500;
pub const DEFAULT_TRAVERSAL_DEPTH: usize = 2;
pub const MAX_TRAVERSAL_DEPTH: usize = 5;
pub const DEFAULT_NEIGHBORHOOD_TOKENS: usize = 4000;
pub const MAX_EDGES_PER_ENTITY: usize = 200;

/// Rough token estimate (4 characters per token) of a JSON-serializable value.
pub fn estimate_tokens<T: Serialize>(v: &T) -> usize {
    serde_json::to_string(v)
        .map(|s| s.len().div_ceil(4))
        .unwrap_or(0)
}

pub fn clamp_limit(limit: Option<usize>) -> usize {
    limit
        .unwrap_or(DEFAULT_RESULT_LIMIT)
        .clamp(1, MAX_RESULT_LIMIT)
}

pub fn clamp_depth(depth: Option<usize>) -> usize {
    depth
        .unwrap_or(DEFAULT_TRAVERSAL_DEPTH)
        .clamp(1, MAX_TRAVERSAL_DEPTH)
}

fn lifecycle_rank(status: &str) -> usize {
    match status {
        "promoted" => 0,
        "in_flight" => 1,
        "deprecated" => 2,
        "archived" => 3,
        _ => 2,
    }
}

pub fn health_rank(health: Option<&str>) -> usize {
    let h = health.unwrap_or("unverified");
    HEALTH_STATES.iter().position(|x| *x == h).unwrap_or(1)
}

fn field_rank(field: &str) -> usize {
    match field {
        "id" => 0,
        "title" => 1,
        "summary" => 2,
        _ => 3,
    }
}

pub const DEFAULT_LINK_PAGE: usize = 25;
pub const MAX_LINK_PAGE: usize = 200;

/// One bounded page of linked entities (backlinks or related).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LinkPage {
    pub items: Vec<EntitySummary>,
    pub total: usize,
    pub truncated: bool,
    pub limit: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}

/// Paging facts of a list carried inline (an entity's outgoing relations).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LinkCount {
    pub total: usize,
    pub truncated: bool,
    pub limit: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
}

/// Options of [`WikiIndex::entity_detail`].
#[derive(Debug, Clone, Default)]
pub struct DetailOptions {
    pub include_body: bool,
    pub include_related: bool,
    /// Page size for relations, backlinks and related (default 25, max 200).
    pub limit: Option<usize>,
    pub relations_offset: usize,
    pub backlinks_offset: usize,
    pub related_offset: usize,
}

/// A bounded entity detail: the entity (relations paged, body opt-in) plus paged links.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityDetail {
    pub entity: WikiEntity,
    pub body_included: bool,
    pub relations_page: LinkCount,
    pub backlinks: LinkPage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related: Option<LinkPage>,
}

/// Compact entity projection used by list/query/related/graph output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EntitySummary {
    pub id: String,
    #[serde(rename = "type")]
    pub entity_type: String,
    pub title: String,
    pub summary: Option<String>,
    pub status: String,
    pub file: String,
    pub revision: i64,
    pub start_line: usize,
    pub end_line: usize,
    pub health: Option<String>,
}

impl From<&WikiEntity> for EntitySummary {
    fn from(e: &WikiEntity) -> Self {
        Self {
            id: e.id.clone(),
            entity_type: e.entity_type.clone(),
            title: e.title.clone(),
            summary: e.summary.clone(),
            status: e.status.clone(),
            file: e.file.clone(),
            revision: e.revision,
            start_line: e.start_line,
            end_line: e.end_line,
            health: e.health.clone(),
        }
    }
}

/// Filters shared by list and query.
#[derive(Debug, Clone, Default)]
pub struct QueryFilter {
    pub types: Vec<String>,
    /// Topic id, title or alias.
    pub topic: Option<String>,
    pub statuses: Vec<String>,
    pub health: Vec<String>,
    pub include_archived: bool,
    pub file: Option<String>,
    pub limit: Option<usize>,
    /// Items to skip (paging); the next page starts at `offset + items.len()` while the page
    /// is `truncated`.
    pub offset: usize,
}

impl QueryFilter {
    fn visible(
        &self,
        e: &WikiEntity,
        topic_id: Option<&str>,
        topics_of: &HashMap<String, Vec<String>>,
    ) -> bool {
        if !self.statuses.is_empty() {
            if !self.statuses.iter().any(|s| s == &e.status) {
                return false;
            }
        } else if e.status == "archived" && !self.include_archived {
            return false;
        }
        if !self.types.is_empty() && !self.types.iter().any(|t| t == &e.entity_type) {
            return false;
        }
        if let Some(f) = &self.file {
            if &e.file != f {
                return false;
            }
        }
        if !self.health.is_empty() {
            let h = e.health.as_deref().unwrap_or("none");
            if !self.health.iter().any(|x| x == h) {
                return false;
            }
        }
        if self.topic.is_some() {
            match topic_id {
                Some(t) => {
                    let member = topics_of
                        .get(&e.entity_key)
                        .is_some_and(|ts| ts.iter().any(|x| x == t));
                    if !member && e.id != t {
                        return false;
                    }
                }
                None => return false,
            }
        }
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    #[serde(flatten)]
    pub entity: EntitySummary,
    /// Best matching field: id | title | summary | body.
    pub matched: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RelationEdge {
    #[serde(rename = "type")]
    pub rel_type: String,
    pub target_id: String,
    pub target: Option<EntitySummary>,
    pub resolved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Neighborhood {
    pub origin: EntitySummary,
    pub relations: Vec<RelationEdge>,
    pub backlinks: Vec<RelationEdge>,
    pub reached: Vec<EntitySummary>,
    pub truncated: bool,
    pub estimated_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    #[serde(rename = "type")]
    pub rel_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphSlice {
    pub nodes: Vec<EntitySummary>,
    pub edges: Vec<GraphEdge>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroundedHit {
    #[serde(flatten)]
    pub entity: EntitySummary,
    /// The first (in query order) query reference this entity matched.
    pub query: String,
    /// Every query reference this entity grounds to, in query order. Results are ranked by
    /// how many of the queried nodes an entity covers.
    pub matched_nodes: Vec<String>,
    /// The grounding reference as written in the entity (for the first matched query).
    pub grounding: String,
    /// Worst health across the entity's matching groundings.
    pub grounding_health: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshStats {
    pub files_added: usize,
    pub files_updated: usize,
    pub files_removed: usize,
    pub files_unchanged: usize,
    pub entities: usize,
}

/// How the recorded index compares with the Markdown on disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexFreshness {
    pub built: bool,
    pub stale: bool,
    pub config_changed: bool,
    pub limit_exceeded: bool,
    pub added: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
}

/// `(reference, health, state)` of one grounding.
pub type GroundingRow = (String, Option<String>, Option<String>);

pub struct WikiIndex {
    conn: Connection,
    db_path: PathBuf,
}

const ENTITY_COLUMNS: &str = "e.entity_key, e.id, e.file, e.type, e.title, e.summary, e.body, e.status, e.revision, e.start_line, e.end_line, e.heading_depth, e.content_hash, e.metadata_kind, e.health, e.aliases, e.provenance, e.metadata";

fn map_entity(r: &rusqlite::Row) -> Result<WikiEntity> {
    let aliases: Option<String> = r.get(15)?;
    let provenance: Option<String> = r.get(16)?;
    let metadata: Option<String> = r.get(17)?;
    Ok(WikiEntity {
        entity_key: r.get(0)?,
        id: r.get(1)?,
        file: r.get(2)?,
        entity_type: r.get(3)?,
        title: r.get(4)?,
        summary: r.get(5)?,
        body: r.get(6)?,
        status: r.get(7)?,
        revision: r.get(8)?,
        relations: Vec::new(),
        grounds_to: Vec::new(),
        committed_groundings: Vec::new(),
        topics: Vec::new(),
        sources: Vec::new(),
        provenance: provenance.and_then(|p| serde_json::from_str::<Provenance>(&p).ok()),
        aliases: aliases
            .and_then(|a| serde_json::from_str(&a).ok())
            .unwrap_or_default(),
        metadata: metadata.and_then(|m| serde_json::from_str(&m).ok()),
        health: r.get(14)?,
        start_line: r.get::<_, i64>(9)? as usize,
        end_line: r.get::<_, i64>(10)? as usize,
        heading_depth: r.get::<_, i64>(11)? as usize,
        content_hash: r.get(12)?,
        metadata_kind: r.get(13)?,
    })
}

/// A typed index error carried through `rusqlite::Error` (so existing callers keep their
/// signatures). The message starts with the diagnostic code; see [`error_code`].
pub fn coded_error(code: &str, message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
        Some(format!("{}: {}", code, message.into())),
    )
}

/// The diagnostic code of an error built by [`coded_error`].
pub fn error_code(e: &rusqlite::Error) -> Option<&str> {
    match e {
        rusqlite::Error::SqliteFailure(_, Some(msg)) => {
            let (code, _) = msg.split_once(": ")?;
            (!code.is_empty() && code.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
                .then_some(code)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum SchemaState {
    /// No tables at all (a brand-new or zero-length file).
    Empty,
    Current,
    /// Older, missing or unparseable version: an explicit rebuild may reset it.
    Stale(Option<String>),
    /// Written by a newer build: never touched.
    Newer(String),
}

fn schema_state(conn: &Connection) -> Result<SchemaState> {
    let tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name != 'wiki_meta'",
        [],
        |r| r.get(0),
    )?;
    let has_meta: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'wiki_meta'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    let version: Option<String> = if has_meta {
        conn.query_row(
            "SELECT value FROM wiki_meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .optional()?
    } else {
        None
    };
    Ok(match version {
        None if tables == 0 => SchemaState::Empty,
        Some(v) if v == WIKI_INDEX_SCHEMA_VERSION => SchemaState::Current,
        Some(v) => {
            let current: u64 = WIKI_INDEX_SCHEMA_VERSION.parse().unwrap_or(0);
            match v.parse::<u64>() {
                Ok(n) if n > current => SchemaState::Newer(v),
                _ => SchemaState::Stale(Some(v)),
            }
        }
        None => SchemaState::Stale(None),
    })
}

fn newer_index_error(db_path: &Path, version: &str) -> rusqlite::Error {
    coded_error(
        "WIKI_INDEX_REBUILD_REQUIRED",
        format!(
            "The wiki index at {} was written by a newer Knobyte (schema {}; this build reads {}). Upgrade Knobyte; the index was left untouched.",
            db_path.display(),
            version,
            WIKI_INDEX_SCHEMA_VERSION
        ),
    )
}

fn check_usable(db_path: &Path, state: &SchemaState) -> Result<()> {
    match state {
        SchemaState::Current | SchemaState::Empty => Ok(()),
        SchemaState::Newer(v) => Err(newer_index_error(db_path, v)),
        SchemaState::Stale(v) => Err(coded_error(
            "WIKI_INDEX_REBUILD_REQUIRED",
            format!(
                "The wiki index at {} was built by schema {}; this build expects {}. Run `knobyte wiki rebuild-index`.",
                db_path.display(),
                v.as_deref().unwrap_or("(unknown)"),
                WIKI_INDEX_SCHEMA_VERSION
            ),
        )),
    }
}

/// Refuse an implausibly large index file before opening it.
fn check_index_size(db_path: &Path) -> Result<()> {
    let size = fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    if size > crate::wiki::maintenance::MAX_INDEX_BYTES {
        return Err(coded_error(
            "WIKI_CORPUS_LIMIT_EXCEEDED",
            format!(
                "The wiki index at {} is {} bytes (bound {}); it was not opened. Delete it and run `knobyte wiki rebuild-index`.",
                db_path.display(),
                size,
                crate::wiki::maintenance::MAX_INDEX_BYTES
            ),
        ));
    }
    Ok(())
}

fn configure_writer(conn: &Connection) {
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");
}

fn reset_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        DROP TRIGGER IF EXISTS wiki_entities_ai;
        DROP TRIGGER IF EXISTS wiki_entities_ad;
        DROP TABLE IF EXISTS wiki_fts;
        DROP TABLE IF EXISTS wiki_entities;
        DROP TABLE IF EXISTS wiki_relations;
        DROP TABLE IF EXISTS wiki_groundings;
        DROP TABLE IF EXISTS wiki_sources;
        DROP TABLE IF EXISTS wiki_topics;
        DROP TABLE IF EXISTS wiki_files;
        DROP TABLE IF EXISTS wiki_diagnostics;
        DROP TABLE IF EXISTS wiki_revision_marks;
        DROP TABLE IF EXISTS wiki_meta;
        "#,
    )
}

fn normalize_name(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

impl WikiIndex {
    /// Open the index for normal use (queries and incremental refresh). A missing or empty
    /// database is initialized; an index built by another schema version is never touched
    /// and reports `WIKI_INDEX_REBUILD_REQUIRED` (only [`WikiIndex::open_for_rebuild`] resets).
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        check_index_size(db_path)?;
        if db_path.exists() {
            // Inspect read-only first so a mismatched index is not even journaled.
            let probe = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let state = schema_state(&probe)?;
            drop(probe);
            check_usable(db_path, &state)?;
        }
        let conn = Connection::open(db_path)?;
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        match schema_state(&conn)? {
            SchemaState::Current => configure_writer(&conn),
            SchemaState::Empty => Self::initialize_schema(&conn)?,
            other => check_usable(db_path, &other)?,
        }
        Ok(Self {
            conn,
            db_path: db_path.to_path_buf(),
        })
    }

    /// Open an existing index strictly read-only (Hub and agent read paths): never creates,
    /// migrates or writes. A missing index is `WIKI_INDEX_MISSING`; one built by another
    /// schema version is `WIKI_INDEX_REBUILD_REQUIRED`.
    pub fn open_read_only(db_path: &Path) -> Result<Self> {
        if !db_path.exists() {
            return Err(coded_error(
                "WIKI_INDEX_MISSING",
                format!("No wiki index at {}", db_path.display()),
            ));
        }
        check_index_size(db_path)?;
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        let state = schema_state(&conn)?;
        if state == SchemaState::Empty {
            return Err(coded_error(
                "WIKI_INDEX_MISSING",
                format!("The wiki index at {} has not been built", db_path.display()),
            ));
        }
        check_usable(db_path, &state)?;
        Ok(Self {
            conn,
            db_path: db_path.to_path_buf(),
        })
    }

    /// Open the index for an explicit rebuild/refresh: an index from an older (or unknown)
    /// schema is reset and recreated. An index written by a NEWER schema is never touched.
    pub fn open_for_rebuild(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if db_path.exists() {
            let probe = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let state = schema_state(&probe)?;
            drop(probe);
            if let SchemaState::Newer(v) = &state {
                return Err(newer_index_error(db_path, v));
            }
        }
        let conn = Connection::open(db_path)?;
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        match schema_state(&conn)? {
            SchemaState::Current => configure_writer(&conn),
            SchemaState::Newer(v) => return Err(newer_index_error(db_path, &v)),
            SchemaState::Stale(_) => {
                reset_schema(&conn)?;
                Self::initialize_schema(&conn)?;
            }
            SchemaState::Empty => Self::initialize_schema(&conn)?,
        }
        Ok(Self {
            conn,
            db_path: db_path.to_path_buf(),
        })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    fn initialize_schema(conn: &Connection) -> Result<()> {
        configure_writer(conn);
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS wiki_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS wiki_files (
                path TEXT PRIMARY KEY,
                content_hash TEXT NOT NULL,
                parse_status TEXT NOT NULL,
                entity_count INTEGER NOT NULL,
                text_length INTEGER NOT NULL,
                indexed_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS wiki_entities (
                entity_key TEXT PRIMARY KEY,
                id TEXT NOT NULL,
                shadowed INTEGER NOT NULL DEFAULT 0,
                file TEXT NOT NULL,
                position INTEGER NOT NULL DEFAULT 0,
                type TEXT NOT NULL,
                title TEXT NOT NULL,
                summary TEXT,
                body TEXT NOT NULL,
                status TEXT NOT NULL,
                revision INTEGER NOT NULL,
                start_line INTEGER NOT NULL DEFAULT 1,
                end_line INTEGER NOT NULL DEFAULT 1,
                heading_depth INTEGER NOT NULL DEFAULT 0,
                content_hash TEXT NOT NULL DEFAULT '',
                metadata_kind TEXT NOT NULL DEFAULT 'implicit',
                health TEXT,
                aliases TEXT,
                provenance TEXT,
                metadata TEXT
            );
            CREATE TABLE IF NOT EXISTS wiki_relations (
                source_key TEXT NOT NULL,
                ordinal INTEGER NOT NULL,
                type TEXT NOT NULL,
                target_id TEXT NOT NULL,
                target_resolved INTEGER NOT NULL DEFAULT 0,
                note TEXT,
                waived INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (source_key, ordinal)
            );
            CREATE TABLE IF NOT EXISTS wiki_groundings (
                entity_key TEXT NOT NULL,
                ordinal INTEGER NOT NULL,
                node_id TEXT NOT NULL,
                health TEXT,
                state TEXT,
                origin TEXT NOT NULL DEFAULT 'frontmatter',
                body_hash TEXT,
                fingerprint TEXT,
                PRIMARY KEY (entity_key, ordinal)
            );
            CREATE TABLE IF NOT EXISTS wiki_sources (
                entity_key TEXT NOT NULL,
                ordinal INTEGER NOT NULL,
                type TEXT NOT NULL,
                ref TEXT,
                note TEXT,
                repository TEXT,
                commit_sha TEXT,
                captured_at TEXT,
                identity TEXT NOT NULL,
                PRIMARY KEY (entity_key, ordinal)
            );
            CREATE TABLE IF NOT EXISTS wiki_topics (
                entity_key TEXT NOT NULL,
                ordinal INTEGER NOT NULL,
                topic_ref TEXT NOT NULL,
                topic_id TEXT,
                PRIMARY KEY (entity_key, ordinal)
            );
            CREATE TABLE IF NOT EXISTS wiki_diagnostics (
                file TEXT NOT NULL,
                code TEXT NOT NULL,
                message TEXT NOT NULL,
                line INTEGER,
                severity TEXT NOT NULL DEFAULT 'error',
                entity_id TEXT,
                path TEXT
            );
            CREATE TABLE IF NOT EXISTS wiki_revision_marks (
                id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                content_hash TEXT NOT NULL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS wiki_fts USING fts5(
                entity_key UNINDEXED,
                id,
                title,
                summary,
                body,
                type,
                aliases
            );
            CREATE TRIGGER IF NOT EXISTS wiki_entities_ai AFTER INSERT ON wiki_entities BEGIN
                INSERT INTO wiki_fts(entity_key, id, title, summary, body, type, aliases)
                VALUES (NEW.entity_key, NEW.id, NEW.title, NEW.summary, NEW.body, NEW.type, NEW.aliases);
            END;
            CREATE TRIGGER IF NOT EXISTS wiki_entities_ad AFTER DELETE ON wiki_entities BEGIN
                DELETE FROM wiki_fts WHERE entity_key = OLD.entity_key;
            END;
            CREATE INDEX IF NOT EXISTS idx_wiki_entities_id ON wiki_entities(id);
            CREATE INDEX IF NOT EXISTS idx_wiki_entities_file ON wiki_entities(file);
            CREATE INDEX IF NOT EXISTS idx_wiki_relations_target ON wiki_relations(target_id);
            CREATE INDEX IF NOT EXISTS idx_wiki_groundings_node ON wiki_groundings(node_id);
            CREATE INDEX IF NOT EXISTS idx_wiki_topics_topic ON wiki_topics(topic_id);
            "#,
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO wiki_meta (key, value) VALUES ('schema_version', ?1)",
            params![WIKI_INDEX_SCHEMA_VERSION],
        )?;
        Ok(())
    }

    /// Scaffold root the index was last built from.
    pub fn scaffold_root(&self) -> Option<PathBuf> {
        self.conn
            .query_row(
                "SELECT value FROM wiki_meta WHERE key = 'scaffold_root'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
            .map(PathBuf::from)
    }

    pub fn is_built(&self) -> bool {
        self.conn
            .query_row(
                "SELECT 1 FROM wiki_meta WHERE key = 'last_refresh'",
                [],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    fn graph_db(&self) -> PathBuf {
        self.db_path.with_file_name("graph.db")
    }

    /// Full rebuild: drop every row and re-read the whole corpus. Returns the entity count.
    /// When discovery hits a corpus safety bound the rebuild is aborted and the existing rows
    /// are kept (see [`WikiIndex::refresh`]).
    pub fn rebuild(&mut self, scaffold_root: &Path) -> Result<usize> {
        let stats = self.refresh_inner(scaffold_root, true, &MaintenanceContext::default())?;
        Ok(stats.entities)
    }

    /// [`WikiIndex::rebuild`] / [`WikiIndex::refresh`] under a maintenance context (abort flag
    /// and progress). An abort, or a Markdown change while it runs, rolls everything back and
    /// fails with `OPERATION_INTERRUPTED`.
    pub fn refresh_with(
        &mut self,
        scaffold_root: &Path,
        full: bool,
        ctx: &MaintenanceContext,
    ) -> Result<RefreshStats> {
        self.refresh_inner(scaffold_root, full, ctx)
    }

    /// Digest of what the index holds: every file path with its content hash, plus the config
    /// digest it was built under. Changes exactly when a refresh changed the indexed corpus.
    pub fn indexed_revision(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"knobyte-wiki-index-v1\0");
        h.update(WIKI_INDEX_SCHEMA_VERSION.as_bytes());
        h.update(b"\0");
        h.update(self.meta("config_digest").unwrap_or_default().as_bytes());
        h.update(b"\0");
        let mut stmt = self
            .conn
            .prepare("SELECT path, content_hash FROM wiki_files ORDER BY path")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (p, c) = row?;
            h.update(format!("{}\0{}\0", p, c).as_bytes());
        }
        Ok(hex::encode(h.finalize()))
    }

    /// One `wiki_meta` value.
    pub fn meta_value(&self, key: &str) -> Option<String> {
        self.meta(key)
    }

    /// Diagnostics recorded at index time (parse and discovery), most severe first.
    pub fn stored_diagnostics(&self, limit: usize) -> Result<Vec<WikiDiagnostic>> {
        let mut stmt = self.conn.prepare(
            "SELECT file, code, message, line, severity, entity_id, path FROM wiki_diagnostics
             ORDER BY CASE severity WHEN 'error' THEN 0 WHEN 'warning' THEN 1 ELSE 2 END, file, line, code
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            let mut d = crate::wiki::diagnostics::diag(
                &r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(0)?,
            );
            d.line = r.get::<_, Option<i64>>(3)?.map(|l| l as usize);
            d.severity = r.get(4)?;
            d.entity_id = r.get(5)?;
            d.path = r.get(6)?;
            Ok(d)
        })?;
        rows.collect()
    }

    /// Incremental refresh: only files whose content hash changed are re-parsed; deleted files
    /// are dropped. Derived state (shadowing, relation resolution, topics, health) is recomputed.
    ///
    /// The refresh key is the file content hash plus a digest of the parse-affecting config
    /// (`wiki.entityTypes`, `wiki.exclude`, parser revision): a config change re-reads every
    /// file. When discovery stops at a corpus safety bound, nothing is removed or re-read; the
    /// `WIKI_CORPUS_LIMIT_EXCEEDED` diagnostics are recorded and the refresh fails with that code.
    pub fn refresh(&mut self, scaffold_root: &Path) -> Result<RefreshStats> {
        self.refresh_inner(scaffold_root, false, &MaintenanceContext::default())
    }

    fn meta(&self, key: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT value FROM wiki_meta WHERE key = ?1",
                params![key],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Compare the index's recorded corpus (per-file content hashes and config digest) with
    /// the Markdown on disk. Cheap enough for a health check: no parsing.
    pub fn freshness(&self, scaffold_root: &Path) -> Result<IndexFreshness> {
        let scope = WikiScope::load(scaffold_root);
        let discovery = scope.discover_checked();
        let known: HashMap<String, String> = {
            let mut stmt = self.conn.prepare("SELECT path, content_hash FROM wiki_files")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<HashMap<_, _>>>()?
        };
        let mut out = IndexFreshness {
            built: self.is_built(),
            config_changed: self.meta("config_digest").as_deref()
                != Some(scope.config_digest().as_str()),
            limit_exceeded: discovery.limit_exceeded,
            ..Default::default()
        };
        let mut present = HashSet::new();
        for (rel, abs) in &discovery.files {
            present.insert(rel.as_str());
            let Ok(bytes) = fs::read(abs) else { continue };
            match known.get(rel) {
                None => out.added.push(rel.clone()),
                Some(h) if *h != compute_file_hash(&bytes) => out.changed.push(rel.clone()),
                _ => {}
            }
        }
        if !discovery.limit_exceeded {
            out.removed = known
                .keys()
                .filter(|k| !present.contains(k.as_str()))
                .cloned()
                .collect();
            out.removed.sort();
        }
        out.stale = !out.built
            || out.config_changed
            || !out.added.is_empty()
            || !out.changed.is_empty()
            || !out.removed.is_empty();
        Ok(out)
    }

    fn refresh_inner(
        &mut self,
        scaffold_root: &Path,
        full: bool,
        ctx: &MaintenanceContext,
    ) -> Result<RefreshStats> {
        // One maintainer at a time; the lease dies with this call (or the process).
        let _lease = crate::wiki::maintenance::acquire_lease(
            &self.db_path,
            crate::wiki::maintenance::LEASE_WAIT,
        )?;
        let scope = WikiScope::load(scaffold_root);
        ctx.boundary("discover", 0, 0)?;
        let observed = crate::wiki::maintenance::observe_corpus(&scope);
        let discovery = scope.discover_checked();
        let config_digest = scope.config_digest();
        let mut stats = RefreshStats::default();

        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM wiki_diagnostics WHERE (code IN ('WIKI_CORPUS_LIMIT_EXCEEDED', 'PATH_OUTSIDE_SCAFFOLD') OR message LIKE 'Broken symlink at %') AND entity_id IS NULL AND line IS NULL",
            [],
        )?;
        for d in &discovery.diagnostics {
            insert_diag(&tx, d)?;
        }
        if discovery.limit_exceeded {
            // A bounded walk is not the corpus: unreached files are not deleted, so abort and
            // keep every existing row; only the limit diagnostics are recorded.
            tx.commit()?;
            let first = discovery
                .diagnostics
                .iter()
                .find(|d| d.code == "WIKI_CORPUS_LIMIT_EXCEEDED")
                .map(|d| d.message.clone())
                .unwrap_or_default();
            return Err(coded_error(
                "WIKI_CORPUS_LIMIT_EXCEEDED",
                format!(
                    "{}. The index was not refreshed; existing entries were kept. Exclude generated or vendored Markdown with `wiki.exclude`.",
                    first
                ),
            ));
        }
        let files = discovery.files;
        ctx.boundary("stage", 0, files.len())?;
        if full {
            tx.execute_batch(
                r#"
                DELETE FROM wiki_entities;
                DELETE FROM wiki_relations;
                DELETE FROM wiki_groundings;
                DELETE FROM wiki_sources;
                DELETE FROM wiki_topics;
                DELETE FROM wiki_files;
                DELETE FROM wiki_diagnostics WHERE code NOT IN ('WIKI_CORPUS_LIMIT_EXCEEDED', 'PATH_OUTSIDE_SCAFFOLD');
                "#,
            )?;
        }
        let recorded_digest: Option<String> = tx
            .query_row(
                "SELECT value FROM wiki_meta WHERE key = 'config_digest'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        // Config that shapes parsing changed: every file is re-read even if its bytes did not.
        let config_changed = recorded_digest.as_deref() != Some(config_digest.as_str());

        let known: HashMap<String, String> = {
            let mut stmt = tx.prepare("SELECT path, content_hash FROM wiki_files")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<Result<HashMap<_, _>>>()?
        };
        let present: HashSet<&str> = files.iter().map(|(rel, _)| rel.as_str()).collect();
        for path in known.keys() {
            if !present.contains(path.as_str()) {
                delete_file_rows(&tx, path)?;
                stats.files_removed += 1;
            }
        }

        for (i, (rel, abs)) in files.iter().enumerate() {
            if i % 64 == 0 {
                ctx.boundary("parse", i, files.len())?;
            }
            let bytes = match fs::read(abs) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let hash = compute_file_hash(&bytes);
            match known.get(rel) {
                Some(h) if *h == hash && !config_changed => {
                    stats.files_unchanged += 1;
                    continue;
                }
                Some(_) => stats.files_updated += 1,
                None => stats.files_added += 1,
            }
            delete_file_rows(&tx, rel)?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            let parsed = parse_markdown_file_with(rel, &text, &scope.registry);
            for d in &parsed.diagnostics {
                insert_diag(&tx, d)?;
            }
            let parse_status = if parsed.diagnostics.iter().any(|d| d.severity == "error") {
                "error"
            } else {
                "ok"
            };
            tx.execute(
                "INSERT OR REPLACE INTO wiki_files (path, content_hash, parse_status, entity_count, text_length, indexed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    rel,
                    hash,
                    parse_status,
                    parsed.entities.len() as i64,
                    text.len() as i64,
                    chrono::Utc::now().to_rfc3339()
                ],
            )?;
            for pe in &parsed.entities {
                insert_entity(&tx, &pe.entity, pe.loc.metadata_start)?;
            }
        }

        ctx.boundary("resolve", files.len(), files.len())?;
        // Duplicate ids: the first claimant (by file, then position) wins.
        tx.execute_batch(
            r#"
            UPDATE wiki_entities SET shadowed = CASE WHEN EXISTS (
                SELECT 1 FROM wiki_entities o
                WHERE o.id = wiki_entities.id
                  AND (o.file < wiki_entities.file OR (o.file = wiki_entities.file AND o.position < wiki_entities.position))
            ) THEN 1 ELSE 0 END;
            UPDATE wiki_relations SET target_resolved = CASE WHEN target_id IN (SELECT id FROM wiki_entities) THEN 1 ELSE 0 END;
            "#,
        )?;
        // Revision marks: the content hash recorded when an entity's revision was first seen.
        // A later hash change at the same revision is a hand edit (REVISION_DIVERGED).
        tx.execute_batch(
            r#"
            INSERT INTO wiki_revision_marks (id, revision, content_hash)
                SELECT id, revision, content_hash FROM wiki_entities WHERE shadowed = 0
            ON CONFLICT(id) DO UPDATE SET revision = excluded.revision, content_hash = excluded.content_hash
                WHERE wiki_revision_marks.revision != excluded.revision;
            "#,
        )?;
        resolve_topics(&tx)?;
        ctx.boundary("validate", files.len(), files.len())?;
        let graph_db = self.db_path.with_file_name("graph.db");
        compute_health(&tx, &graph_db)?;

        let now = chrono::Utc::now().to_rfc3339();
        for (k, v) in [
            ("last_refresh", now.clone()),
            ("last_rebuild", now),
            ("scaffold_root", scaffold_root.to_string_lossy().to_string()),
            ("config_digest", config_digest),
        ] {
            tx.execute(
                "INSERT OR REPLACE INTO wiki_meta (key, value) VALUES (?1, ?2)",
                params![k, v],
            )?;
        }
        stats.entities = tx.query_row("SELECT COUNT(*) FROM wiki_entities", [], |r| {
            r.get::<_, i64>(0)
        })? as usize;
        // Publish only an index of the corpus that still exists.
        ctx.boundary("publish", files.len(), files.len())?;
        crate::wiki::maintenance::assert_corpus_unchanged(&observed, &scope, "publish")?;
        tx.commit()?;
        Ok(stats)
    }

    // ------------------------------------------------------------------
    // Reads
    // ------------------------------------------------------------------

    fn entities_where(&self, clause: &str, p: &[&dyn rusqlite::ToSql]) -> Result<Vec<WikiEntity>> {
        let sql = format!(
            "SELECT {} FROM wiki_entities e WHERE {} ORDER BY e.type, e.title, e.id",
            ENTITY_COLUMNS, clause
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(p, map_entity)?;
        rows.collect()
    }

    /// Number of visible (non-shadowed) entities, without loading them.
    pub fn entity_count(&self) -> Result<usize> {
        self.conn
            .query_row(
                "SELECT COUNT(*) FROM wiki_entities WHERE shadowed = 0",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as usize)
    }

    /// Every indexed entity (including shadowed and archived), ordered by type and title.
    pub fn list(&self) -> Result<Vec<WikiEntity>> {
        self.entities_where("1 = 1", &[])
    }

    fn topic_memberships(&self) -> Result<HashMap<String, Vec<String>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT entity_key, topic_id FROM wiki_topics WHERE topic_id IS NOT NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for r in rows {
            let (k, t) = r?;
            out.entry(k).or_default().push(t);
        }
        Ok(out)
    }

    /// Resolve a topic reference (id, title or alias) to a topic id.
    pub fn resolve_topic(&self, reference: &str) -> Result<Option<String>> {
        let topics = self.entities_where("e.type = 'topic' AND e.shadowed = 0", &[])?;
        if let Some(t) = topics.iter().find(|t| t.id == reference) {
            return Ok(Some(t.id.clone()));
        }
        let wanted = normalize_name(reference);
        let matches: Vec<&WikiEntity> = topics
            .iter()
            .filter(|t| {
                normalize_name(&t.title) == wanted
                    || t.aliases.iter().any(|a| normalize_name(a) == wanted)
            })
            .collect();
        Ok(if matches.len() == 1 {
            Some(matches[0].id.clone())
        } else {
            None
        })
    }

    /// Filtered, ordered listing (archived hidden unless asked for).
    pub fn list_filtered(&self, filter: &QueryFilter) -> Result<Page<EntitySummary>> {
        let limit = clamp_limit(filter.limit);
        let topic_id = match &filter.topic {
            Some(t) => self.resolve_topic(t)?,
            None => None,
        };
        let topics_of = self.topic_memberships()?;
        let mut all: Vec<WikiEntity> = self
            .entities_where("e.shadowed = 0", &[])?
            .into_iter()
            .filter(|e| filter.visible(e, topic_id.as_deref(), &topics_of))
            .collect();
        all.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id)));
        let truncated = all.len() > filter.offset.saturating_add(limit);
        Ok(Page {
            items: all
                .iter()
                .skip(filter.offset)
                .take(limit)
                .map(EntitySummary::from)
                .collect(),
            truncated,
        })
    }

    /// Ranked search: exact id > title > summary > body/aliases; ties broken by lifecycle,
    /// then grounding health, then title.
    pub fn search(&self, text: &str, filter: &QueryFilter) -> Result<Page<SearchHit>> {
        let limit = clamp_limit(filter.limit);
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(Page {
                items: Vec::new(),
                truncated: false,
            });
        }
        let topic_id = match &filter.topic {
            Some(t) => self.resolve_topic(t)?,
            None => None,
        };
        let topics_of = self.topic_memberships()?;
        let mut hits: Vec<(WikiEntity, &'static str)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut add = |rows: Vec<WikiEntity>,
                       field: &'static str,
                       hits: &mut Vec<(WikiEntity, &'static str)>| {
            for e in rows {
                if seen.contains(&e.id) || !filter.visible(&e, topic_id.as_deref(), &topics_of) {
                    continue;
                }
                seen.insert(e.id.clone());
                hits.push((e, field));
            }
        };
        let exact =
            self.entities_where("e.shadowed = 0 AND lower(e.id) = lower(?1)", &[&trimmed])?;
        add(exact, "id", &mut hits);

        let terms: Vec<String> = trimmed
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|t| !t.is_empty())
            .map(|t| format!("\"{}\"*", t.replace('"', "")))
            .collect();
        if !terms.is_empty() {
            for joiner in [" AND ", " OR "] {
                let expr = terms.join(joiner);
                for (cols, field) in [
                    ("title", "title"),
                    ("summary", "summary"),
                    ("body aliases id type", "body"),
                ] {
                    let q = format!("{{{}}} : ({})", cols, expr);
                    let sql = format!(
                        "SELECT {} FROM wiki_fts f JOIN wiki_entities e ON e.entity_key = f.entity_key WHERE wiki_fts MATCH ?1 AND e.shadowed = 0 ORDER BY rank LIMIT ?2",
                        ENTITY_COLUMNS
                    );
                    let mut stmt = self.conn.prepare(&sql)?;
                    let rows = stmt
                        .query_map(
                            params![q, (filter.offset.saturating_add(limit) * 4) as i64],
                            map_entity,
                        )?
                        .collect::<Result<Vec<_>>>()?;
                    add(rows, field, &mut hits);
                }
                // Fall back to OR only when every term together matched nothing.
                if hits.iter().any(|(_, f)| *f != "id") {
                    break;
                }
            }
        }
        hits.sort_by(|(a, fa), (b, fb)| {
            field_rank(fa)
                .cmp(&field_rank(fb))
                .then_with(|| lifecycle_rank(&a.status).cmp(&lifecycle_rank(&b.status)))
                .then_with(|| {
                    health_rank(a.health.as_deref()).cmp(&health_rank(b.health.as_deref()))
                })
                .then_with(|| a.title.cmp(&b.title))
                .then_with(|| a.id.cmp(&b.id))
        });
        let truncated = hits.len() > filter.offset.saturating_add(limit);
        Ok(Page {
            items: hits
                .iter()
                .skip(filter.offset)
                .take(limit)
                .map(|(e, f)| SearchHit {
                    entity: EntitySummary::from(e),
                    matched: f.to_string(),
                })
                .collect(),
            truncated,
        })
    }

    /// Back-compat search returning full entities (relations and groundings loaded).
    /// An empty `text` returns the first page of the bounded, archived-hidden listing (never
    /// every entity).
    pub fn query(&self, text: &str) -> Result<Vec<WikiEntity>> {
        if text.trim().is_empty() {
            let page = self.list_filtered(&QueryFilter {
                limit: Some(DEFAULT_RESULT_LIMIT),
                ..Default::default()
            })?;
            let mut out = Vec::new();
            for s in page.items {
                if let Some(e) = self.show(&s.id)? {
                    out.push(e);
                }
            }
            return Ok(out);
        }
        let page = self.search(
            text,
            &QueryFilter {
                limit: Some(DEFAULT_RESULT_LIMIT),
                ..Default::default()
            },
        )?;
        let mut out = Vec::new();
        for hit in page.items {
            if let Some(e) = self.show(&hit.entity.id)? {
                out.push(e);
            }
        }
        Ok(out)
    }

    /// One entity by id (or entity key), with relations, groundings, sources and topics.
    pub fn show(&self, id: &str) -> Result<Option<WikiEntity>> {
        let sql = format!(
            "SELECT {} FROM wiki_entities e WHERE e.id = ?1 OR e.entity_key = ?1 ORDER BY e.shadowed, e.file, e.position LIMIT 1",
            ENTITY_COLUMNS
        );
        let entity = self
            .conn
            .query_row(&sql, params![id], map_entity)
            .optional()?;
        match entity {
            Some(mut e) => {
                self.load_details(&mut e)?;
                Ok(Some(e))
            }
            None => Ok(None),
        }
    }

    pub fn summary(&self, id: &str) -> Result<Option<EntitySummary>> {
        let sql = format!(
            "SELECT {} FROM wiki_entities e WHERE e.id = ?1 AND e.shadowed = 0 LIMIT 1",
            ENTITY_COLUMNS
        );
        Ok(self
            .conn
            .query_row(&sql, params![id], map_entity)
            .optional()?
            .map(|e| EntitySummary::from(&e)))
    }

    fn summaries_by_id(&self, ids: &[String]) -> Result<HashMap<String, EntitySummary>> {
        let mut out = HashMap::new();
        for id in ids {
            if out.contains_key(id) {
                continue;
            }
            if let Some(s) = self.summary(id)? {
                out.insert(id.clone(), s);
            }
        }
        Ok(out)
    }

    fn edges_from(&self, id: &str) -> Result<Vec<RelationEdge>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.type, r.target_id FROM wiki_relations r JOIN wiki_entities e ON e.entity_key = r.source_key WHERE e.id = ?1 AND e.shadowed = 0 ORDER BY r.type, r.target_id LIMIT ?2",
        )?;
        let raw: Vec<(String, String)> = stmt
            .query_map(params![id, MAX_EDGES_PER_ENTITY as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<_>>()?;
        self.to_edges(raw)
    }

    fn edges_to(&self, id: &str) -> Result<Vec<RelationEdge>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.type, e.id FROM wiki_relations r JOIN wiki_entities e ON e.entity_key = r.source_key WHERE r.target_id = ?1 AND e.shadowed = 0 ORDER BY r.type, e.id LIMIT ?2",
        )?;
        let raw: Vec<(String, String)> = stmt
            .query_map(params![id, MAX_EDGES_PER_ENTITY as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<_>>()?;
        self.to_edges(raw)
    }

    fn to_edges(&self, raw: Vec<(String, String)>) -> Result<Vec<RelationEdge>> {
        let ids: Vec<String> = raw.iter().map(|(_, t)| t.clone()).collect();
        let by_id = self.summaries_by_id(&ids)?;
        let mut edges: Vec<RelationEdge> = raw
            .into_iter()
            .map(|(t, id)| RelationEdge {
                rel_type: t,
                target: by_id.get(&id).cloned(),
                resolved: by_id.contains_key(&id),
                target_id: id,
            })
            .collect();
        edges.sort_by(|a, b| {
            a.rel_type
                .cmp(&b.rel_type)
                .then_with(|| {
                    a.target
                        .as_ref()
                        .map(|t| t.title.clone())
                        .cmp(&b.target.as_ref().map(|t| t.title.clone()))
                })
                .then_with(|| a.target_id.cmp(&b.target_id))
        });
        Ok(edges)
    }

    /// Bounded neighbourhood: direct relations and backlinks, plus entities reached within
    /// `depth` hops (both directions), cut to `limit` entries and `max_tokens`.
    pub fn neighborhood(
        &self,
        id: &str,
        depth: Option<usize>,
        max_tokens: Option<usize>,
        limit: Option<usize>,
        include_archived: bool,
    ) -> Result<Option<Neighborhood>> {
        let Some(origin) = self.summary(id)? else {
            return Ok(None);
        };
        let depth = clamp_depth(depth);
        let limit = clamp_limit(limit);
        let max_tokens = max_tokens.unwrap_or(DEFAULT_NEIGHBORHOOD_TOKENS).max(1);
        let visible = |s: &EntitySummary| include_archived || s.status != "archived";
        let relations = self.edges_from(&origin.id)?;
        let backlinks = self.edges_to(&origin.id)?;
        let mut reached: Vec<EntitySummary> = Vec::new();
        let mut visited: HashSet<String> = HashSet::from([origin.id.clone()]);
        let mut frontier = vec![origin.id.clone()];
        let mut truncated = false;
        'outer: for _ in 0..depth {
            let mut next = Vec::new();
            for current in &frontier {
                let mut edges = self.edges_from(current)?;
                edges.extend(self.edges_to(current)?);
                for edge in edges {
                    let Some(t) = edge.target else { continue };
                    if visited.contains(&t.id) || !visible(&t) {
                        continue;
                    }
                    visited.insert(t.id.clone());
                    if reached.len() >= limit {
                        truncated = true;
                        break 'outer;
                    }
                    next.push(t.id.clone());
                    reached.push(t);
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        reached.sort_by(|a, b| a.title.cmp(&b.title).then_with(|| a.id.cmp(&b.id)));
        let mut used =
            estimate_tokens(&origin) + estimate_tokens(&relations) + estimate_tokens(&backlinks);
        let mut fitted = Vec::new();
        for s in reached {
            let cost = estimate_tokens(&s);
            if used + cost > max_tokens {
                truncated = true;
                break;
            }
            used += cost;
            fitted.push(s);
        }
        Ok(Some(Neighborhood {
            origin,
            relations,
            backlinks,
            reached: fitted,
            truncated,
            estimated_tokens: used,
        }))
    }

    /// Entities directly related to `id`, in either direction (back-compat).
    pub fn related(&self, id: &str) -> Result<Vec<WikiEntity>> {
        let mut ids: Vec<String> = Vec::new();
        for e in self.edges_from(id)?.into_iter().chain(self.edges_to(id)?) {
            if e.resolved && !ids.contains(&e.target_id) {
                ids.push(e.target_id);
            }
        }
        let mut out = Vec::new();
        for i in ids {
            if let Some(e) = self.show(&i)? {
                out.push(e);
            }
        }
        Ok(out)
    }

    /// Bounded, paged backlinks: distinct visible entities with a relation to `id`, by id.
    pub fn backlinks_page(&self, id: &str, limit: Option<usize>, offset: usize) -> Result<LinkPage> {
        self.link_page(
            "SELECT DISTINCT e.id AS id FROM wiki_relations r JOIN wiki_entities e ON e.entity_key = r.source_key WHERE r.target_id = ?1 AND e.shadowed = 0 AND e.id != ?1",
            id,
            limit,
            offset,
        )
    }

    /// Bounded, paged related entities: distinct visible entities this one relates to or that
    /// relate to it (resolved targets only), by id.
    pub fn related_page(&self, id: &str, limit: Option<usize>, offset: usize) -> Result<LinkPage> {
        self.link_page(
            "SELECT x.id AS id FROM (
                SELECT r.target_id AS id FROM wiki_relations r JOIN wiki_entities s ON s.entity_key = r.source_key WHERE s.id = ?1 AND s.shadowed = 0
                UNION
                SELECT s.id AS id FROM wiki_relations r JOIN wiki_entities s ON s.entity_key = r.source_key WHERE r.target_id = ?1 AND s.shadowed = 0
             ) x WHERE x.id != ?1 AND EXISTS (SELECT 1 FROM wiki_entities v WHERE v.id = x.id AND v.shadowed = 0)",
            id,
            limit,
            offset,
        )
    }

    fn link_page(&self, ids_sql: &str, id: &str, limit: Option<usize>, offset: usize) -> Result<LinkPage> {
        let limit = limit.unwrap_or(DEFAULT_LINK_PAGE).clamp(1, MAX_LINK_PAGE);
        let total: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM ({})", ids_sql),
            params![id],
            |r| r.get(0),
        )?;
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT id FROM ({}) ORDER BY id LIMIT ?2 OFFSET ?3", ids_sql))?;
        let ids: Vec<String> = stmt
            .query_map(params![id, limit as i64, offset as i64], |r| r.get(0))?
            .collect::<Result<_>>()?;
        let mut items = Vec::with_capacity(ids.len());
        for i in &ids {
            if let Some(s) = self.summary(i)? {
                items.push(s);
            }
        }
        let total = total as usize;
        let truncated = offset + ids.len() < total;
        Ok(LinkPage {
            next_offset: truncated.then_some(offset + ids.len()),
            items,
            total,
            truncated,
            limit,
            offset,
        })
    }

    /// One entity with bounded relation lists: the outgoing `relations`, `backlinks` and
    /// `related` are each paged (`limit` default 25, max 200) and report `total`,
    /// `truncated` and `nextOffset`; the body is included only with `include_body`. This is
    /// the bounded detail read for adapters (CLI `wiki show`, MCP `knobyte_wiki_get`, Hub).
    pub fn entity_detail(&self, id: &str, opts: &DetailOptions) -> Result<Option<EntityDetail>> {
        let Some(mut entity) = self.show(id)? else {
            return Ok(None);
        };
        let limit = opts.limit.unwrap_or(DEFAULT_LINK_PAGE).clamp(1, MAX_LINK_PAGE);
        let rel_total = entity.relations.len();
        let rel_offset = opts.relations_offset.min(rel_total);
        entity.relations = std::mem::take(&mut entity.relations)
            .into_iter()
            .skip(rel_offset)
            .take(limit)
            .collect();
        let rel_truncated = rel_offset + entity.relations.len() < rel_total;
        if !opts.include_body {
            entity.body = String::new();
        }
        let backlinks = self.backlinks_page(&entity.id, Some(limit), opts.backlinks_offset)?;
        let related = if opts.include_related {
            Some(self.related_page(&entity.id, Some(limit), opts.related_offset)?)
        } else {
            None
        };
        Ok(Some(EntityDetail {
            relations_page: LinkCount {
                total: rel_total,
                truncated: rel_truncated,
                limit,
                offset: rel_offset,
                next_offset: rel_truncated.then_some(rel_offset + entity.relations.len()),
            },
            entity,
            body_included: opts.include_body,
            backlinks,
            related,
        }))
    }

    pub fn backlinks(&self, id: &str) -> Result<Vec<WikiEntity>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for e in self.edges_to(id)? {
            if seen.insert(e.target_id.clone()) {
                if let Some(x) = self.show(&e.target_id)? {
                    out.push(x);
                }
            }
        }
        Ok(out)
    }

    /// A bounded slice of the entity graph: everything reachable from `seeds` within `depth`
    /// hops, or (without seeds) the first `limit` visible entities, plus the edges among them.
    pub fn graph_slice(
        &self,
        seeds: &[String],
        depth: Option<usize>,
        limit: Option<usize>,
        include_archived: bool,
    ) -> Result<GraphSlice> {
        let limit = clamp_limit(limit);
        let depth = clamp_depth(depth);
        let mut nodes: BTreeMap<String, EntitySummary> = BTreeMap::new();
        let mut truncated = false;
        if seeds.is_empty() {
            let page = self.list_filtered(&QueryFilter {
                include_archived,
                limit: Some(limit),
                ..Default::default()
            })?;
            truncated = page.truncated;
            for s in page.items {
                nodes.insert(s.id.clone(), s);
            }
        } else {
            let mut queue: VecDeque<(String, usize)> = VecDeque::new();
            for s in seeds {
                if let Some(sum) = self.summary(s)? {
                    queue.push_back((sum.id.clone(), 0));
                    nodes.insert(sum.id.clone(), sum);
                }
            }
            while let Some((id, d)) = queue.pop_front() {
                if d >= depth {
                    continue;
                }
                let mut edges = self.edges_from(&id)?;
                edges.extend(self.edges_to(&id)?);
                for e in edges {
                    let Some(t) = e.target else { continue };
                    if nodes.contains_key(&t.id) || (!include_archived && t.status == "archived") {
                        continue;
                    }
                    if nodes.len() >= limit {
                        truncated = true;
                        break;
                    }
                    queue.push_back((t.id.clone(), d + 1));
                    nodes.insert(t.id.clone(), t);
                }
            }
        }
        let mut edges = Vec::new();
        let mut seen = HashSet::new();
        for id in nodes.keys() {
            for e in self.edges_from(id)? {
                if nodes.contains_key(&e.target_id)
                    && seen.insert((id.clone(), e.target_id.clone(), e.rel_type.clone()))
                {
                    edges.push(GraphEdge {
                        source: id.clone(),
                        target: e.target_id,
                        rel_type: e.rel_type,
                    });
                }
            }
        }
        Ok(GraphSlice {
            nodes: nodes.into_values().collect(),
            edges,
            truncated,
        })
    }

    /// References equivalent to `reference` (same resolved code symbol), including itself.
    ///
    /// Only groundings that could name the resolved node are resolved: its id, or readable
    /// references mentioning both its file path and its name. Resolving every grounding in
    /// the wiki per query reference does not scale.
    fn equivalent_refs(&self, graph: Option<&Connection>, reference: &str) -> Result<Vec<String>> {
        let mut refs: Vec<String> = vec![reference.to_string()];
        let Some(gc) = graph else {
            return Ok(refs);
        };
        if let Ok(RefResolution::Resolved(target)) = resolve_grounding_ref(gc, reference) {
            if !refs.contains(&target.id) {
                refs.push(target.id.clone());
            }
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT node_id FROM wiki_groundings WHERE node_id = ?1 OR (instr(node_id, ?2) > 0 AND instr(node_id, ?3) > 0) LIMIT 1000",
            )?;
            let candidates: Vec<String> = stmt
                .query_map(params![target.id, target.file_path, target.name], |r| r.get(0))?
                .collect::<Result<_>>()?;
            for g in candidates {
                if refs.contains(&g) {
                    continue;
                }
                if let Ok(RefResolution::Resolved(n)) = resolve_grounding_ref(gc, &g) {
                    if n.id == target.id {
                        refs.push(g);
                    }
                }
            }
        }
        Ok(refs)
    }

    /// Entities grounded in any of `references` (graph ids or readable refs), archived hidden,
    /// at most `limit` entities. See [`WikiIndex::for_code_filtered`].
    pub fn for_code_many(
        &self,
        references: &[String],
        limit: Option<usize>,
    ) -> Result<Page<GroundedHit>> {
        self.for_code_filtered(references, limit, false)
    }

    /// One hit per entity grounded in any of `references`, with `matchedNodes` (the queried
    /// references it grounds to, in query order). Ranked by overlap count (descending), then
    /// lifecycle, then grounding health, then title and id. Archived entities are hidden
    /// unless `include_archived`.
    pub fn for_code_filtered(
        &self,
        references: &[String],
        limit: Option<usize>,
        include_archived: bool,
    ) -> Result<Page<GroundedHit>> {
        let limit = clamp_limit(limit);
        let mut queries: Vec<String> = Vec::new();
        for q in references {
            let q = q.trim();
            if !q.is_empty() && !queries.iter().any(|x| x == q) {
                queries.push(q.to_string());
            }
        }
        if queries.is_empty() {
            return Ok(Page {
                items: Vec::new(),
                truncated: false,
            });
        }
        let graph_db = self.graph_db();
        let graph = if graph_db.exists() {
            Connection::open_with_flags(&graph_db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
        } else {
            None
        };
        // grounding reference -> indexes of the queries it answers
        let mut answers: HashMap<String, Vec<usize>> = HashMap::new();
        for (qi, q) in queries.iter().enumerate() {
            for r in self.equivalent_refs(graph.as_ref(), q)? {
                let v = answers.entry(r).or_default();
                if !v.contains(&qi) {
                    v.push(qi);
                }
            }
        }
        let all_refs: Vec<&String> = answers.keys().collect();
        let refs_json = serde_json::to_string(&all_refs).unwrap_or_else(|_| "[]".to_string());
        let archived_clause = if include_archived {
            ""
        } else {
            " AND e.status != 'archived'"
        };
        let sql = format!(
            "SELECT {}, g.node_id, g.health FROM wiki_entities e JOIN wiki_groundings g ON e.entity_key = g.entity_key WHERE e.shadowed = 0{} AND g.node_id IN (SELECT value FROM json_each(?1)) ORDER BY e.entity_key, g.ordinal LIMIT ?2",
            ENTITY_COLUMNS, archived_clause
        );
        let row_cap = (limit.saturating_add(1)).saturating_mul(MAX_EDGES_PER_ENTITY) as i64;
        struct Acc {
            entity: WikiEntity,
            matched: Vec<usize>,
            grounding: String,
            first_q: usize,
            health: Option<String>,
        }
        let mut by_key: BTreeMap<String, Acc> = BTreeMap::new();
        {
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params![refs_json, row_cap], |r| {
                Ok((
                    map_entity(r)?,
                    r.get::<_, String>(18)?,
                    r.get::<_, Option<String>>(19)?,
                ))
            })?;
            for row in rows {
                let (e, g, h) = row?;
                let qis = answers.get(&g).cloned().unwrap_or_default();
                let Some(&min_q) = qis.iter().min() else {
                    continue;
                };
                let acc = by_key.entry(e.entity_key.clone()).or_insert_with(|| Acc {
                    entity: e,
                    matched: Vec::new(),
                    grounding: g.clone(),
                    first_q: min_q,
                    health: h.clone(),
                });
                if min_q < acc.first_q {
                    acc.first_q = min_q;
                    acc.grounding = g.clone();
                }
                for qi in qis {
                    if !acc.matched.contains(&qi) {
                        acc.matched.push(qi);
                    }
                }
                if health_rank(h.as_deref()) > health_rank(acc.health.as_deref()) {
                    acc.health = h;
                }
            }
        }
        let mut hits: Vec<Acc> = by_key.into_values().collect();
        for h in hits.iter_mut() {
            h.matched.sort();
        }
        hits.sort_by(|a, b| {
            b.matched
                .len()
                .cmp(&a.matched.len())
                .then_with(|| {
                    lifecycle_rank(&a.entity.status).cmp(&lifecycle_rank(&b.entity.status))
                })
                .then_with(|| {
                    health_rank(a.health.as_deref()).cmp(&health_rank(b.health.as_deref()))
                })
                .then_with(|| a.entity.title.cmp(&b.entity.title))
                .then_with(|| a.entity.id.cmp(&b.entity.id))
        });
        let truncated = hits.len() > limit;
        Ok(Page {
            items: hits
                .into_iter()
                .take(limit)
                .map(|a| GroundedHit {
                    entity: EntitySummary::from(&a.entity),
                    query: queries[a.first_q].clone(),
                    matched_nodes: a.matched.iter().map(|&i| queries[i].clone()).collect(),
                    grounding: a.grounding,
                    grounding_health: a.health,
                })
                .collect(),
            truncated,
        })
    }

    /// Entities grounded to a code symbol (back-compat single-reference form).
    pub fn for_code(&self, node_id: &str) -> Result<Vec<WikiEntity>> {
        let page = self.for_code_many(&[node_id.to_string()], Some(MAX_RESULT_LIMIT))?;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for h in page.items {
            if seen.insert(h.entity.id.clone()) {
                if let Some(e) = self.show(&h.entity.id)? {
                    out.push(e);
                }
            }
        }
        Ok(out)
    }

    /// Per-grounding health of one entity: `(reference, health, state)`.
    pub fn groundings_for(&self, id: &str) -> Result<Vec<GroundingRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.node_id, g.health, g.state FROM wiki_groundings g JOIN wiki_entities e ON e.entity_key = g.entity_key WHERE e.id = ?1 AND e.shadowed = 0 ORDER BY g.ordinal",
        )?;
        let rows = stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// Recorded content hash and revision of an entity at index time.
    pub fn recorded_state(&self, id: &str) -> Result<Option<(i64, String, String)>> {
        self.conn
            .query_row(
                "SELECT revision, content_hash, file FROM wiki_entities WHERE id = ?1 AND shadowed = 0 LIMIT 1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
    }

    /// `(revision, content hash)` recorded when the entity's current revision was first indexed.
    pub fn revision_mark(&self, id: &str) -> Result<Option<(i64, String)>> {
        self.conn
            .query_row(
                "SELECT revision, content_hash FROM wiki_revision_marks WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    /// Validate the scaffold this index was built from (see [`crate::wiki::validate`]). The code
    /// graph is looked up next to the wiki database.
    pub fn validate(&self) -> Result<Vec<WikiDiagnostic>> {
        let graph_db = self.graph_db();
        self.validate_with_graph(Some(graph_db.as_path()))
    }

    /// Like [`WikiIndex::validate`] with an explicit code graph (None skips grounding checks
    /// and reports them as unchecked).
    pub fn validate_with_graph(&self, graph_db: Option<&Path>) -> Result<Vec<WikiDiagnostic>> {
        let Some(root) = self.scaffold_root() else {
            return Ok(vec![crate::wiki::diagnostics::diag(
                "WIKI_INDEX_MISSING",
                "The wiki index has not been built",
                "wiki.db",
            )]);
        };
        let scope = WikiScope::load(&root);
        let project_root = root
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| root.clone());
        let report =
            crate::wiki::validate::validate_scaffold(&crate::wiki::validate::ValidateOptions {
                scope: &scope,
                project_root: &project_root,
                graph_db,
                index: Some(self),
                limit: None,
            });
        Ok(report.diagnostics)
    }

    fn load_details(&self, entity: &mut WikiEntity) -> Result<()> {
        let mut rel_stmt = self.conn.prepare(
            "SELECT type, target_id, note, waived FROM wiki_relations WHERE source_key = ?1 ORDER BY ordinal",
        )?;
        let rels = rel_stmt.query_map(params![entity.entity_key], |r| {
            Ok(EntityRelation {
                rel_type: r.get(0)?,
                target_id: r.get(1)?,
                note: r.get(2)?,
                waived: r.get::<_, i64>(3)? != 0,
                ..Default::default()
            })
        })?;
        entity.relations = rels.collect::<Result<_>>()?;
        let mut g_stmt = self.conn.prepare(
            "SELECT node_id, origin, body_hash, fingerprint FROM wiki_groundings WHERE entity_key = ?1 ORDER BY ordinal",
        )?;
        entity.committed_groundings = g_stmt
            .query_map(params![entity.entity_key], |r| {
                Ok(CommittedGrounding {
                    reference: r.get(0)?,
                    origin: r.get(1)?,
                    body_hash: r.get(2)?,
                    fingerprint: r.get(3)?,
                })
            })?
            .collect::<Result<_>>()?;
        entity.grounds_to = entity
            .committed_groundings
            .iter()
            .map(|c| c.reference.clone())
            .collect();
        let mut t_stmt = self
            .conn
            .prepare("SELECT topic_ref FROM wiki_topics WHERE entity_key = ?1 ORDER BY ordinal")?;
        entity.topics = t_stmt
            .query_map(params![entity.entity_key], |r| r.get(0))?
            .collect::<Result<_>>()?;
        let mut s_stmt = self.conn.prepare(
            "SELECT type, ref, note, repository, commit_sha, captured_at FROM wiki_sources WHERE entity_key = ?1 ORDER BY ordinal",
        )?;
        entity.sources = s_stmt
            .query_map(params![entity.entity_key], |r| {
                Ok(WikiSource {
                    source_type: r.get(0)?,
                    reference: r.get(1)?,
                    note: r.get(2)?,
                    repository: r.get(3)?,
                    commit: r.get(4)?,
                    captured_at: r.get(5)?,
                    metadata: None,
                })
            })?
            .collect::<Result<_>>()?;
        Ok(())
    }
}

fn delete_file_rows(tx: &Connection, path: &str) -> Result<()> {
    for table in [
        "wiki_relations",
        "wiki_groundings",
        "wiki_sources",
        "wiki_topics",
    ] {
        let key = if table == "wiki_relations" {
            "source_key"
        } else {
            "entity_key"
        };
        tx.execute(
            &format!(
                "DELETE FROM {} WHERE {} IN (SELECT entity_key FROM wiki_entities WHERE file = ?1)",
                table, key
            ),
            params![path],
        )?;
    }
    tx.execute("DELETE FROM wiki_entities WHERE file = ?1", params![path])?;
    tx.execute(
        "DELETE FROM wiki_diagnostics WHERE file = ?1",
        params![path],
    )?;
    tx.execute("DELETE FROM wiki_files WHERE path = ?1", params![path])?;
    Ok(())
}

fn insert_diag(tx: &Connection, d: &WikiDiagnostic) -> Result<()> {
    tx.execute(
        "INSERT INTO wiki_diagnostics (file, code, message, line, severity, entity_id, path) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            d.file,
            d.code,
            d.message,
            d.line.map(|l| l as i64),
            d.severity,
            d.entity_id,
            d.path
        ],
    )?;
    Ok(())
}

fn insert_entity(tx: &Connection, e: &WikiEntity, position: usize) -> Result<()> {
    let aliases = if e.aliases.is_empty() {
        None
    } else {
        serde_json::to_string(&e.aliases).ok()
    };
    tx.execute(
        r#"INSERT OR REPLACE INTO wiki_entities (
            entity_key, id, shadowed, file, position, type, title, summary, body, status, revision,
            start_line, end_line, heading_depth, content_hash, metadata_kind, health, aliases, provenance, metadata
        ) VALUES (?1, ?2, 0, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, NULL, ?16, ?17, ?18)"#,
        params![
            e.entity_key,
            e.id,
            e.file,
            position as i64,
            e.entity_type,
            e.title,
            e.summary,
            e.body,
            e.status,
            e.revision,
            e.start_line as i64,
            e.end_line as i64,
            e.heading_depth as i64,
            e.content_hash,
            e.metadata_kind,
            aliases,
            e.provenance.as_ref().and_then(|p| serde_json::to_string(p).ok()),
            e.metadata.as_ref().map(|m| m.to_string()),
        ],
    )?;
    for (i, r) in e.relations.iter().enumerate() {
        tx.execute(
            "INSERT OR REPLACE INTO wiki_relations (source_key, ordinal, type, target_id, note, waived) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![e.entity_key, i as i64, r.rel_type, r.target_id, r.note, r.waived as i64],
        )?;
    }
    for (i, g) in e.grounds_to.iter().enumerate() {
        let c = e.committed_for(g);
        tx.execute(
            "INSERT OR REPLACE INTO wiki_groundings (entity_key, ordinal, node_id, origin, body_hash, fingerprint) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                e.entity_key,
                i as i64,
                g,
                c.map(|c| c.origin.as_str()).unwrap_or(GROUNDING_ORIGIN_FRONTMATTER),
                c.and_then(|c| c.body_hash.as_deref()),
                c.and_then(|c| c.fingerprint.as_deref()),
            ],
        )?;
    }
    for (i, s) in e.sources.iter().enumerate() {
        tx.execute(
            "INSERT OR REPLACE INTO wiki_sources (entity_key, ordinal, type, ref, note, repository, commit_sha, captured_at, identity) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                e.entity_key,
                i as i64,
                s.source_type,
                s.reference,
                s.note,
                s.repository,
                s.commit,
                s.captured_at,
                s.identity()
            ],
        )?;
    }
    for (i, t) in e.topics.iter().enumerate() {
        tx.execute(
            "INSERT OR REPLACE INTO wiki_topics (entity_key, ordinal, topic_ref) VALUES (?1, ?2, ?3)",
            params![e.entity_key, i as i64, t],
        )?;
    }
    Ok(())
}

fn resolve_topics(tx: &Connection) -> Result<()> {
    let mut stmt = tx.prepare(
        "SELECT id, title, aliases FROM wiki_entities WHERE type = 'topic' AND shadowed = 0",
    )?;
    let topics: Vec<(String, String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_>>()?;
    let ids: HashSet<String> = topics.iter().map(|t| t.0.clone()).collect();
    let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
    for (id, title, aliases) in &topics {
        let mut names = vec![title.clone()];
        if let Some(a) = aliases
            .as_deref()
            .and_then(|a| serde_json::from_str::<Vec<String>>(a).ok())
        {
            names.extend(a);
        }
        for n in names {
            let k = normalize_name(&n);
            if k.is_empty() {
                continue;
            }
            let v = by_name.entry(k).or_default();
            if !v.contains(id) {
                v.push(id.clone());
            }
        }
    }
    let mut stmt = tx.prepare("SELECT rowid, topic_ref FROM wiki_topics")?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_>>()?;
    for (rowid, reference) in rows {
        let resolved = if ids.contains(&reference) {
            Some(reference.clone())
        } else {
            match by_name.get(&normalize_name(&reference)) {
                Some(v) if v.len() == 1 => Some(v[0].clone()),
                _ => None,
            }
        };
        tx.execute(
            "UPDATE wiki_topics SET topic_id = ?1 WHERE rowid = ?2",
            params![resolved, rowid],
        )?;
    }
    Ok(())
}

/// The grounding `reference` of `e` as drift sees it: its origin and committed baseline.
pub fn doc_ref_of(e: &WikiEntity, reference: &str) -> DocRef {
    let c = e.committed_for(reference);
    doc_ref(
        reference,
        c.map(|c| c.origin.as_str()),
        c.and_then(|c| c.body_hash.clone()),
        c.and_then(|c| c.fingerprint.clone()),
        e.start_line,
    )
}

fn doc_ref(
    reference: &str,
    origin: Option<&str>,
    body_hash: Option<String>,
    fingerprint: Option<String>,
    line: usize,
) -> DocRef {
    DocRef {
        reference: reference.to_string(),
        origin: if origin == Some(GROUNDING_ORIGIN_ANCHOR) {
            RefOrigin::Anchor(line.max(1))
        } else {
            RefOrigin::Frontmatter
        },
        committed: CommittedBaseline {
            body_hash,
            fingerprint,
        },
    }
}

/// Committed `grounds_to` baselines of `entities`, by file (the scaffold-wide index drift uses
/// to resolve an anchor without a baseline of its own).
pub fn committed_index<'a>(entities: impl IntoIterator<Item = &'a WikiEntity>) -> CommittedIndex {
    let mut docs: BTreeMap<String, Vec<DocRef>> = BTreeMap::new();
    for e in entities {
        let refs = docs.entry(e.file.clone()).or_default();
        for g in &e.grounds_to {
            refs.push(doc_ref_of(e, g));
        }
    }
    CommittedIndex::build(docs.iter().map(|(d, r)| (d.as_str(), r.as_slice())))
}

/// Grounding health of one reference in `file`: `(health, state)`. The baseline is resolved
/// like drift's (committed values in the markdown first, the graph.db cache second); a
/// resolved reference without any baseline is `unverified` (drift's GROUNDING_UNVERIFIED).
pub fn grounding_health(
    graph: Option<&Connection>,
    file: &str,
    r: &DocRef,
    committed: &CommittedIndex,
) -> (String, String) {
    let Some(gc) = graph else {
        return ("unverified".into(), "unchecked".into());
    };
    let baseline = resolve_baseline(Some(gc), file, r, committed);
    match resolve_grounding_ref(gc, &r.reference) {
        Ok(RefResolution::Resolved(node)) => match baseline.body_hash.as_deref() {
            Some(b) => {
                let current = node.body_hash.clone().unwrap_or_default();
                if current.is_empty() || current == b {
                    ("fresh".into(), "resolved".into())
                } else {
                    ("changed".into(), "resolved".into())
                }
            }
            None => ("unverified".into(), "resolved".into()),
        },
        Ok(RefResolution::Ambiguous(_)) => ("ambiguous".into(), "ambiguous".into()),
        Ok(RefResolution::Missing) | Err(_) => {
            if baseline.body_hash.is_some() || baseline.minhash.is_some() {
                ("missing".into(), "missing".into())
            } else {
                ("missing".into(), "unresolved".into())
            }
        }
    }
}

fn compute_health(tx: &Connection, graph_db: &Path) -> Result<()> {
    let graph = if graph_db.exists() {
        Connection::open_with_flags(graph_db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
    } else {
        None
    };
    let mut stmt = tx.prepare(
        "SELECT g.rowid, e.file, g.node_id, g.entity_key, g.origin, g.body_hash, g.fingerprint, e.start_line FROM wiki_groundings g JOIN wiki_entities e ON e.entity_key = g.entity_key",
    )?;
    type Row = (i64, String, String, String, DocRef);
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let origin: Option<String> = r.get(4)?;
            let line: i64 = r.get(7)?;
            let node: String = r.get(2)?;
            let dr = doc_ref(&node, origin.as_deref(), r.get(5)?, r.get(6)?, line as usize);
            Ok((r.get(0)?, r.get(1)?, node, r.get(3)?, dr))
        })?
        .collect::<Result<_>>()?;
    let mut docs: BTreeMap<String, Vec<DocRef>> = BTreeMap::new();
    for (_, file, _, _, dr) in &rows {
        docs.entry(file.clone()).or_default().push(dr.clone());
    }
    let committed = CommittedIndex::build(docs.iter().map(|(d, r)| (d.as_str(), r.as_slice())));
    let mut worst: HashMap<String, String> = HashMap::new();
    for (rowid, file, _node, key, dr) in rows {
        let (health, state) = grounding_health(graph.as_ref(), &file, &dr, &committed);
        tx.execute(
            "UPDATE wiki_groundings SET health = ?1, state = ?2 WHERE rowid = ?3",
            params![health, state, rowid],
        )?;
        let entry = worst.entry(key).or_insert_with(|| health.clone());
        if health_rank(Some(&health)) > health_rank(Some(entry)) {
            *entry = health;
        }
    }
    tx.execute("UPDATE wiki_entities SET health = NULL", [])?;
    for (key, h) in worst {
        tx.execute(
            "UPDATE wiki_entities SET health = ?1 WHERE entity_key = ?2",
            params![h, key],
        )?;
    }
    Ok(())
}
