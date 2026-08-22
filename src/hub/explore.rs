//! Read models for the Hub pages: hybrid search, symbol workspace (source
//! paging, callers, callees, impact, related knowledge), health, overview
//! (attention / next action) and the shell (counts, capabilities, repo state).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::time::SystemTime;

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    Json,
};
use super::problem::Query;
use serde::Deserialize;
use serde_json::{json, Value};

use super::git::repo_state;
use super::problem::Problem;
use super::HubState;
use crate::config::KnobyteConfig;
use crate::graph::engine::CALL_EDGE_KINDS;
use crate::graph::{inspect_status, GraphEngine, Node};
use crate::mcp::security::resolve_confined_path;
use crate::wiki::index::QueryFilter;
use crate::wiki::WikiIndex;

const RRF_K: f64 = 60.0;
const MAX_FUSED: usize = 200;
const SOURCE_PAGE: usize = 80;

async fn blocking<F>(f: F) -> Response
where
    F: FnOnce() -> Response + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|_| Problem::internal("Hub worker failed").into_response())
}

fn open_graph(config: &KnobyteConfig) -> Option<GraphEngine> {
    let p = config.graph_db_path();
    if !p.exists() {
        return None;
    }
    // One immutable snapshot for the whole request: every query answers from one publication.
    let engine = GraphEngine::open(&p).ok()?;
    let _ = engine.pin_snapshot();
    Some(engine)
}

fn open_wiki(config: &KnobyteConfig) -> Option<WikiIndex> {
    let p = config.wiki_db_path();
    if !p.exists() {
        return None;
    }
    WikiIndex::open_read_only(&p).ok()
}

fn node_json(n: &Node) -> Value {
    json!({
        "id": n.id, "name": n.name, "qualifiedName": n.qualified_name, "kind": n.kind,
        "file": n.file_path, "language": n.language, "startLine": n.start_line, "endLine": n.end_line,
        "signature": n.signature, "docstring": n.docstring, "visibility": n.visibility,
        "isExported": n.is_exported, "isAsync": n.is_async, "returnType": n.return_type,
    })
}

// ---------------------------------------------------------------------------
// Search (FTS + Cozo vector, reciprocal-rank fusion)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    q: String,
    /// `all` | `wiki` | `code`
    scope: Option<String>,
    /// `hybrid` (default) | `fts` | `vector`
    mode: Option<String>,
    /// Wiki entity type filter (comma separated).
    #[serde(rename = "type")]
    entity_type: Option<String>,
    /// Code node kind filter (comma separated).
    kind: Option<String>,
    status: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
}

fn csv(v: &Option<String>) -> Vec<String> {
    v.as_deref()
        .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default()
}

struct Hit {
    key: String,
    item: Value,
    score: f64,
    sources: Vec<&'static str>,
}

fn add_hit(hits: &mut HashMap<String, Hit>, key: String, rank: usize, source: &'static str, item: impl FnOnce() -> Value) {
    let inc = 1.0 / (RRF_K + rank as f64 + 1.0);
    match hits.get_mut(&key) {
        Some(h) => {
            h.score += inc;
            if !h.sources.contains(&source) {
                h.sources.push(source);
            }
        }
        None => {
            hits.insert(key.clone(), Hit { key, item: item(), score: inc, sources: vec![source] });
        }
    }
}

