use cozo::{DataValue, MemStorage, NamedRows, ScriptMutability};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::embedding::{
    CodeSymbolText, Embedder, HashedEmbedder, EMBEDDING_DIM, HASHED_EMBEDDER_ID,
};
use super::model2vec::embedder_from_config;
use super::schema::*;
use super::sled_store::{new_cozo_sled_fixed, FixedSledStorage};
use crate::config::KnobyteConfig;
use crate::graph::grounding::{kinds_equivalent, parse_grounding_ref, readable_ref, ParsedRef};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Rows per Cozo write transaction during graph synchronisation. HNSW maintenance keeps a
/// vector cache per transaction, so larger batches re-read fewer neighbour vectors.
const SYNC_BATCH: usize = 2000;

/// A code node to (re-)embed and write.
struct PendingNode {
    id: String,
    file_path: String,
    kind: String,
    name: String,
    qualified_name: String,
    start_line: i64,
    end_line: i64,
    body_hash: String,
    signature: Option<String>,
    docstring: Option<String>,
    body: String,
}

/// Embeddings of `nodes`, in order, computed on all cores.
fn embed_parallel(embedder: &dyn Embedder, nodes: &[PendingNode]) -> Vec<Vec<f32>> {
    let embed = |n: &PendingNode| {
        embedder.embed_code_symbol(&CodeSymbolText {
            kind: &n.kind,
            name: &n.name,
            qualified_name: &n.qualified_name,
            file_path: &n.file_path,
            signature: n.signature.as_deref(),
            docstring: n.docstring.as_deref(),
            body: &n.body,
        })
    };
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
    if nodes.len() < 64 || workers == 1 {
        return nodes.iter().map(embed).collect();
    }
    let chunk = nodes.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let handles: Vec<_> = nodes
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(embed).collect::<Vec<_>>()))
            .collect();
        handles.into_iter().flat_map(|h| h.join().expect("embedding worker panicked")).collect()
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorMatch {
    pub id: String,
    pub score: f64,
    pub distance: f64,
    pub metadata: BTreeMap<String, serde_json::Value>,
}

/// Options of [`CozoEngine::vector_search_with`].
#[derive(Debug, Clone, Default)]
pub struct VectorSearchOptions {
    /// Matches wanted (0 = 10).
    pub k: usize,
    /// Relevance floor (score = 1 - cosine distance, 0-1); `None` = the embedder's default
    /// ([`MIN_RELEVANCE_SCORE`]). `Some(0.0)` disables it.
    pub min_score: Option<f64>,
}

/// Result of [`CozoEngine::vector_search_with`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VectorSearchOutcome {
    /// Up to `k` matches, nearest first, all scoring at least `min_score`.
    pub matches: Vec<VectorMatch>,
    /// Matches requested.
    pub k: usize,
    /// Relevance floor applied.
    pub min_score: f64,
    /// Candidates among the `k` nearest that scored below `min_score` and were left out.
    pub below_floor: usize,
    /// The HNSW index returned too few candidates and an exact scan filled the gap.
    pub exact_fallback: bool,
}

/// Relevance score of a search row (`[id, dist, ...]`): 1 - cosine distance, at least 0.
fn score_of(row: &[DataValue]) -> f64 {
    (1.0f64 - row[1].get_float().unwrap_or(1.0)).max(0.0)
}

/// Build a [`VectorMatch`] from a code-node row (`id, dist, file_path, kind, name, start_line,
/// end_line, qualified_name`) or a wiki row (`id, dist, title, path, summary`).
fn vector_match(row: &[DataValue], is_node: bool) -> VectorMatch {
    let id = row[0].get_str().unwrap_or("").to_string();
    let dist = row[1].get_float().unwrap_or(1.0);
    let mut metadata = BTreeMap::new();
    if is_node {
        if let Some(fp) = row[2].get_str() {
            metadata.insert("file_path".to_string(), serde_json::json!(fp));
        }
        if let Some(kind) = row[3].get_str() {
            metadata.insert("kind".to_string(), serde_json::json!(kind));
        }
        if let Some(name) = row[4].get_str() {
            metadata.insert("name".to_string(), serde_json::json!(name));
        }
        if row.len() >= 7 {
            if let Some(sl) = row[5].get_int() {
                metadata.insert("start_line".to_string(), serde_json::json!(sl));
            }
            if let Some(el) = row[6].get_int() {
                metadata.insert("end_line".to_string(), serde_json::json!(el));
            }
        }
        let q = row.get(7).and_then(|v| v.get_str()).unwrap_or("");
        if !q.is_empty() {
            metadata.insert("qualified_name".to_string(), serde_json::json!(q));
        }
        if let (Some(fp), Some(kind), Some(name)) = (row[2].get_str(), row[3].get_str(), row[4].get_str()) {
            metadata.insert("ref".to_string(), serde_json::json!(node_ref(kind, fp, q, name)));
        }
    } else {
        if let Some(title) = row[2].get_str() {
            metadata.insert("title".to_string(), serde_json::json!(title));
        }
        if let Some(path) = row[3].get_str() {
            metadata.insert("path".to_string(), serde_json::json!(path));
        }
        if let Some(summary) = row[4].get_str() {
            metadata.insert("summary".to_string(), serde_json::json!(summary));
        }
    }
    VectorMatch { id, score: score_of(row), distance: dist, metadata }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageRankResult {
    pub id: String,
    pub rank: f64,
    /// Readable reference `kind:path:qualified_name` (when the node is in `code_nodes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readable_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathStep {
    pub id: String,
    /// Readable reference `kind:path:qualified_name` (when the node is in `code_nodes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readable_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
}

/// (id, kind, file_path, name, qualified_name) of a `code_nodes` row.
type CandidateRow = (String, String, String, String, String);

pub struct CozoEngine {
    db: CozoDb,
    embedder: Arc<dyn Embedder>,
}

/// Cozo database on one of the supported storage engines. Persistent databases use
/// [`FixedSledStorage`] (Sled with working deletions) instead of cozo's built-in Sled engine.
enum CozoDb {
    Mem(cozo::Db<MemStorage>),
    Sled(cozo::Db<FixedSledStorage>),
}

impl CozoDb {
    fn run_script(
        &self,
        script: &str,
        params: BTreeMap<String, DataValue>,
        mutability: ScriptMutability,
    ) -> std::result::Result<NamedRows, cozo::Error> {
        match self {
            CozoDb::Mem(db) => db.run_script(script, params, mutability),
            CozoDb::Sled(db) => db.run_script(script, params, mutability),
        }
    }

    fn run_default(&self, script: &str) -> std::result::Result<NamedRows, cozo::Error> {
        self.run_script(script, BTreeMap::new(), ScriptMutability::Mutable)
    }
}

/// Node details used to enrich graph results: (name, kind, file_path, start_line, qualified_name).
type NodeInfo = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<String>,
);

