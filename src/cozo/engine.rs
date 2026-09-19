use std::collections::BTreeMap;
use std::path::Path;
use cozo::{DataValue, DbInstance, ScriptMutability};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::embedding::Embedder;
use super::schema::*;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorMatch {
    pub id: String,
    pub score: f64,
    pub distance: f64,
    pub metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageRankResult {
    pub id: String,
    pub rank: f64,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
}

pub struct CozoEngine {
    db: DbInstance,
}

impl CozoEngine {
    /// Open or create a persistent CozoDB backed by Sled storage.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path_str = path.as_ref().to_string_lossy();
        let db = DbInstance::new("sled", path_str.as_ref(), "")
            .map_err(|e| format!("Failed to initialize Cozo Sled DB at {}: {}", path_str, e))?;
        let engine = Self { db };
        engine.ensure_schema()?;
        Ok(engine)
    }

    /// Create an in-memory CozoDB instance (ideal for tests and ephemeral sessions).
    pub fn in_memory() -> Result<Self> {
        let db = DbInstance::new("mem", "", "")
            .map_err(|e| format!("Failed to initialize Cozo in-memory DB: {}", e))?;
        let engine = Self { db };
        engine.ensure_schema()?;
        Ok(engine)
    }

    /// Ensure all relations and HNSW vector indices exist.
    pub fn ensure_schema(&self) -> Result<()> {
        let schemas = [
            CREATE_CODE_NODES,
            CREATE_CODE_EDGES,
            CREATE_WIKI_ENTITIES,
            CREATE_CODE_NODES_HNSW,
            CREATE_WIKI_ENTITIES_HNSW,
        ];

        for script in schemas {
            if let Err(err) = self.db.run_default(script) {
                let err_msg = err.to_string();
                // Ignore "already exists" errors
                if !err_msg.contains("already exists") && !err_msg.contains("duplicate") {
                    // In case relation exists, skip
                }
            }
        }

        Ok(())
    }

    /// Execute an arbitrary CozoScript Datalog query and return JSON results.
    pub fn datalog_query(&self, script: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let mut param_map = BTreeMap::new();
        if let serde_json::Value::Object(map) = params {
            for (k, v) in map {
                param_map.insert(k, DataValue::from(&v));
            }
        }

        let named_rows = self.db.run_script(script, param_map, ScriptMutability::Mutable)
            .map_err(|e| format!("Cozo Datalog query error: {}", e))?;

        Ok(named_rows.into_json())
    }

    /// Sync code graph nodes and edges from SQLite `graph.db` into Cozo relations with embeddings.
    pub fn sync_from_graph(&self, graph_conn: &Connection) -> Result<(usize, usize)> {
        // 1. Fetch nodes
        let mut stmt = graph_conn.prepare(
            "SELECT id, file_path, kind, name, start_line, end_line, body_hash FROM nodes"
        )?;

        let node_rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?;

        let mut node_tuples = Vec::new();
        for res in node_rows {
            let (id, file_path, kind, name, start_line, end_line, body_hash) = res?;
            let embed_text = format!("{} {} in {}", kind, name, file_path);
            let embedding = Embedder::embed(&embed_text);
            node_tuples.push(serde_json::json!([
                id,
                file_path,
                kind,
                name,
                start_line,
                end_line,
                body_hash,
                embedding
            ]));
        }

        let nodes_count = node_tuples.len();
        if !node_tuples.is_empty() {
            let script = r#"
                ?[id, file_path, kind, name, start_line, end_line, body_hash, embedding] <- $data
                :put code_nodes { id => file_path, kind, name, start_line, end_line, body_hash, embedding }
            "#;
            let mut params = BTreeMap::new();
            params.insert("data".to_string(), DataValue::from(&serde_json::Value::Array(node_tuples)));
            self.db.run_script(script, params, ScriptMutability::Mutable)
                .map_err(|e| format!("Failed to put code_nodes into Cozo: {}", e))?;
        }

        // 2. Fetch edges
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
        for res in edge_rows {
            let (source_id, target_id, kind, file_path) = res?;
            edge_tuples.push(serde_json::json!([
                source_id,
                target_id,
                kind,
                file_path
            ]));
        }

        let edges_count = edge_tuples.len();
        if !edge_tuples.is_empty() {
            let script = r#"
                ?[source_id, target_id, kind, file_path] <- $data
                :put code_edges { source_id, target_id, kind => file_path }
            "#;
            let mut params = BTreeMap::new();
            params.insert("data".to_string(), DataValue::from(&serde_json::Value::Array(edge_tuples)));
            self.db.run_script(script, params, ScriptMutability::Mutable)
                .map_err(|e| format!("Failed to put code_edges into Cozo: {}", e))?;
        }

        Ok((nodes_count, edges_count))
    }

    /// Sync wiki entities from SQLite `wiki.db` into Cozo relations with embeddings.
    pub fn sync_from_wiki(&self, wiki_conn: &Connection) -> Result<usize> {
        let mut stmt = wiki_conn.prepare(
            "SELECT entity_key, title, file, type, COALESCE(summary, '') FROM wiki_entities WHERE shadowed = 0"
        )?;

        let entity_rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;

        let mut tuples = Vec::new();
        for res in entity_rows {
            let (id, title, path, type_str, summary) = res?;
            let tags = vec![type_str.clone()];
            let embed_text = format!("{} {} {}", title, type_str, summary);
            let embedding = Embedder::embed(&embed_text);
            tuples.push(serde_json::json!([
                id,
                title,
                path,
                tags,
                summary,
                embedding
            ]));
        }

        let count = tuples.len();
        if !tuples.is_empty() {
            let script = r#"
                ?[id, title, path, tags, summary, embedding] <- $data
                :put wiki_entities { id => title, path, tags, summary, embedding }
            "#;
            let mut params = BTreeMap::new();
            params.insert("data".to_string(), DataValue::from(&serde_json::Value::Array(tuples)));
            self.db.run_script(script, params, ScriptMutability::Mutable)
                .map_err(|e| format!("Failed to put wiki_entities into Cozo: {}", e))?;
        }

        Ok(count)
    }

    /// Search for nearest nodes or wiki entities using HNSW vector index.
    pub fn vector_search(&self, query_text: &str, target: &str, k: usize) -> Result<Vec<VectorMatch>> {
        let query_vec = Embedder::embed(query_text);
        let k_val = if k == 0 { 10 } else { k };

        let (script, is_node) = if target == "wiki" {
            (
                r#"
                    ?[id, dist, title, path, summary] :=
                        ~wiki_entities:wiki_vec{id | query: $query_vec, k: $k, ef: 64, bind_distance: dist},
                        *wiki_entities{id, title, path, summary}
                    :order dist
                "#,
                false
            )
        } else {
            (
                r#"
                    ?[id, dist, file_path, kind, name, start_line, end_line] :=
                        ~code_nodes:node_vec{id | query: $query_vec, k: $k, ef: 64, bind_distance: dist},
                        *code_nodes{id, file_path, kind, name, start_line, end_line}
                    :order dist
                "#,
                true
            )
        };

        let mut params = BTreeMap::new();
        params.insert("query_vec".to_string(), DataValue::Vec(cozo::Vector::F32(ndarray::Array1::from(query_vec))));
        params.insert("k".to_string(), DataValue::from(k_val as i64));

        let named_rows = self.db.run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| format!("HNSW vector search failed: {}", e))?;

        let mut matches = Vec::new();
        for row in named_rows.rows {
            if row.len() >= 5 {
                let id = row[0].get_str().unwrap_or("").to_string();
                let dist = row[1].get_float().unwrap_or(1.0);
                let score = (1.0f64 - dist).max(0.0);

                // Relevance floor: reject irrelevant matches (e.g. distance > 0.80 or score < 0.20)
                if score < 0.20 || dist > 0.80 {
                    continue;
                }

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

                matches.push(VectorMatch {
                    id,
                    score,
                    distance: dist,
                    metadata,
                });
            }
        }

        Ok(matches)
    }

    /// Lookup code node metadata for rich graph query results
    pub fn lookup_code_node(&self, node_id: &str) -> (Option<String>, Option<String>, Option<String>, Option<i64>) {
        let script = r#"
            ?[name, kind, file_path, line] := *code_nodes{id: $id, name, kind, file_path, start_line: line}
        "#;
        let mut params = BTreeMap::new();
        params.insert("id".to_string(), DataValue::from(node_id));
        if let Ok(res) = self.db.run_script(script, params, ScriptMutability::Immutable) {
            if let Some(row) = res.rows.into_iter().next() {
                if row.len() >= 4 {
                    let name = row[0].get_str().map(|s| s.to_string());
                    let kind = row[1].get_str().map(|s| s.to_string());
                    let file_path = row[2].get_str().map(|s| s.to_string());
                    let line = row[3].get_int();
                    return (name, kind, file_path, line);
                }
            }
        }
        (None, None, None, None)
    }

    /// Compute PageRank centrality scores over the code dependency graph.
    pub fn pagerank(&self, theta: Option<f64>, iterations: Option<usize>) -> Result<Vec<PageRankResult>> {
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

        let named_rows = self.db.run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| format!("PageRank execution failed: {}", e))?;

        let mut results = Vec::new();
        for row in named_rows.rows {
            if row.len() >= 2 {
                let id = row[0].get_str().unwrap_or("").to_string();
                let rank = row[1].get_float().unwrap_or(0.0);
                let (name, kind, file_path, line) = self.lookup_code_node(&id);
                results.push(PageRankResult {
                    id,
                    rank,
                    name,
                    kind,
                    file_path,
                    line,
                });
            }
        }

        Ok(results)
    }

    /// Find shortest path between two nodes in the code graph using ShortestPathBFS.
    pub fn shortest_path(&self, start_id: &str, target_id: &str) -> Result<Option<Vec<String>>> {
        let script = r#"
            edges[src, dst] := *code_edges{source_id: src, target_id: dst}
            start[] <- [[$start]]
            end[] <- [[$target]]
            ?[source, target, path] <~ ShortestPathBFS(edges[], start[], end[])
        "#;

        let mut params = BTreeMap::new();
        params.insert("start".to_string(), DataValue::from(start_id));
        params.insert("target".to_string(), DataValue::from(target_id));

        let named_rows = self.db.run_script(script, params, ScriptMutability::Immutable)
            .map_err(|e| format!("ShortestPath execution failed: {}", e))?;

        for row in named_rows.rows {
            if row.len() >= 3 {
                if let Some(list) = row[2].get_slice() {
                    let path_nodes: Vec<String> = list.iter()
                        .filter_map(|v| v.get_str().map(|s| s.to_string()))
                        .collect();
                    return Ok(Some(path_nodes));
                }
            }
        }

        Ok(None)
    }

    /// Find shortest path with enriched symbol details (name, kind, file_path, line).
    pub fn shortest_path_detailed(&self, start_id: &str, target_id: &str) -> Result<Option<Vec<PathStep>>> {
        let path_nodes = match self.shortest_path(start_id, target_id)? {
            Some(nodes) => nodes,
            None => return Ok(None),
        };

        let mut steps = Vec::new();
        for id in path_nodes {
            let (name, kind, file_path, line) = self.lookup_code_node(&id);
            steps.push(PathStep {
                id,
                name,
                kind,
                file_path,
                line,
            });
        }
        Ok(Some(steps))
    }
}