pub fn search_full(config: &KnobyteConfig, q: &SearchQuery) -> Value {
    let text: String = q.q.trim().chars().take(200).collect();
    let scope = q.scope.as_deref().unwrap_or("all");
    let mode = q.mode.as_deref().unwrap_or("hybrid");
    let want_wiki = scope != "code";
    let want_code = scope != "wiki";
    let use_fts = mode != "vector";
    let use_vector = mode != "fts";
    let types = csv(&q.entity_type);
    let kinds = csv(&q.kind);
    let statuses = csv(&q.status);
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let offset = q.offset.unwrap_or(0).min(MAX_FUSED);
    let embedding = crate::cozo::model2vec::embedding_status(&config.embedding);

    let mut hits: HashMap<String, Hit> = HashMap::new();
    let mut notes: Vec<String> = Vec::new();
    let mut vector_state = "skipped".to_string();
    let mut vector_error: Option<String> = None;
    if text.is_empty() {
        return json!({ "query": text, "items": [], "total": 0, "offset": 0, "limit": limit, "nextOffset": null,
            "backend": { "embedding": embedding, "vector": vector_state, "fts": use_fts }, "facets": {} });
    }

    let wiki = if want_wiki { open_wiki(config) } else { None };
    let graph = if want_code { open_graph(config) } else { None };
    let wiki_ok = |t: &str, s: &str| (types.is_empty() || types.iter().any(|x| x == t)) && (statuses.is_empty() || statuses.iter().any(|x| x == s));
    let code_ok = |k: &str| kinds.is_empty() || kinds.iter().any(|x| x == k);

    if use_fts {
        if let Some(w) = &wiki {
            let filter = QueryFilter { types: types.clone(), statuses: statuses.clone(), limit: Some(MAX_FUSED), ..Default::default() };
            match w.search(&text, &filter) {
                Ok(page) => {
                    for (rank, h) in page.items.iter().enumerate() {
                        let e = &h.entity;
                        add_hit(&mut hits, format!("wiki:{}", e.id), rank, "fts", || json!({
                            "kind": "wiki", "id": e.id, "title": e.title, "type": e.entity_type, "status": e.status,
                            "summary": e.summary, "file": e.file, "line": e.start_line, "matched": h.matched,
                        }));
                    }
                }
                Err(e) => notes.push(format!("Wiki search failed: {}", e)),
            }
        } else if want_wiki {
            notes.push("Wiki index not built yet.".into());
        }
        if let Some(g) = &graph {
            let mut seen = HashSet::new();
            let mut ranked: Vec<Node> = Vec::new();
            for n in g.query_where_defined(&text).unwrap_or_default() {
                if seen.insert(n.id.clone()) {
                    ranked.push(n);
                }
            }
            for s in g.query_scope_explained(&text).unwrap_or_default() {
                if ranked.len() >= MAX_FUSED {
                    break;
                }
                if seen.insert(s.node.id.clone()) {
                    ranked.push(s.node);
                }
            }
            for (rank, n) in ranked.iter().filter(|n| code_ok(&n.kind)).enumerate() {
                add_hit(&mut hits, format!("code:{}", n.id), rank, "fts", || {
                    let mut v = node_json(n);
                    v["kind"] = json!("code");
                    v["nodeKind"] = json!(n.kind);
                    v["title"] = json!(n.name);
                    v["line"] = json!(n.start_line);
                    v
                });
            }
        } else if want_code {
            notes.push("Code graph not built yet.".into());
        }
    }

    if use_vector {
        match crate::cozo::CozoEngine::open_configured(config) {
            Ok(engine) => {
                vector_state = "available".into();
                if want_wiki {
                    match engine.vector_search(&text, "wiki", 50) {
                        Ok(ms) => {
                            for (rank, m) in ms.iter().enumerate() {
                                let wiki_entity = wiki.as_ref().and_then(|w| w.show(&m.id).ok().flatten());
                                if let Some(e) = &wiki_entity {
                                    if !wiki_ok(&e.entity_type, &e.status) {
                                        continue;
                                    }
                                } else if !types.is_empty() || !statuses.is_empty() {
                                    continue;
                                }
                                add_hit(&mut hits, format!("wiki:{}", m.id), rank, "vector", || match &wiki_entity {
                                    Some(e) => json!({
                                        "kind": "wiki", "id": e.id, "title": e.title, "type": e.entity_type, "status": e.status,
                                        "summary": e.summary, "file": e.file, "line": e.start_line,
                                    }),
                                    None => json!({
                                        "kind": "wiki", "id": m.id, "title": m.metadata.get("title").cloned().unwrap_or(json!(m.id)),
                                        "summary": m.metadata.get("summary"), "file": m.metadata.get("path"),
                                    }),
                                });
                                if let Some(h) = hits.get_mut(&format!("wiki:{}", m.id)) {
                                    h.item["similarity"] = json!((m.score * 1000.0).round() / 1000.0);
                                }
                            }
                        }
                        Err(e) => vector_error = Some(e.to_string()),
                    }
                }
                if want_code {
                    match engine.vector_search(&text, "code", 50) {
                        Ok(ms) => {
                            for (rank, m) in ms.iter().enumerate() {
                                let k = m.metadata.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                                if !code_ok(k) {
                                    continue;
                                }
                                add_hit(&mut hits, format!("code:{}", m.id), rank, "vector", || json!({
                                    "kind": "code", "id": m.id, "nodeKind": k,
                                    "title": m.metadata.get("name").cloned().unwrap_or(json!(m.id)),
                                    "name": m.metadata.get("name"), "qualifiedName": m.metadata.get("qualified_name"),
                                    "file": m.metadata.get("file_path"), "line": m.metadata.get("start_line"),
                                    "startLine": m.metadata.get("start_line"), "endLine": m.metadata.get("end_line"),
                                }));
                                if let Some(h) = hits.get_mut(&format!("code:{}", m.id)) {
                                    h.item["similarity"] = json!((m.score * 1000.0).round() / 1000.0);
                                }
                            }
                        }
                        Err(e) => vector_error = Some(e.to_string()),
                    }
                }
            }
            Err(e) => {
                vector_state = "unavailable".into();
                vector_error = Some(e.to_string());
            }
        }
    }

    let mut list: Vec<Hit> = hits.into_values().collect();
    list.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(a.key.cmp(&b.key)));
    list.truncate(MAX_FUSED);
    let mut wiki_types: BTreeMap<String, usize> = BTreeMap::new();
    let mut code_kinds: BTreeMap<String, usize> = BTreeMap::new();
    for h in &list {
        if h.item["kind"] == "wiki" {
            *wiki_types.entry(h.item["type"].as_str().unwrap_or("unknown").to_string()).or_default() += 1;
        } else {
            *code_kinds.entry(h.item["nodeKind"].as_str().unwrap_or("unknown").to_string()).or_default() += 1;
        }
    }
    let total = list.len();
    let items: Vec<Value> = list
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|h| {
            let mut v = h.item;
            v["score"] = json!((h.score * 10000.0).round() / 10000.0);
            v["sources"] = json!(h.sources);
            v
        })
        .collect();
    let next = if offset + items.len() < total { Some(offset + items.len()) } else { None };
    json!({
        "query": text, "scope": scope, "mode": mode,
        "items": items, "total": total, "offset": offset, "limit": limit, "nextOffset": next,
        "backend": { "embedding": embedding, "vector": vector_state, "vectorError": vector_error, "fts": use_fts },
        "facets": { "wikiTypes": wiki_types, "codeKinds": code_kinds },
        "notes": notes,
    })
}

