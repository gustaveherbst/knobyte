//! Incremental Cozo synchronisation: unchanged nodes keep their embeddings, changed ones are
//! re-embedded, removed ones are deleted.

use knobyte::cozo::{CozoEngine, Embedder, HashedEmbedder};
use knobyte::graph::GraphEngine;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::tempdir;

struct Counting {
    inner: HashedEmbedder,
    calls: AtomicUsize,
}

impl Embedder for Counting {
    fn id(&self) -> String {
        self.inner.id()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        self.inner.embed(text)
    }
    fn embed_code_symbol(&self, symbol: &knobyte::cozo::embedding::CodeSymbolText) -> Vec<f32> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.embed_fields(&symbol.fields())
    }
}

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn count_nodes(cozo: &CozoEngine) -> usize {
    let v = cozo.run_query("?[count(id)] := *code_nodes{id}", serde_json::json!({})).unwrap();
    v["rows"][0][0].as_u64().unwrap() as usize
}

#[test]
fn cozo_sync_is_incremental() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn alpha() -> u8 { beta() }\npub fn beta() -> u8 { 1 }\n");
    write(root, "src/b.rs", "pub fn gamma() -> u8 { 2 }\n");
    let graph_path = root.join(".knobyte/graph.db");
    let mut graph = GraphEngine::open(&graph_path).unwrap();
    graph.rebuild(root).unwrap();

    let embedder = Arc::new(Counting {
        inner: HashedEmbedder,
        calls: AtomicUsize::new(0),
    });
    let cozo = CozoEngine::open_with_embedder(root.join("cozo.db"), embedder.clone()).unwrap();
    let conn = || rusqlite::Connection::open(&graph_path).unwrap();

    let (nodes, edges) = cozo.sync_from_graph(&conn()).unwrap();
    let first = embedder.calls.swap(0, Ordering::SeqCst);
    assert_eq!(first, nodes, "cold sync embeds every node");
    assert!(edges > 0);
    assert_eq!(count_nodes(&cozo), nodes);

    // Nothing changed: no embedding work, same content.
    cozo.sync_from_graph(&conn()).unwrap();
    assert_eq!(embedder.calls.swap(0, Ordering::SeqCst), 0);
    assert_eq!(count_nodes(&cozo), nodes);

    // One body changes, one file disappears.
    write(root, "src/a.rs", "pub fn alpha() -> u8 { beta() + 1 }\npub fn beta() -> u8 { 1 }\n");
    fs::remove_file(root.join("src/b.rs")).unwrap();
    graph.rebuild(root).unwrap();
    let (after, _) = cozo.sync_from_graph(&conn()).unwrap();
    let reembedded = embedder.calls.swap(0, Ordering::SeqCst);
    assert!(reembedded >= 1 && reembedded < first, "only changed nodes re-embedded: {}", reembedded);
    assert_eq!(count_nodes(&cozo), after, "removed nodes deleted");
    let gamma = cozo
        .run_query("?[id] := *code_nodes{id, name: 'gamma'}", serde_json::json!({}))
        .unwrap();
    assert!(gamma["rows"].as_array().unwrap().is_empty());
}

/// A synthetic graph of `n` declarations whose names, signatures and docs draw on a shared
/// vocabulary (so neighbourhoods overlap the way real code does).
fn synthetic_graph(path: &Path, n: usize) {
    const WORDS: [&str; 48] = [
        "user", "account", "session", "token", "parse", "render", "graph", "node", "edge", "index",
        "query", "cache", "store", "load", "save", "build", "refresh", "publish", "route", "handler",
        "request", "response", "config", "policy", "scope", "rank", "score", "vector", "embed",
        "search", "wiki", "entity", "ground", "drift", "check", "report", "status", "repair",
        "lock", "file", "path", "chunk", "token", "stream", "event", "queue", "worker", "retry",
    ];
    let conn = rusqlite::Connection::open(path).unwrap();
    knobyte::graph::schema::initialize_graph_schema(&conn).unwrap();
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = |m: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed % m as u64) as usize
    };
    let tx = conn.unchecked_transaction().unwrap();
    for i in 0..n {
        let words: Vec<&str> = (0..6).map(|_| WORDS[next(WORDS.len())]).collect();
        let name = format!("{}_{}_{}", words[0], words[1], i);
        let signature = format!("fn {}({}: {}) -> {}", name, words[2], words[3], words[4]);
        let doc = format!("{} the {} for {} {}", words[1], words[2], words[5], words[0]);
        let file = format!("src/{}.rs", words[5]);
        tx.execute(
            "INSERT INTO nodes (id, kind, name, qualified_name, identity_key, file_path, language, start_line, \
             end_line, start_column, end_column, signature, docstring, body_hash, updated_at) \
             VALUES (?1, 'function', ?2, ?2, ?3, ?4, 'rust', 1, 2, 0, 0, ?5, ?6, ?7, 0)",
            rusqlite::params![format!("n{}", i), name, format!("{}:{}", file, name), file, signature, doc, format!("h{}", i)],
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

/// The HNSW index (built with `HNSW_EF_CONSTRUCTION`) must find what an exhaustive scan finds.
#[test]
fn cozo_vector_index_recall_matches_exhaustive_search() {
    let dir = tempdir().unwrap();
    let graph_path = dir.path().join("graph.db");
    synthetic_graph(&graph_path, 1500);
    let cozo = CozoEngine::open_with_embedder(dir.path().join("cozo.db"), Arc::new(HashedEmbedder)).unwrap();
    let conn = rusqlite::Connection::open(&graph_path).unwrap();
    let (nodes, _) = cozo.sync_from_graph(&conn).unwrap();
    assert_eq!(nodes, 1500);

    let queries = [
        "user session token", "parse graph node", "render wiki entity", "refresh publish index",
        "route handler request", "config policy scope", "vector embed search", "drift check report",
        "lock file path", "stream event queue", "retry worker", "account cache store", "rank score",
        "repair status", "ground entity", "chunk token parse", "load save build", "response handler",
        "query index cache", "edge node graph",
    ];
    let k = 10;
    let mut total = 0.0;
    for q in queries {
        let v: Vec<f32> = cozo.embedder().embed(q);
        let params = serde_json::json!({ "q": v, "k": k });
        let ids = |script: &str| -> Vec<String> {
            let r = cozo.run_query(script, params.clone()).unwrap();
            r["rows"].as_array().unwrap().iter().map(|row| row[0].as_str().unwrap().to_string()).collect()
        };
        let exact = ids("?[id, d] := *code_nodes{id, embedding}, d = cos_dist(embedding, vec($q)) :order d :limit $k");
        let approx = ids(
            "?[id, d] := ~code_nodes:node_vec{id | query: vec($q), k: $k, ef: 64, bind_distance: d} :order d",
        );
        let hit = exact.iter().filter(|id| approx.contains(id)).count();
        total += hit as f64 / exact.len().max(1) as f64;
    }
    let recall = total / queries.len() as f64;
    eprintln!("HNSW recall@{} = {:.3}", k, recall);
    assert!(recall >= 0.9, "HNSW recall@{} = {:.3}", k, recall);
}