/// Default relevance floor: vector matches scoring below this (1 - cosine distance) are left
/// out of search results (and counted, see [`VectorSearchOutcome::below_floor`]).
pub const MIN_RELEVANCE_SCORE: f64 = 0.20;
/// Cosine distance matching [`MIN_RELEVANCE_SCORE`] (`1 - MIN_RELEVANCE_SCORE`).
pub const MAX_RELEVANCE_DISTANCE: f64 = 0.80;

fn sqlite_table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |_| Ok(()),
    )
    .is_ok()
}

/// Readable `kind:path:qualified_name` reference (falls back to the bare name).
fn node_ref(kind: &str, file_path: &str, qualified_name: &str, name: &str) -> String {
    let q = if qualified_name.is_empty() {
        name
    } else {
        qualified_name
    };
    readable_ref(kind, file_path, q)
}

fn json_rows(rows: Vec<serde_json::Value>) -> DataValue {
    DataValue::from(&serde_json::Value::Array(rows))
}

impl CozoEngine {
    /// Open or create a persistent CozoDB backed by Sled storage, using the hashed embedder.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_embedder(path, Arc::new(HashedEmbedder))
    }

    /// Open or create a persistent CozoDB with the given embedding backend.
    pub fn open_with_embedder(path: impl AsRef<Path>, embedder: Arc<dyn Embedder>) -> Result<Self> {
        let path_str = path.as_ref().to_string_lossy();
        let db =
            CozoDb::Sled(new_cozo_sled_fixed(path.as_ref()).map_err(|e| {
                format!("Failed to initialize Cozo Sled DB at {}: {}", path_str, e)
            })?);
        let engine = Self { db, embedder };
        engine.ensure_schema()?;
        Ok(engine)
    }

    /// Open the project's CozoDB (`.knobyte/cozo.db`) with the embedding backend configured in
    /// `.knobyte/config.json`. Fails (without downloading anything) when the configured model
    /// is missing.
    pub fn open_configured(config: &KnobyteConfig) -> Result<Self> {
        let embedder = embedder_from_config(&config.embedding)?;
        Self::open_with_embedder(config.cozo_db_path(), embedder)
    }

    /// Create an in-memory CozoDB instance (ideal for tests and ephemeral sessions).
    pub fn in_memory() -> Result<Self> {
        Self::in_memory_with_embedder(Arc::new(HashedEmbedder))
    }

    /// In-memory CozoDB with the given embedding backend.
    pub fn in_memory_with_embedder(embedder: Arc<dyn Embedder>) -> Result<Self> {
        let db = CozoDb::Mem(
            cozo::new_cozo_mem()
                .map_err(|e| format!("Failed to initialize Cozo in-memory DB: {}", e))?,
        );
        let engine = Self { db, embedder };
        engine.ensure_schema()?;
        Ok(engine)
    }

    /// The active embedding backend.
    pub fn embedder(&self) -> &dyn Embedder {
        self.embedder.as_ref()
    }

    fn existing_relations(&self) -> Result<HashSet<String>> {
        let rows = self
            .db
            .run_script("::relations", BTreeMap::new(), ScriptMutability::Immutable)
            .map_err(|e| format!("Failed to list Cozo relations: {}", e))?;
        Ok(rows
            .rows
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.get_str()).map(|s| s.to_string()))
            .collect())
    }

    fn relation_columns(&self, relation: &str) -> Result<HashSet<String>> {
        let rows = self
            .db
            .run_script(
                &format!("::columns {}", relation),
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| format!("Failed to list Cozo columns of {}: {}", relation, e))?;
        Ok(rows
            .rows
            .iter()
            .filter_map(|r| r.first().and_then(|v| v.get_str()).map(|s| s.to_string()))
            .collect())
    }

    fn create_tolerant(&self, name: &str, script: &str) -> Result<()> {
        if let Err(err) = self.db.run_default(script) {
            let err_msg = err.to_string();
            if !err_msg.contains("already exists")
                && !err_msg.contains("conflicts with an existing")
            {
                return Err(format!("Failed to create Cozo relation {}: {}", name, err_msg).into());
            }
        }
        Ok(())
    }

    fn drop_vector_relation(
        &self,
        relation: &str,
        index: &str,
        existing: &HashSet<String>,
    ) -> Result<()> {
        let idx = format!("{}:{}", relation, index);
        if existing.contains(&idx) {
            self.db
                .run_default(&format!("::hnsw drop {}", idx))
                .map_err(|e| format!("Failed to drop vector index {}: {}", idx, e))?;
        }
        if existing.contains(relation) {
            self.db
                .run_default(&format!("::remove {}", relation))
                .map_err(|e| format!("Failed to remove relation {}: {}", relation, e))?;
        }
        Ok(())
    }

    fn write_space(&self, relation: &str, embedder_id: &str, dim: usize) -> Result<()> {
        self.put_rows(
            "?[relation, embedder_id, dim] <- $data :put embedding_meta { relation => embedder_id, dim }",
            vec![serde_json::json!([relation, embedder_id, dim as i64])],
            "embedding_meta",
        )
    }

    /// Embedder id and dimension the vectors of `relation` were built with.
    pub fn stored_space(&self, relation: &str) -> Result<Option<(String, usize)>> {
        let mut params = BTreeMap::new();
        params.insert("r".to_string(), DataValue::from(relation));
        let rows = self
            .db
            .run_script(
                "?[id, dim] := *embedding_meta{relation, embedder_id: id, dim}, relation == $r",
                params,
                ScriptMutability::Immutable,
            )
            .map_err(|e| format!("Failed to read embedding_meta: {}", e))?;
        Ok(rows.rows.first().and_then(|r| {
            Some((
                r.first()?.get_str()?.to_string(),
                r.get(1)?.get_int()? as usize,
            ))
        }))
    }

    /// Ensure all relations and HNSW vector indices exist. Only "already exists" conditions are
    /// tolerated; any other schema error is returned. A `code_nodes` relation from an older
    /// layout (derived data) is dropped and recreated. Vector relations created here use the
    /// active embedder's dimension; existing ones keep the space they were built with (recorded
    /// in `embedding_meta`; relations from before that table existed are hashed 128-dim) until
    /// the next sync re-embeds them.
    pub fn ensure_schema(&self) -> Result<()> {
        let mut existing = self.existing_relations()?;

        if existing.contains("code_nodes")
            && !self
                .relation_columns("code_nodes")?
                .contains("qualified_name")
        {
            self.drop_vector_relation("code_nodes", "node_vec", &existing)?;
            existing.remove("code_nodes");
            existing.remove("code_nodes:node_vec");
        }

        if !existing.contains("embedding_meta") {
            self.create_tolerant("embedding_meta", CREATE_EMBEDDING_META)?;
        }
        if !existing.contains("code_edges") {
            self.create_tolerant("code_edges", CREATE_CODE_EDGES)?;
        }

        for (relation, index) in VECTOR_RELATIONS {
            let created = !existing.contains(relation);
            let dim = if created {
                let dim = self.embedder.dim();
                self.create_tolerant(relation, &create_vector_relation(relation, dim))?;
                self.write_space(relation, &self.embedder.id(), dim)?;
                dim
            } else {
                match self.stored_space(relation)? {
                    Some((_, dim)) => dim,
                    None => {
                        // Created before embedding_meta existed: always hashed, 128-dim.
                        self.write_space(relation, HASHED_EMBEDDER_ID, EMBEDDING_DIM)?;
                        EMBEDDING_DIM
                    }
                }
            };
            let idx = format!("{}:{}", relation, index);
            if created || !existing.contains(&idx) {
                self.create_tolerant(&idx, &create_hnsw(relation, index, dim))?;
            }
        }

        Ok(())
    }

    /// If the vectors of `relation` were built by a different embedder (id or dimension), drop
    /// and recreate the relation and its HNSW index with the active embedder's dimension.
    /// Returns true when the relation was recreated (it is then empty until re-synced).
    pub fn ensure_vector_space(&self, relation: &str) -> Result<bool> {
        let index = VECTOR_RELATIONS
            .iter()
            .find(|(r, _)| *r == relation)
            .map(|(_, i)| *i)
            .ok_or_else(|| format!("'{}' is not a vector relation", relation))?;
        let active = (self.embedder.id(), self.embedder.dim());
        if self.stored_space(relation)?.as_ref() == Some(&active) {
            return Ok(false);
        }
        let existing = self.existing_relations()?;
        self.drop_vector_relation(relation, index, &existing)?;
        self.create_tolerant(relation, &create_vector_relation(relation, active.1))?;
        self.create_tolerant(
            &format!("{}:{}", relation, index),
            &create_hnsw(relation, index, active.1),
        )?;
        self.write_space(relation, &active.0, active.1)?;
        Ok(true)
    }

    /// When the vectors of the search `target` ("code" or "wiki") were not built by the active
    /// embedder, a message explaining the mismatch.
    pub fn space_mismatch(&self, target: &str) -> Result<Option<String>> {
        let relation = if target == "wiki" {
            "wiki_entities"
        } else {
            "code_nodes"
        };
        let active = (self.embedder.id(), self.embedder.dim());
        let stored = self.stored_space(relation)?;
        Ok(match stored {
            Some(s) if s == active => None,
            Some((id, dim)) => Some(format!(
                "The {} vector index was built with embedder '{}' ({}-dim) but the configured backend is '{}' ({}-dim). Run 'knobyte cozo sync' to re-embed.",
                relation, id, dim, active.0, active.1
            )),
            None => Some(format!(
                "The {} vector index has no embedder metadata. Run 'knobyte cozo sync' to re-embed.",
                relation
            )),
        })
    }

    /// Whether any vector relation must be re-embedded for the active embedder.
    pub fn needs_reembed(&self) -> Result<bool> {
        Ok(self.space_mismatch("code")?.is_some() || self.space_mismatch("wiki")?.is_some())
    }

    fn run(
        &self,
        script: &str,
        params: serde_json::Value,
        mutability: ScriptMutability,
    ) -> Result<serde_json::Value> {
        let mut param_map = BTreeMap::new();
        if let serde_json::Value::Object(map) = params {
            for (k, v) in map {
                param_map.insert(k, DataValue::from(&v));
            }
        }

        let named_rows = self
            .db
            .run_script(script, param_map, mutability)
            .map_err(|e| format!("Cozo Datalog query error: {}", e))?;

        Ok(named_rows.into_json())
    }

    /// Execute a read-only CozoScript Datalog query and return JSON results.
    /// Scripts that write (`:put`, `:rm`, `:create`, `::remove`, ...) are rejected.
    pub fn datalog_query(
        &self,
        script: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.run(script, params, ScriptMutability::Immutable)
    }

    /// Alias of [`CozoEngine::datalog_query`] (read-only).
    pub fn run_query(&self, script: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.datalog_query(script, params)
    }

    /// Execute a CozoScript that may modify the store. Explicit opt-in for callers that really
    /// need writes; prefer [`CozoEngine::datalog_query`].
    pub fn datalog_query_mutable(
        &self,
        script: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.run(script, params, ScriptMutability::Mutable)
    }

    /// Alias of [`CozoEngine::datalog_query_mutable`].
    pub fn run_query_mutable(
        &self,
        script: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        self.datalog_query_mutable(script, params)
    }

    fn put_rows(&self, script: &str, rows: Vec<serde_json::Value>, what: &str) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut params = BTreeMap::new();
        params.insert("data".to_string(), json_rows(rows));
        self.db
            .run_script(script, params, ScriptMutability::Mutable)
            .map_err(|e| format!("Failed to write {} into Cozo: {}", what, e))?;
        Ok(())
    }

    fn existing_keys(&self, script: &str) -> Result<Vec<Vec<String>>> {
        let rows = self
            .db
            .run_script(script, BTreeMap::new(), ScriptMutability::Immutable)
            .map_err(|e| format!("Cozo key scan failed: {}", e))?;
        Ok(rows
            .rows
            .iter()
            .map(|r| {
                r.iter()
                    .map(|v| v.get_str().unwrap_or("").to_string())
                    .collect()
            })
            .collect())
    }

    /// Write rows in bounded batches (one Cozo transaction each), so a large sync never
    /// builds one giant parameter list and progress is incremental.
    fn put_rows_batched(&self, script: &str, rows: Vec<serde_json::Value>, what: &str) -> Result<()> {
        let mut rows = rows;
        while !rows.is_empty() {
            let rest = rows.split_off(rows.len().min(SYNC_BATCH));
            self.put_rows(script, rows, what)?;
            rows = rest;
        }
        Ok(())
    }

    /// Synchronize the code graph in Cozo with SQLite `graph.db`, incrementally: nodes whose
    /// identity, location and body hash are unchanged keep their stored embedding and are not
    /// rewritten; changed or new nodes are re-embedded and upserted in batches; rows that no
    /// longer exist in SQLite are removed. Edges are diffed the same way.
    /// If the active embedder differs from the one that built `code_nodes`, the relation and
    /// its HNSW index are recreated with the new dimension and every node is re-embedded.
    pub fn sync_from_graph(&self, graph_conn: &Connection) -> Result<(usize, usize)> {
        self.ensure_vector_space("code_nodes")?;
        if !sqlite_table_exists(graph_conn, "nodes") {
            // Graph never built: nothing to sync, keep what Cozo has.
            return Ok((0, 0));
        }
        let project_root: Option<PathBuf> = graph_conn
            .query_row(
                "SELECT value FROM project_metadata WHERE key = 'project_root'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .map(PathBuf::from);
        let mut file_cache: HashMap<String, Option<Vec<String>>> = HashMap::new();

        // What Cozo already holds: id -> (file_path, kind, name, start, end, body_hash, qualified).
        type Stored = (String, String, String, i64, i64, String, String);
        let mut stored: HashMap<String, Stored> = HashMap::new();
        if let Ok(rows) = self.db.run_script(
            "?[id, file_path, kind, name, start_line, end_line, body_hash, qualified_name] := \
             *code_nodes{id, file_path, kind, name, start_line, end_line, body_hash, qualified_name}",
            BTreeMap::new(),
            ScriptMutability::Immutable,
        ) {
            for r in rows.rows {
                if r.len() < 8 {
                    continue;
                }
                let st = |i: usize| r[i].get_str().unwrap_or("").to_string();
                stored.insert(
                    st(0),
                    (
                        st(1),
                        st(2),
                        st(3),
                        r[4].get_int().unwrap_or(-1),
                        r[5].get_int().unwrap_or(-1),
                        st(6),
                        st(7),
                    ),
                );
            }
        }

        // 1. Nodes
        let mut stmt = graph_conn.prepare(
            "SELECT id, file_path, kind, name, qualified_name, start_line, end_line,
                    COALESCE(body_hash, ''), signature, docstring
             FROM nodes",
        )?;
        let node_rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
            ))
        })?;

        // Changed or new rows (source read once per file), embedded in parallel below.
        // `KNOBYTE_COZO_TIMING=1` prints the phase timings to stderr.
        let timing = std::env::var_os("KNOBYTE_COZO_TIMING").is_some();
        let started = std::time::Instant::now();
        let mut pending: Vec<PendingNode> = Vec::new();
        let mut node_ids: HashSet<String> = HashSet::new();
        for res in node_rows {
            let (
                id,
                file_path,
                kind,
                name,
                qualified_name,
                start_line,
                end_line,
                body_hash,
                signature,
                docstring,
            ) = res?;
            node_ids.insert(id.clone());
            let unchanged = stored.get(&id).is_some_and(|s| {
                s.0 == file_path
                    && s.1 == kind
                    && s.2 == name
                    && s.3 == start_line
                    && s.4 == end_line
                    && s.5 == body_hash
                    && s.6 == qualified_name
            });
            if unchanged {
                continue;
            }

            let body = if kind == "file" || kind == "module" || file_path.is_empty() {
                String::new()
            } else {
                project_root
                    .as_ref()
                    .and_then(|root| {
                        file_cache
                            .entry(file_path.clone())
                            .or_insert_with(|| {
                                std::fs::read_to_string(root.join(&file_path))
                                    .ok()
                                    .map(|c| c.lines().map(|l| l.to_string()).collect())
                            })
                            .as_ref()
                            .map(|lines| {
                                let start = (start_line.max(1) as usize - 1).min(lines.len());
                                let end = (end_line.max(0) as usize).min(lines.len()).max(start);
                                lines[start..end].join("\n")
                            })
                    })
                    .unwrap_or_default()
            };
            pending.push(PendingNode {
                id,
                file_path,
                kind,
                name,
                qualified_name,
                start_line,
                end_line,
                body_hash,
                signature,
                docstring,
                body,
            });
        }
        drop(file_cache);
        let t_read = started.elapsed();
        let embeddings = embed_parallel(self.embedder.as_ref(), &pending);
        let t_embed = started.elapsed();
        let node_tuples: Vec<serde_json::Value> = pending
            .into_iter()
            .zip(embeddings)
            .map(|(n, embedding)| {
                serde_json::json!([
                    n.id,
                    n.file_path,
                    n.kind,
                    n.name,
                    n.start_line,
                    n.end_line,
                    n.body_hash,
                    embedding,
                    n.qualified_name
                ])
            })
            .collect();
        let written = node_tuples.len();

        let nodes_count = node_ids.len();
        self.put_rows_batched(
            r#"
                ?[id, file_path, kind, name, start_line, end_line, body_hash, embedding, qualified_name] <- $data
                :put code_nodes { id => file_path, kind, name, start_line, end_line, body_hash, embedding, qualified_name }
            "#,
            node_tuples,
            "code_nodes",
        )?;
        let t_put = started.elapsed();
        if timing {
            eprintln!(
                "cozo sync: {} nodes, {} rewritten | read {:?}, embed {:?}, write + index {:?}",
                nodes_count,
                written,
                t_read,
                t_embed - t_read,
                t_put - t_embed
            );
        }

        let stale_nodes: Vec<serde_json::Value> = stored
            .keys()
            .filter(|id| !node_ids.contains(*id))
            .map(|id| serde_json::json!([id]))
            .collect();
        self.put_rows_batched(
            "?[id] <- $data :rm code_nodes {id}",
            stale_nodes,
            "code_nodes removals",
        )?;

        // 2. Edges
        let existing_edges: HashMap<(String, String, String), String> = self
            .existing_keys("?[s, t, k, f] := *code_edges{source_id: s, target_id: t, kind: k, file_path: f}")?
            .into_iter()
            .filter(|k| k.len() == 4)
            .map(|k| ((k[0].clone(), k[1].clone(), k[2].clone()), k[3].clone()))
            .collect();
        let mut edge_stmt = graph_conn.prepare(
            "SELECT e.source, e.target, e.kind, COALESCE(n.file_path, '') FROM edges e LEFT JOIN nodes n ON e.source = n.id"
        )?;
        let edge_rows = edge_stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;

        let mut edge_tuples = Vec::new();
        let mut edge_keys: HashSet<(String, String, String)> = HashSet::new();
        for res in edge_rows {
            let (source_id, target_id, kind, file_path) = res?;
            let key = (source_id.clone(), target_id.clone(), kind.clone());
            if !edge_keys.insert(key.clone()) {
                continue;
            }
            if existing_edges.get(&key) == Some(&file_path) {
                continue;
            }
            edge_tuples.push(serde_json::json!([source_id, target_id, kind, file_path]));
        }

        let edges_count = edge_keys.len();
        self.put_rows_batched(
            r#"
                ?[source_id, target_id, kind, file_path] <- $data
                :put code_edges { source_id, target_id, kind => file_path }
            "#,
            edge_tuples,
            "code_edges",
        )?;

        let stale_edges: Vec<serde_json::Value> = existing_edges
            .keys()
            .filter(|k| !edge_keys.contains(*k))
            .map(|k| serde_json::json!([k.0, k.1, k.2]))
            .collect();
        self.put_rows_batched(
            "?[source_id, target_id, kind] <- $data :rm code_edges {source_id, target_id, kind}",
            stale_edges,
            "code_edges removals",
        )?;

        if timing {
            eprintln!("cozo sync: {} edges | total {:?}", edges_count, started.elapsed());
        }
        Ok((nodes_count, edges_count))
    }

    /// Replace wiki entities in Cozo with the non-shadowed entities of SQLite `wiki.db`
    /// (upsert with embeddings, remove entities that no longer exist).
    /// Like [`CozoEngine::sync_from_graph`], re-embeds everything when the embedder changed.
    pub fn sync_from_wiki(&self, wiki_conn: &Connection) -> Result<usize> {
        self.ensure_vector_space("wiki_entities")?;
        if !sqlite_table_exists(wiki_conn, "wiki_entities") {
            // Wiki index never built: nothing to sync, keep what Cozo has.
            return Ok(0);
        }
        let mut stmt = wiki_conn.prepare(
            "SELECT id, title, file, type, COALESCE(summary, ''), body FROM wiki_entities WHERE shadowed = 0"
        )?;

        let entity_rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;

        let mut tuples = Vec::new();
        let mut ids: HashSet<String> = HashSet::new();
        for res in entity_rows {
            let (id, title, path, type_str, summary, body) = res?;
            if !ids.insert(id.clone()) {
                continue;
            }
            let tags = vec![type_str.clone()];
            let embedding = self.embedder.embed_wiki(&title, &summary, &type_str, &body);
            tuples.push(serde_json::json!([
                id, title, path, tags, summary, embedding
            ]));
        }

        let count = tuples.len();
        self.put_rows(
            r#"
                ?[id, title, path, tags, summary, embedding] <- $data
                :put wiki_entities { id => title, path, tags, summary, embedding }
            "#,
            tuples,
            "wiki_entities",
        )?;

        let stale: Vec<serde_json::Value> = self
            .existing_keys("?[id] := *wiki_entities{id}")?
            .into_iter()
            .filter_map(|k| k.into_iter().next())
            .filter(|id| !ids.contains(id))
            .map(|id| serde_json::json!([id]))
            .collect();
        self.put_rows(
            "?[id] <- $data :rm wiki_entities {id}",
            stale,
            "wiki_entities removals",
        )?;

        Ok(count)
    }

    /// Search for nearest nodes or wiki entities (see [`CozoEngine::vector_search_with`]),
    /// with the embedder's default relevance floor. Returns only the matches.
    pub fn vector_search(
        &self,
        query_text: &str,
        target: &str,
        k: usize,
    ) -> Result<Vec<VectorMatch>> {
        Ok(self
            .vector_search_with(query_text, target, &VectorSearchOptions { k, min_score: None })?
            .matches)
    }

    /// Search for the `k` nearest code nodes (`target` "code") or wiki entities ("wiki").
    ///
    /// The HNSW index is over-fetched (`max(4k, k + 16)` candidates, `ef >= 2 x` that), then
    /// filtered: external `module` nodes are skipped and the relevance floor applied. When the
    /// index returned fewer candidates than it should have (approximate recall, or too many
    /// filtered rows), an exact scan over the relation fills the gap, so a search returns `k`
    /// matches whenever `k` candidates clear the floor. Candidates among the `k` nearest that
    /// the floor removed are counted in [`VectorSearchOutcome::below_floor`], never dropped
    /// silently.
    pub fn vector_search_with(
        &self,
        query_text: &str,
        target: &str,
        opts: &VectorSearchOptions,
    ) -> Result<VectorSearchOutcome> {
        // Never compare vectors from different embedding spaces.
        if let Some(msg) = self.space_mismatch(target)? {
            return Err(msg.into());
        }
        let k = if opts.k == 0 { 10 } else { opts.k };
        let min_score = opts
            .min_score
            .unwrap_or_else(|| self.embedder.min_relevance())
            .clamp(0.0, 1.0);
        let is_node = target != "wiki";
        let query_vec = DataValue::Vec(cozo::Vector::F32(ndarray::Array1::from(
            self.embedder.embed(query_text),
        )));

        let fetch = (k * 4).max(k + 16);
        let ef = (2 * fetch).max(64);
        let hnsw = if is_node {
            format!(
                "?[id, dist, file_path, kind, name, start_line, end_line, qualified_name] :=
                    ~code_nodes:node_vec{{id | query: $query_vec, k: $k, ef: {ef}, bind_distance: dist}},
                    *code_nodes{{id, file_path, kind, name, start_line, end_line, qualified_name}}
                :order dist"
            )
        } else {
            format!(
                "?[id, dist, title, path, summary] :=
                    ~wiki_entities:wiki_vec{{id | query: $query_vec, k: $k, ef: {ef}, bind_distance: dist}},
                    *wiki_entities{{id, title, path, summary}}
                :order dist"
            )
        };
        let run = |script: &str, limit: usize, what: &str| -> Result<Vec<Vec<DataValue>>> {
            let mut params = BTreeMap::new();
            params.insert("query_vec".to_string(), query_vec.clone());
            params.insert("k".to_string(), DataValue::from(limit as i64));
            Ok(self
                .db
                .run_script(script, params, ScriptMutability::Immutable)
                .map_err(|e| format!("{} vector search failed: {}", what, e))?
                .rows)
        };
        let rows = run(&hnsw, fetch, "HNSW")?;
        let returned = rows.len();
        let candidate = |row: &Vec<DataValue>| {
            row.len() >= 5 && !(is_node && row[3].get_str() == Some("module"))
        };
        let mut candidates: Vec<Vec<DataValue>> = rows.into_iter().filter(|r| candidate(r)).collect();
        let passing = |c: &[Vec<DataValue>]| {
            c.iter()
                .take(k)
                .filter(|r| score_of(r) >= min_score)
                .count()
        };

        // Short of k: when the index returned fewer rows than it holds (approximate recall,
        // or filtered rows crowding out candidates), fill from an exact scan.
        let mut exact_fallback = false;
        if passing(&candidates) < k {
            let total = self.relation_count(if is_node { "code_nodes" } else { "wiki_entities" })?;
            if (candidates.len() < k && returned < total) || returned < fetch.min(total) {
                let exact = if is_node {
                    r#"?[id, dist, file_path, kind, name, start_line, end_line, qualified_name] :=
                        *code_nodes{id, file_path, kind, name, start_line, end_line, qualified_name, embedding},
                        kind != "module",
                        dist = cos_dist(embedding, $query_vec)
                    :order dist
                    :limit $k"#
                } else {
                    r#"?[id, dist, title, path, summary] :=
                        *wiki_entities{id, title, path, summary, embedding},
                        dist = cos_dist(embedding, $query_vec)
                    :order dist
                    :limit $k"#
                };
                candidates = run(exact, k, "Exact")?.into_iter().filter(|r| candidate(r)).collect();
                exact_fallback = true;
            }
        }

        candidates.truncate(k);
        let considered = candidates.len();
        let matches: Vec<VectorMatch> = candidates
            .iter()
            .filter(|r| score_of(r) >= min_score)
            .map(|r| vector_match(r, is_node))
            .collect();
        Ok(VectorSearchOutcome {
            below_floor: considered - matches.len(),
            matches,
            k,
            min_score,
            exact_fallback,
        })
    }

    /// Number of rows in a stored relation.
    fn relation_count(&self, relation: &str) -> Result<usize> {
        let rows = self
            .db
            .run_script(
                &format!("?[count(id)] := *{relation}{{id}}"),
                BTreeMap::new(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| format!("Counting {} failed: {}", relation, e))?;
        Ok(rows
            .rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.get_int())
            .unwrap_or(0)
            .max(0) as usize)
    }

    /// Lookup code node metadata for rich graph query results
    pub fn lookup_code_node(
        &self,
        node_id: &str,
    ) -> (Option<String>, Option<String>, Option<String>, Option<i64>) {
        let (name, kind, file_path, line, _) = self.lookup_node_info(node_id);
        (name, kind, file_path, line)
    }

    fn lookup_node_info(&self, node_id: &str) -> NodeInfo {
        let script = r#"
            ?[name, kind, file_path, line, q] := *code_nodes{id: $id, name, kind, file_path, start_line: line, qualified_name: q}
        "#;
        let mut params = BTreeMap::new();
        params.insert("id".to_string(), DataValue::from(node_id));
        if let Ok(res) = self
            .db
            .run_script(script, params, ScriptMutability::Immutable)
        {
            if let Some(row) = res.rows.into_iter().next() {
                if row.len() >= 5 {
                    let s = |i: usize| row[i].get_str().map(|s| s.to_string());
                    return (s(0), s(1), s(2), row[3].get_int(), s(4));
                }
            }
        }
        (None, None, None, None, None)
    }

    /// Readable `kind:path:qualified_name` reference of a code node id, if it is known.
    pub fn readable_ref_of(&self, node_id: &str) -> Option<String> {
        let (name, kind, file_path, _, q) = self.lookup_node_info(node_id);
        Some(node_ref(
            &kind?,
            &file_path?,
            q.as_deref().unwrap_or(""),
            name.as_deref().unwrap_or(""),
        ))
    }

    /// Compute PageRank centrality scores over the code dependency graph.
    pub fn pagerank(
        &self,
        theta: Option<f64>,
        iterations: Option<usize>,
    ) -> Result<Vec<PageRankResult>> {
        let theta_val = theta.unwrap_or(0.85);
        let iter_val = iterations.unwrap_or(20) as i64;

        let script = r#"
            edges[src, dst] := *code_edges{source_id: src, target_id: dst}
            ?[node, rank] <~ PageRank(edges[], theta: $theta, iterations: $iter)
            :order -rank
        "#;

        let mut params = BTreeMap::new();
        params.insert("theta".to_string(), DataValue::from(theta_val));
        params.insert("iter".to_string(), DataValue::from(iter_val));

        let named_rows = self
            .db
            .run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| format!("PageRank execution failed: {}", e))?;

        let mut results = Vec::new();
        for row in named_rows.rows {
            if row.len() >= 2 {
                let id = row[0].get_str().unwrap_or("").to_string();
                let rank = row[1].get_float().unwrap_or(0.0);
                let (name, kind, file_path, line, q) = self.lookup_node_info(&id);
                let readable_ref = match (&kind, &file_path) {
                    (Some(k), Some(fp)) => Some(node_ref(
                        k,
                        fp,
                        q.as_deref().unwrap_or(""),
                        name.as_deref().unwrap_or(""),
                    )),
                    _ => None,
                };
                results.push(PageRankResult {
                    id,
                    rank,
                    readable_ref,
                    name,
                    kind,
                    file_path,
                    line,
                });
            }
        }

        Ok(results)
    }

    /// Resolve a user-supplied node reference to a `code_nodes` id. Accepts, in order: a raw
    /// node id, a readable reference (`kind:path:qualified_name`), or a symbol name / qualified
    /// name. Errors clearly when nothing or more than one symbol matches.
    pub fn resolve_node_ref(&self, reference: &str) -> Result<String> {
        let reference = reference.trim();
        let mut params = BTreeMap::new();
        params.insert("v".to_string(), DataValue::from(reference));

        let by_id = self
            .db
            .run_script(
                "?[id] := *code_nodes{id}, id == $v",
                params.clone(),
                ScriptMutability::Immutable,
            )
            .map_err(|e| format!("Node lookup failed: {}", e))?;
        if !by_id.rows.is_empty() {
            return Ok(reference.to_string());
        }

        let fetch = |script: &str,
                     params: BTreeMap<String, DataValue>|
         -> Result<Vec<CandidateRow>> {
            let rows = self
                .db
                .run_script(script, params, ScriptMutability::Immutable)
                .map_err(|e| format!("Node lookup failed: {}", e))?;
            Ok(rows
                .rows
                .iter()
                .map(|r| {
                    let s = |i: usize| r.get(i).and_then(|v| v.get_str()).unwrap_or("").to_string();
                    (s(0), s(1), s(2), s(3), s(4))
                })
                .collect())
        };

        let candidates: Vec<CandidateRow> = match parse_grounding_ref(reference) {
            ParsedRef::Readable(r) => {
                let mut p = BTreeMap::new();
                p.insert("p".to_string(), DataValue::from(r.file_path.as_str()));
                let norm = |s: &str| s.replace("::", ".");
                let bare = !r.qualified_name.contains("::") && !r.qualified_name.contains('.');
                fetch(
                    "?[id, kind, file_path, name, qualified_name] := *code_nodes{id, kind, file_path, name, qualified_name}, file_path == $p",
                    p,
                )?
                .into_iter()
                .filter(|(_, kind, _, name, q)| {
                    kinds_equivalent(&r.kind, kind)
                        && (norm(q) == norm(&r.qualified_name) || (bare && name == &r.qualified_name))
                })
                .collect()
            }
            _ => fetch(
                r#"
                ?[id, kind, file_path, name, qualified_name] := *code_nodes{id, kind, file_path, name, qualified_name}, name == $v
                ?[id, kind, file_path, name, qualified_name] := *code_nodes{id, kind, file_path, name, qualified_name}, qualified_name == $v
                "#,
                params,
            )?
            .into_iter()
            .filter(|(_, kind, _, _, _)| kind != "file" && kind != "module")
            .collect(),
        };

        match candidates.len() {
            0 => Err(format!(
                "No code symbol matches '{}'. Use a node id, a symbol name, or kind:path:qualified_name (run 'knobyte cozo sync' if the graph changed).",
                reference
            )
            .into()),
            1 => Ok(candidates[0].0.clone()),
            n => {
                let listed: Vec<String> = candidates
                    .iter()
                    .take(10)
                    .map(|(_, kind, file, _, q)| format!("{}:{}:{}", kind, file, q))
                    .collect();
                Err(format!(
                    "'{}' is ambiguous ({} matches). Use one of: {}",
                    reference,
                    n,
                    listed.join(", ")
                )
                .into())
            }
        }
    }

    /// Find shortest path between two nodes in the code graph using ShortestPathBFS.
    /// `start` and `target` may be node ids, symbol names or readable refs
    /// (see [`CozoEngine::resolve_node_ref`]).
    pub fn shortest_path(&self, start: &str, target: &str) -> Result<Option<Vec<String>>> {
        let start_id = self.resolve_node_ref(start)?;
        let target_id = self.resolve_node_ref(target)?;
        let script = r#"
            edges[src, dst] := *code_edges{source_id: src, target_id: dst}
            start[] <- [[$start]]
            end[] <- [[$target]]
            ?[source, target, path] <~ ShortestPathBFS(edges[], start[], end[])
        "#;

        let mut params = BTreeMap::new();
        params.insert("start".to_string(), DataValue::from(start_id.as_str()));
        params.insert("target".to_string(), DataValue::from(target_id.as_str()));

        let named_rows = self
            .db
            .run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| format!("ShortestPath execution failed: {}", e))?;

        for row in named_rows.rows {
            if row.len() >= 3 {
                if let Some(list) = row[2].get_slice() {
                    let path_nodes: Vec<String> = list
                        .iter()
                        .filter_map(|v| v.get_str().map(|s| s.to_string()))
                        .collect();
                    if !path_nodes.is_empty() {
                        return Ok(Some(path_nodes));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Find shortest path with enriched symbol details (name, kind, file_path, line).
    pub fn shortest_path_detailed(
        &self,
        start_id: &str,
        target_id: &str,
    ) -> Result<Option<Vec<PathStep>>> {
        let path_nodes = match self.shortest_path(start_id, target_id)? {
            Some(nodes) => nodes,
            None => return Ok(None),
        };

        let mut steps = Vec::new();
        for id in path_nodes {
            let readable_ref = self.readable_ref_of(&id);
            let (name, kind, file_path, line) = self.lookup_code_node(&id);
            steps.push(PathStep {
                id,
                readable_ref,
                name,
                kind,
                file_path,
                line,
            });
        }
        Ok(Some(steps))
    }
}