pub async fn search(State(state): State<HubState>, Query(q): Query<SearchQuery>) -> Response {
    if q.q.len() > 1000 {
        return Problem::bad_request("Query too long").into_response();
    }
    blocking(move || Json(search_full(&state.config, &q)).into_response()).await
}

// ---------------------------------------------------------------------------
// Symbol workspace
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SymbolQuery {
    id: String,
    from: Option<i64>,
    limit: Option<usize>,
    offset: Option<usize>,
    depth: Option<usize>,
}

fn require_graph(config: &KnobyteConfig) -> Result<GraphEngine, Problem> {
    open_graph(config).ok_or_else(|| Problem::unavailable("Code graph not built yet. Refresh or rebuild it from the Health page."))
}

fn require_node(g: &GraphEngine, id: &str) -> Result<Node, Problem> {
    if id.is_empty() || id.len() > 2048 {
        return Err(Problem::bad_request("Invalid symbol id"));
    }
    resolve_code_node(g, id)
}

/// Look up a code node by graph id, falling back to a readable grounding ref
/// (`function:src/lib.rs:double`) so links built from wiki/relay groundings resolve.
pub(crate) fn resolve_code_node(g: &GraphEngine, id: &str) -> Result<Node, Problem> {
    if let Some(n) = g.get_nodes(&[id.to_string()]).ok().and_then(|mut v| if v.is_empty() { None } else { Some(v.remove(0)) }) {
        return Ok(n);
    }
    match crate::graph::resolve_grounding_ref(g.connection(), id) {
        Ok(crate::graph::grounding::RefResolution::Resolved(n)) => Ok(*n),
        Ok(crate::graph::grounding::RefResolution::Ambiguous(c)) => Err(Problem::conflict(format!(
            "Reference '{}' matches {} code nodes; use a graph id",
            id,
            c.len()
        ))
        .with_extra(json!({ "candidates": c.iter().take(10).map(|n| json!({ "id": n.id, "kind": n.kind, "qualifiedName": n.qualified_name })).collect::<Vec<_>>() }))),
        _ => Err(Problem::not_found(format!("Symbol '{}' not found in the code graph", id))),
    }
}

/// Lines `[from, from+limit)` of the node's declaration (1-based), confined to the project.
fn source_page(config: &KnobyteConfig, n: &Node, from: i64, limit: usize) -> Value {
    let start = n.start_line.max(1);
    let end = n.end_line.max(start);
    let from = from.clamp(start, end);
    let Ok(path) = resolve_confined_path(&config.project_root, &n.file_path) else {
        return json!({ "available": false, "lines": [], "nextLine": null });
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return json!({ "available": false, "lines": [], "nextLine": null });
    };
    let last = (from + limit as i64 - 1).min(end);
    let lines: Vec<Value> = content
        .lines()
        .enumerate()
        .skip((from - 1) as usize)
        .take((last - from + 1).max(0) as usize)
        .map(|(i, l)| json!({ "n": i + 1, "text": l }))
        .collect();
    let next = if last < end { Some(last + 1) } else { None };
    json!({ "available": true, "lines": lines, "nextLine": next, "startLine": start, "endLine": end, "totalLines": end - start + 1 })
}

fn edges_of(g: &GraphEngine, id: &str, incoming: bool) -> Vec<(String, String, Option<i64>)> {
    let kinds = CALL_EDGE_KINDS.iter().map(|k| format!("'{}'", k)).collect::<Vec<_>>().join(", ");
    let sql = if incoming {
        format!("SELECT source, kind, line FROM edges WHERE target = ?1 AND kind IN ({}) ORDER BY source", kinds)
    } else {
        format!("SELECT target, kind, line FROM edges WHERE source = ?1 AND kind IN ({}) ORDER BY target", kinds)
    };
    let Ok(mut stmt) = g.connection().prepare(&sql) else { return Vec::new() };
    let rows = stmt.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?)));
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if let Ok(rows) = rows {
        for (other, kind, line) in rows.flatten() {
            if other != id && seen.insert(other.clone()) {
                out.push((other, kind, line));
            }
        }
    }
    out
}

fn related_page(g: &GraphEngine, id: &str, incoming: bool, offset: usize, limit: usize) -> Value {
    let all = edges_of(g, id, incoming);
    let total = all.len();
    let slice: Vec<(String, String, Option<i64>)> = all.into_iter().skip(offset).take(limit).collect();
    let ids: Vec<String> = slice.iter().map(|(i, _, _)| i.clone()).collect();
    let nodes: HashMap<String, Node> = g.get_nodes(&ids).unwrap_or_default().into_iter().map(|n| (n.id.clone(), n)).collect();
    let items: Vec<Value> = slice
        .iter()
        .map(|(i, kind, line)| {
            let mut v = nodes.get(i).map(node_json).unwrap_or_else(|| json!({ "id": i, "name": i, "unresolved": true }));
            v["via"] = json!(kind);
            v["callLine"] = json!(line);
            v
        })
        .collect();
    let next = if offset + items.len() < total { Some(offset + items.len()) } else { None };
    json!({ "items": items, "total": total, "offset": offset, "nextOffset": next })
}

fn entity_summary(e: &crate::wiki::WikiEntity) -> Value {
    json!({ "id": e.id, "title": e.title, "type": e.entity_type, "status": e.status, "summary": e.summary, "file": e.file })
}

pub async fn symbol(State(state): State<HubState>, Query(q): Query<SymbolQuery>) -> Response {
    blocking(move || {
        let config = &state.config;
        let g = match require_graph(config) {
            Ok(g) => g,
            Err(p) => return p.into_response(),
        };
        let n = match require_node(&g, &q.id) {
            Ok(n) => n,
            Err(p) => return p.into_response(),
        };
        let knowledge: Vec<Value> = open_wiki(config)
            .and_then(|w| w.for_code(&n.id).ok())
            .unwrap_or_default()
            .iter()
            .map(entity_summary)
            .collect();
        let container = n.container_id.as_ref().and_then(|c| g.get_nodes(std::slice::from_ref(c)).ok()).and_then(|v| v.into_iter().next());
        Json(json!({
            "node": node_json(&n),
            "container": container.as_ref().map(node_json),
            "source": source_page(config, &n, n.start_line, SOURCE_PAGE),
            "callers": related_page(&g, &n.id, true, 0, 10),
            "callees": related_page(&g, &n.id, false, 0, 10),
            "knowledge": knowledge,
        }))
        .into_response()
    })
    .await
}

pub async fn symbol_source(State(state): State<HubState>, Query(q): Query<SymbolQuery>) -> Response {
    blocking(move || {
        let g = match require_graph(&state.config) {
            Ok(g) => g,
            Err(p) => return p.into_response(),
        };
        match require_node(&g, &q.id) {
            Ok(n) => Json(source_page(&state.config, &n, q.from.unwrap_or(n.start_line), q.limit.unwrap_or(SOURCE_PAGE).clamp(1, 400))).into_response(),
            Err(p) => p.into_response(),
        }
    })
    .await
}

async fn relations(state: HubState, q: SymbolQuery, incoming: bool) -> Response {
    blocking(move || {
        let g = match require_graph(&state.config) {
            Ok(g) => g,
            Err(p) => return p.into_response(),
        };
        match require_node(&g, &q.id) {
            Ok(n) => Json(related_page(&g, &n.id, incoming, q.offset.unwrap_or(0), q.limit.unwrap_or(25).clamp(1, 100))).into_response(),
            Err(p) => p.into_response(),
        }
    })
    .await
}

pub async fn symbol_callers(State(state): State<HubState>, Query(q): Query<SymbolQuery>) -> Response {
    relations(state, q, true).await
}

pub async fn symbol_callees(State(state): State<HubState>, Query(q): Query<SymbolQuery>) -> Response {
    relations(state, q, false).await
}

pub async fn symbol_impact(State(state): State<HubState>, Query(q): Query<SymbolQuery>) -> Response {
    blocking(move || {
        let config = &state.config;
        let g = match require_graph(config) {
            Ok(g) => g,
            Err(p) => return p.into_response(),
        };
        let n = match require_node(&g, &q.id) {
            Ok(n) => n,
            Err(p) => return p.into_response(),
        };
        let opts = crate::graph::ImpactOptions { depth: q.depth.unwrap_or(3).clamp(1, 8), callers_only: false };
        let mut report = None;
        for target in [n.id.as_str(), n.qualified_name.as_str(), n.name.as_str()] {
            if target.is_empty() {
                continue;
            }
            if let Ok(r) = g.impact(target, opts) {
                if r.roots.iter().any(|x| x.id == n.id) {
                    report = Some(r);
                    break;
                }
            }
        }
        let Some(report) = report else {
            return Json(json!({ "items": [], "total": 0, "truncated": false, "groundings": [] })).into_response();
        };
        let ids: HashSet<String> = std::iter::once(n.id.clone()).chain(report.impacted.iter().map(|e| e.node.id.clone())).collect();
        let groundings = g.groundings_for(&config.scaffold_root, &ids);
        let items: Vec<Value> = report
            .impacted
            .iter()
            .filter(|e| e.root == n.id)
            .take(200)
            .map(|e| {
                let mut v = node_json(&e.node);
                v["depth"] = json!(e.depth);
                v["via"] = json!(e.via);
                v
            })
            .collect();
        Json(json!({ "items": items, "total": items.len(), "truncated": report.truncated, "groundings": groundings })).into_response()
    })
    .await
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

fn newest_markdown_mtime(dir: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    for e in walkdir::WalkDir::new(dir).max_depth(6).into_iter().flatten() {
        let p = e.path();
        if p.components().any(|c| c.as_os_str() == "local") {
            continue;
        }
        if p.extension().is_some_and(|x| x == "md") {
            if let Some(m) = e.metadata().ok().and_then(|m| m.modified().ok()) {
                newest = Some(newest.map_or(m, |n| n.max(m)));
            }
        }
    }
    newest
}

/// Wiki index health, from the index's recorded corpus (per-file content hashes and config
/// digest) compared with the Markdown on disk. Opening the index read-only never changes it,
/// so this cannot be fooled by the database file's mtime.
pub fn wiki_health(config: &KnobyteConfig) -> Value {
    let db = config.wiki_db_path();
    if !db.exists() {
        return json!({ "status": "missing", "service": "unavailable", "entities": 0, "recommendedJob": "wiki_rebuild",
            "detail": "The wiki index has not been built yet." });
    }
    let index = match WikiIndex::open_read_only(&db) {
        Ok(i) => i,
        Err(e) => {
            let code = crate::wiki::index::error_code(&e).unwrap_or("WIKI_INDEX_REBUILD_REQUIRED").to_string();
            let missing = code == "WIKI_INDEX_MISSING";
            return json!({ "status": if missing { "missing" } else { "corrupt" }, "service": "unavailable", "entities": 0,
                "recommendedJob": "wiki_rebuild", "code": code,
                "detail": if missing { "The wiki index has not been built yet.".to_string() } else { format!("The wiki index cannot be used: {}", e) } });
        }
    };
    let entities = index.entity_count().unwrap_or(0);
    let fresh = match index.freshness(&config.scaffold_root) {
        Ok(f) => f,
        Err(e) => {
            return json!({ "status": "corrupt", "service": "unavailable", "entities": entities, "recommendedJob": "wiki_rebuild",
                "detail": format!("The wiki index could not be read: {}", e) });
        }
    };
    let indexed_at: Option<String> = index
        .connection()
        .query_row("SELECT value FROM wiki_meta WHERE key = 'last_refresh'", [], |r| r.get(0))
        .ok();
    let newest = newest_markdown_mtime(&config.scaffold_root);
    let fmt = |t: Option<SystemTime>| t.map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339());
    let stale = fresh.stale;
    let detail = if fresh.limit_exceeded {
        "The wiki corpus exceeds a safety bound; the index keeps its last complete state. Exclude generated Markdown with `wiki.exclude`.".to_string()
    } else if !fresh.built {
        "The wiki index has never been refreshed.".to_string()
    } else if stale {
        let mut parts = Vec::new();
        if !fresh.added.is_empty() { parts.push(format!("{} new", fresh.added.len())); }
        if !fresh.changed.is_empty() { parts.push(format!("{} changed", fresh.changed.len())); }
        if !fresh.removed.is_empty() { parts.push(format!("{} removed", fresh.removed.len())); }
        if fresh.config_changed { parts.push("wiki config changed".to_string()); }
        format!("Scaffold Markdown differs from the index ({}); a refresh is recommended.", parts.join(", "))
    } else {
        "The wiki index matches the scaffold.".to_string()
    };
    json!({
        "status": if stale { "stale" } else { "fresh" },
        "service": if stale || fresh.limit_exceeded { "degraded" } else { "healthy" },
        "entities": entities,
        "indexedAt": indexed_at,
        "newestSourceAt": fmt(newest),
        "recommendedJob": if stale { Some("wiki_refresh") } else { None },
        "changes": { "added": fresh.added.len(), "changed": fresh.changed.len(), "removed": fresh.removed.len(), "configChanged": fresh.config_changed },
        "limitExceeded": fresh.limit_exceeded,
        "detail": detail,
    })
}

/// Bring the wiki index up to date after the Hub itself wrote scaffold Markdown (an inbox
/// approval), so the new entity is reachable at once. Never resets an index from another
/// schema; any failure is left for Health to report (it then recommends a refresh).
pub fn refresh_wiki_after_write(config: &KnobyteConfig) -> Option<String> {
    let db = config.wiki_db_path();
    if !db.exists() {
        return Some("wiki index not built".into());
    }
    match WikiIndex::open(&db).and_then(|mut i| i.refresh(&config.scaffold_root)) {
        Ok(_) => None,
        Err(e) => Some(e.to_string()),
    }
}

fn graph_service(status: &str) -> &'static str {
    match status {
        "fresh" => "healthy",
        "stale" | "degraded" => "degraded",
        _ => "unavailable",
    }
}

/// Hub-recorded snapshot of the repository when the graph was last published by a Hub job.
pub fn recorded_index_snapshot(config: &KnobyteConfig) -> Option<Value> {
    let text = std::fs::read_to_string(config.local_dir().join("hub").join("graph-index.json")).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn record_index_snapshot(config: &KnobyteConfig) {
    let repo = repo_state(&config.project_root);
    let v = json!({ "branch": repo.branch, "head": repo.head, "recordedAt": chrono::Utc::now().to_rfc3339() });
    let dir = config.local_dir().join("hub");
    if config.scaffold_root.is_dir() && std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(dir.join("graph-index.json"), v.to_string());
    }
}

pub fn health_report(state: &HubState) -> Value {
    let config = &state.config;
    let repo = repo_state(&config.project_root);
    let graph = inspect_status(&config.graph_db_path(), &config.project_root);
    let wiki = wiki_health(config);
    let embedding = crate::cozo::model2vec::embedding_status(&config.embedding);
    let cozo_exists = config.cozo_db_path().exists();
    let active = state.jobs.active();
    let indexed = recorded_index_snapshot(config);
    let head_moved = indexed
        .as_ref()
        .and_then(|i| i["head"].as_str().map(|h| Some(h) != repo.head.as_deref()))
        .unwrap_or(false);
    let graph_rec = match graph.status.as_str() {
        "fresh" => None,
        "stale" | "degraded" => Some("graph_refresh"),
        _ => Some("graph_rebuild"),
    };
    let git_service = if !repo.available { "unavailable" } else { "healthy" };
    let cozo_service = if !embedding.model_present {
        "degraded"
    } else if cozo_exists {
        "healthy"
    } else {
        "degraded"
    };
    let services = vec![
        json!({
            "id": "git", "title": "Git repository", "status": git_service,
            "detail": if repo.available { "Branch and working tree are readable." } else { "No readable git repository; drift history and commits are unavailable." },
            "repo": repo,
        }),
        json!({
            "id": "graph", "title": "Code graph", "status": graph_service(&graph.status),
            "detail": match graph.status.as_str() {
                "fresh" => "The code graph matches the working tree.",
                "stale" => "Source files changed since the last index; a bounded refresh is recommended.",
                "degraded" => "Some files failed to parse; the graph is usable but incomplete.",
                "missing" => "The code graph has not been built yet.",
                _ => "The code graph cannot be used safely; rebuild it.",
            },
            "graph": graph,
            "indexedSnapshot": indexed,
            "headMoved": head_moved,
            "recommendedJob": graph_rec,
        }),
        json!({ "id": "wiki", "title": "Project wiki", "status": wiki["service"], "detail": wiki["detail"], "wiki": wiki, "recommendedJob": wiki["recommendedJob"] }),
        json!({
            "id": "cozo", "title": "Cozo & embeddings", "status": cozo_service,
            "detail": if !embedding.model_present {
                format!("The {} model is not downloaded; run `knobyte cozo model pull`.", embedding.model.clone().unwrap_or_default())
            } else if cozo_exists {
                format!("Vector search uses {} embeddings.", embedding.backend)
            } else {
                "CozoDB has not been synchronized yet.".to_string()
            },
            "embedding": embedding, "dbExists": cozo_exists, "recommendedJob": if cozo_exists { None } else { Some("cozo_sync") },
        }),
        json!({
            "id": "hub", "title": "Local Hub", "status": "healthy",
            "detail": "Sessions, jobs and settings are process-local; shared records use git.",
            "version": crate::version::VERSION, "bind": state.bind_addr.as_str(), "activeJob": active,
        }),
    ];
    let overall = if services.iter().any(|s| s["status"] == "unavailable") {
        "unavailable"
    } else if services.iter().any(|s| s["status"] == "degraded") {
        "degraded"
    } else {
        "healthy"
    };
    json!({ "checkedAt": chrono::Utc::now().to_rfc3339(), "overall": overall, "services": services, "activeJob": state.jobs.active() })
}

pub async fn health(State(state): State<HubState>) -> Response {
    blocking(move || Json(health_report(&state)).into_response()).await
}

// ---------------------------------------------------------------------------
// Overview (attention / next action) and shell
// ---------------------------------------------------------------------------

fn counts(config: &KnobyteConfig) -> Value {
    use crate::team::inbox::{list_inbox_drafts, list_inbox_proposals, PROPOSAL_STATUS_PENDING};
    let me = crate::team::resolve_actor(config).actor.member_id().map(str::to_string);
    let proposals = list_inbox_proposals(config);
    let relays = crate::team::relay::list_relays(config);
    let for_me = relays
        .iter()
        .filter(|r| r.status == "published" && me.as_deref().is_some_and(|m| r.is_team() || r.named_recipients.iter().any(|x| x == m) ) && me.as_deref() != Some(r.sender.as_str()))
        .count();
    json!({
        "inboxPending": proposals.iter().filter(|p| p.status == PROPOSAL_STATUS_PENDING).count(),
        "inboxStale": proposals.iter().filter(|p| p.status == "stale").count(),
        "inboxDrafts": list_inbox_drafts(config).len(),
        "relaysOpen": relays.iter().filter(|r| r.status != "closed").count(),
        "relaysForMe": for_me,
        "relayDrafts": crate::team::relay::list_relay_drafts(config).len(),
        "members": crate::team::members::list_members(config).iter().filter(|m| m.is_active()).count(),
        "workstreams": crate::team::workstreams::list_workstreams(config).iter().filter(|w| w.status != "archived").count(),
        "specs": crate::team::specs::list_specs(config).len(),
    })
}

pub async fn shell(State(state): State<HubState>) -> Response {
    blocking(move || {
        let config = &state.config;
        let scaffold = config.scaffold_root.is_dir();
        let res = crate::team::resolve_actor(config);
        let member = res.actor.member_id().and_then(|id| crate::team::members::get_member(config, id)).filter(|m| m.is_active());
        let graph = config.graph_db_path().exists();
        let wiki = config.wiki_db_path().exists();
        let embedding = crate::cozo::model2vec::embedding_status(&config.embedding);
        Json(json!({
            "product": "knobyte",
            "version": crate::version::VERSION,
            "repo": config.project_name(),
            "projectRoot": config.project_root,
            "bind": state.bind_addr.as_str(),
            "git": repo_state(&config.project_root),
            "counts": if scaffold { counts(config) } else { json!({}) },
            "actor": { "id": res.actor.id(), "source": res.source, "member": member.map(|m| json!({ "id": m.id, "displayName": m.display_name, "role": m.role })) },
            "capabilities": {
                "scaffold": scaffold,
                "graph": graph,
                "wiki": wiki,
                "vector": config.cozo_db_path().exists() && embedding.model_present,
                "embeddingBackend": embedding.backend,
                "team": scaffold,
                "member": member_present(config),
            },
            "activeJob": state.jobs.active(),
            "auth": { "accessTokenConfigured": state.security.access_token.is_some() },
        }))
        .into_response()
    })
    .await
}

fn member_present(config: &KnobyteConfig) -> bool {
    crate::team::resolve_actor(config)
        .actor
        .member_id()
        .and_then(|id| crate::team::members::get_member(config, id))
        .is_some_and(|m| m.is_active())
}

pub async fn home(State(state): State<HubState>) -> Response {
    blocking(move || {
        let config = &state.config;
        let setup = super::setup_wizard::setup_status(config, &state.setup);
        let stage = setup["stage"].as_str().unwrap_or("ready").to_string();
        let mut attention: Vec<Value> = Vec::new();
        let push = |a: &mut Vec<Value>, kind: &str, title: String, detail: String, href: String, action: &str| {
            a.push(json!({ "kind": kind, "title": title, "detail": detail, "href": href, "action": action }));
        };
        if !matches!(stage.as_str(), "ready" | "complete") {
            push(&mut attention, "setup", "Finish setting up project memory".into(),
                format!("Setup stage: {}.", stage.replace('_', " ")), "/setup".into(), "Open setup");
        }
        let scaffold = config.scaffold_root.is_dir();
        let me = crate::team::resolve_actor(config).actor.member_id().map(str::to_string);
        if scaffold {
            let relays = crate::team::relay::list_relays(config);
            if let Some(me) = &me {
                for r in relays.iter().filter(|r| r.status == "published" && &r.sender != me && (r.is_team() || r.named_recipients.iter().any(|x| x == me))).take(3) {
                    push(&mut attention, "relay", "Take the handoff waiting for you".into(), r.title.clone(), format!("/relays/{}", r.id), "Open handoff");
                }
                for r in relays.iter().filter(|r| r.status == "acknowledged" && r.claimed_by() == Some(me.as_str())).take(3) {
                    push(&mut attention, "relay", "Continue the handoff you took".into(), r.title.clone(), format!("/relays/{}", r.id), "Continue");
                }
            } else {
                push(&mut attention, "identity", "Choose who you are working as".into(),
                    "No active member is selected for this checkout, so reviews and handoffs cannot be attributed.".into(), "/members".into(), "Open team");
            }
            let proposals = crate::team::inbox::list_inbox_proposals(config);
            let pending: Vec<_> = proposals.iter().filter(|p| p.status == "pending" && !me.as_deref().is_some_and(|m| p.is_contributor(m))).collect();
            if !pending.is_empty() {
                push(&mut attention, "inbox", format!("Review {} knowledge proposal{}", pending.len(), if pending.len() == 1 { "" } else { "s" }),
                    pending[0].title.clone(), "/inbox".into(), "Review");
            }
            for p in proposals.iter().filter(|p| p.status == "stale" && me.as_deref().is_some_and(|m| p.is_contributor(m))).take(2) {
                push(&mut attention, "inbox", "Repair a stale proposal".into(), p.title.clone(), format!("/inbox/{}", p.id), "Repair");
            }
        }
        let graph = inspect_status(&config.graph_db_path(), &config.project_root);
        if scaffold && graph.status != "fresh" {
            push(&mut attention, "health", if graph.status == "missing" { "Build the code graph".into() } else { "Review local context health".into() },
                format!("The code graph is {}.", graph.status.replace('_', " ")), "/health".into(), "Open health");
        }
        let wiki = wiki_health(config);
        if scaffold && wiki["status"] != "fresh" {
            push(&mut attention, "health", "Refresh the wiki index".into(), wiki["detail"].as_str().unwrap_or("").to_string(), "/health".into(), "Open health");
        }
        let drift = if scaffold { Some(crate::drift::checker::run_drift_check(config)) } else { None };
        if let Some(d) = &drift {
            if d.issue_count > 0 {
                push(&mut attention, "drift", format!("{} drift issue{} in the scaffold", d.issue_count, if d.issue_count == 1 { "" } else { "s" }),
                    format!("Drift score {:.1}.", d.score), "/groundings".into(), "Review drift");
            }
        }
        let repo = repo_state(&config.project_root);
        let mut memory: Vec<Value> = Vec::new();
        if scaffold {
            for ev in crate::events::read_events(config).iter().rev().take(6) {
                memory.push(json!({ "source": "event", "timestamp": ev.timestamp, "actor": ev.actor, "kind": ev.kind, "summary": ev.summary, "files": ev.files }));
            }
            for a in crate::team::activity::list_activity(config, 6) {
                memory.push(json!({ "source": "activity", "timestamp": a.timestamp, "actor": a.actor, "kind": a.action, "summary": a.summary, "entity": a.entity_title }));
            }
            memory.sort_by(|a, b| b["timestamp"].as_str().unwrap_or("").cmp(a["timestamp"].as_str().unwrap_or("")));
            memory.truncate(6);
        }
        Json(json!({
            "repo": repo,
            "setupStage": stage,
            "nextAction": attention.first().cloned(),
            "attention": attention,
            "readiness": {
                "graph": { "status": graph.status, "counts": graph.counts, "parseHealth": graph.parse_health, "changes": graph.changes.total, "lastIndexedAt": graph.last_successful_index_at },
                "wiki": wiki,
                "drift": drift.as_ref().map(|d| json!({ "score": d.score, "status": d.status, "issueCount": d.issue_count, "grounding": d.grounding })),
                "indexedSnapshot": recorded_index_snapshot(config),
            },
            "latestMemory": memory,
            "activeJob": state.jobs.active(),
            "counts": if scaffold { counts(config) } else { json!({}) },
        }))
        .into_response()
    })
    .await
}
