use knobyte::cozo::{CozoEngine, HashedEmbedder};

#[test]
fn test_embedder_deterministic_and_normalized() {
    let text = "fn calculate_hash(data: &[u8]) -> u64";
    let v1 = HashedEmbedder::embed_text(text);
    let v2 = HashedEmbedder::embed_text(text);
    assert_eq!(v1, v2);
    assert_eq!(v1.len(), 128);

    let norm: f32 = v1.iter().map(|&x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-4);
}

#[test]
fn test_cozo_engine_init_and_query() {
    let engine = CozoEngine::in_memory().expect("in-memory CozoEngine should initialize");

    // Run simple Datalog query
    let res = engine
        .datalog_query("?[a, b] <- [['hello', 42]]", serde_json::json!({}))
        .expect("Datalog query should succeed");

    assert_eq!(res["headers"], serde_json::json!(["a", "b"]));
    assert_eq!(res["rows"], serde_json::json!([["hello", 42]]));
}

#[test]
fn test_cozo_vector_search_and_graph_algos() {
    let engine = CozoEngine::in_memory().expect("in-memory CozoEngine should initialize");

    // Insert sample nodes with embeddings into code_nodes
    let sample_nodes = vec![
        (
            "fn:auth_login",
            "src/auth.rs",
            "function",
            "login",
            10,
            30,
            "hash1",
            "fn login(user: &str, pass: &str) -> Result<Session>",
        ),
        (
            "fn:auth_logout",
            "src/auth.rs",
            "function",
            "logout",
            32,
            45,
            "hash2",
            "fn logout(session: &Session) -> Result<()>",
        ),
        (
            "fn:render_canvas",
            "src/ui/canvas.rs",
            "function",
            "render_canvas",
            5,
            50,
            "hash3",
            "fn render_canvas(ctx: &Context, width: u32, height: u32)",
        ),
    ];

    let mut tuples = Vec::new();
    for (id, file_path, kind, name, start, end, body_hash, snippet) in sample_nodes {
        let embedding = HashedEmbedder::embed_text(snippet);
        tuples.push(serde_json::json!([
            id, file_path, kind, name, start, end, body_hash, embedding
        ]));
    }

    let put_nodes = r#"
        ?[id, file_path, kind, name, start_line, end_line, body_hash, embedding] <- $data
        :put code_nodes { id => file_path, kind, name, start_line, end_line, body_hash, embedding }
    "#;
    engine
        .datalog_query_mutable(put_nodes, serde_json::json!({ "data": tuples }))
        .expect("Inserting nodes should succeed");

    // Insert sample edges
    let sample_edges = vec![
        ("fn:auth_login", "fn:auth_logout", "calls", "src/auth.rs"),
        ("fn:auth_login", "fn:render_canvas", "calls", "src/auth.rs"),
    ];
    let mut edge_tuples = Vec::new();
    for (src, dst, kind, fp) in sample_edges {
        edge_tuples.push(serde_json::json!([src, dst, kind, fp]));
    }
    let put_edges = r#"
        ?[source_id, target_id, kind, file_path] <- $data
        :put code_edges { source_id, target_id, kind => file_path }
    "#;
    engine
        .datalog_query_mutable(put_edges, serde_json::json!({ "data": edge_tuples }))
        .expect("Inserting edges should succeed");

    // 1. Test Vector Search: Search for "login user password"
    let matches = engine
        .vector_search("login user password", "code", 2)
        .expect("Vector search should succeed");

    assert!(!matches.is_empty(), "Vector search should return matches");
    assert_eq!(
        matches[0].id, "fn:auth_login",
        "Expected login function to match login user password query"
    );

    // 2. Test PageRank
    let ranks = engine
        .pagerank(Some(0.85), Some(20))
        .expect("PageRank should execute");
    assert!(!ranks.is_empty(), "PageRank should return scored nodes");

    // 3. Test Shortest Path
    let path = engine
        .shortest_path("fn:auth_login", "fn:render_canvas")
        .expect("ShortestPath should execute");
    assert!(
        path.is_some(),
        "Shortest path should be found between login and render_canvas"
    );
    let path_nodes = path.unwrap();
    assert_eq!(path_nodes, vec!["fn:auth_login", "fn:render_canvas"]);
}

#[test]
fn test_ensure_schema_migrates_old_code_nodes_layout() {
    let engine = CozoEngine::in_memory().unwrap();
    // Recreate the pre-qualified_name layout.
    engine
        .datalog_query_mutable("::hnsw drop code_nodes:node_vec", serde_json::json!({}))
        .unwrap();
    engine
        .datalog_query_mutable("::remove code_nodes", serde_json::json!({}))
        .unwrap();
    engine
        .datalog_query_mutable(
            ":create code_nodes { id: String => file_path: String, kind: String, name: String, start_line: Int, end_line: Int, body_hash: String, embedding: <F32; 128> }",
            serde_json::json!({}),
        )
        .unwrap();

    engine.ensure_schema().expect("migration should succeed");
    let cols = engine
        .datalog_query("::columns code_nodes", serde_json::json!({}))
        .unwrap();
    assert!(cols["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r[0] == "qualified_name"));
    // Idempotent.
    engine.ensure_schema().unwrap();
}

#[test]
fn test_cozo_query_rejects_writes() {
    let engine = CozoEngine::in_memory().unwrap();
    let write = r#"
        ?[source_id, target_id, kind, file_path] <- [['a', 'b', 'calls', 'x.rs']]
        :put code_edges { source_id, target_id, kind => file_path }
    "#;
    assert!(
        engine.datalog_query(write, serde_json::json!({})).is_err(),
        "the default query entry point must be read-only"
    );
    assert!(engine
        .datalog_query("::remove code_edges", serde_json::json!({}))
        .is_err());

    // Nothing was written.
    let rows = engine
        .datalog_query("?[s] := *code_edges{source_id: s}", serde_json::json!({}))
        .unwrap();
    assert_eq!(rows["rows"], serde_json::json!([]));

    // Explicit opt-in still works.
    engine
        .datalog_query_mutable(write, serde_json::json!({}))
        .unwrap();
    let rows = engine
        .datalog_query("?[s] := *code_edges{source_id: s}", serde_json::json!({}))
        .unwrap();
    assert_eq!(rows["rows"], serde_json::json!([["a"]]));
}

fn cozo_ids(engine: &CozoEngine, rel: &str) -> Vec<String> {
    let res = engine
        .datalog_query(&format!("?[id] := *{}{{id}}", rel), serde_json::json!({}))
        .unwrap();
    res["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[0].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn test_cozo_sync_replaces_deleted_code_and_wiki() {
    use knobyte::graph::GraphEngine;
    use knobyte::wiki::WikiIndex;
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/keep.rs"), "pub fn keep_me() {}\n").unwrap();
    fs::write(
        root.join("src/gone.rs"),
        "pub fn delete_me() { keep_me(); }\n",
    )
    .unwrap();
    let docs = root.join("docs");
    fs::create_dir_all(&docs).unwrap();
    fs::write(docs.join("a.md"), "---\nid: kb_a\n---\n# A\n").unwrap();
    fs::write(docs.join("b.md"), "---\nid: kb_b\n---\n# B\n").unwrap();

    let graph_path = root.join("graph.db");
    let wiki_path = root.join("wiki.db");
    let mut graph = GraphEngine::open(&graph_path).unwrap();
    graph.rebuild(root).unwrap();
    let mut wiki = WikiIndex::open(&wiki_path).unwrap();
    wiki.rebuild(&docs).unwrap();

    let cozo = CozoEngine::in_memory().unwrap();
    let gconn = rusqlite::Connection::open(&graph_path).unwrap();
    let wconn = rusqlite::Connection::open(&wiki_path).unwrap();
    cozo.sync_from_graph(&gconn).unwrap();
    cozo.sync_from_wiki(&wconn).unwrap();

    let delete_id = graph.query_where_defined("delete_me").unwrap()[0]
        .id
        .clone();
    assert!(cozo_ids(&cozo, "code_nodes").contains(&delete_id));
    assert!(cozo_ids(&cozo, "wiki_entities").contains(&"kb_b".to_string()));
    let edges_before = cozo
        .datalog_query(
            "?[s, t] := *code_edges{source_id: s, target_id: t}",
            serde_json::json!({}),
        )
        .unwrap();
    assert!(edges_before["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r[0] == delete_id.as_str()));

    // Delete code and a wiki page, rebuild, re-sync.
    fs::remove_file(root.join("src/gone.rs")).unwrap();
    fs::remove_file(docs.join("b.md")).unwrap();
    graph.rebuild(root).unwrap();
    wiki.rebuild(&docs).unwrap();
    cozo.sync_from_graph(&gconn).unwrap();
    cozo.sync_from_wiki(&wconn).unwrap();

    let ids = cozo_ids(&cozo, "code_nodes");
    assert!(
        !ids.contains(&delete_id),
        "deleted code must disappear from Cozo"
    );
    assert!(ids.contains(&graph.query_where_defined("keep_me").unwrap()[0].id));
    assert_eq!(cozo_ids(&cozo, "wiki_entities"), vec!["kb_a".to_string()]);
    let edges_after = cozo
        .datalog_query(
            "?[s, t] := *code_edges{source_id: s, target_id: t}",
            serde_json::json!({}),
        )
        .unwrap();
    assert!(!edges_after["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r[0] == delete_id.as_str()));

    // Search no longer returns the deleted symbol.
    let matches = cozo.vector_search("delete me", "code", 10).unwrap();
    assert!(matches.iter().all(|m| m.id != delete_id));
}

#[test]
fn test_shortest_path_accepts_names_and_readable_refs() {
    use knobyte::graph::GraphEngine;
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn login() { verify(); }\nfn verify() { hash_password(); }\nfn hash_password() {}\n\
         pub struct A;\nimpl A { pub fn new() -> Self { A } }\npub struct B;\nimpl B { pub fn new() -> Self { B } }\n",
    )
    .unwrap();
    let graph_path = root.join("graph.db");
    let mut graph = GraphEngine::open(&graph_path).unwrap();
    graph.rebuild(root).unwrap();

    let cozo = CozoEngine::in_memory().unwrap();
    cozo.sync_from_graph(&rusqlite::Connection::open(&graph_path).unwrap())
        .unwrap();

    let path = cozo
        .shortest_path("login", "hash_password")
        .unwrap()
        .expect("path");
    assert_eq!(path.len(), 3);

    let path2 = cozo
        .shortest_path("function:src/lib.rs:login", "function:src/lib.rs:verify")
        .unwrap()
        .expect("path");
    assert_eq!(path2.len(), 2);

    let err = cozo.shortest_path("login", "new").unwrap_err().to_string();
    assert!(err.contains("ambiguous"), "{}", err);
    assert!(cozo.resolve_node_ref("method:src/lib.rs:B::new").is_ok());
    assert!(cozo.resolve_node_ref("does_not_exist").is_err());
}

/// Insert code nodes `(id, kind, embedded text)` into an in-memory engine.
fn engine_with_nodes(nodes: &[(String, &str, String)]) -> CozoEngine {
    let engine = CozoEngine::in_memory().unwrap();
    let rows: Vec<serde_json::Value> = nodes
        .iter()
        .map(|(id, kind, text)| {
            serde_json::json!([id, "src/lib.rs", kind, id, 1, 2, "h", HashedEmbedder::embed_text(text)])
        })
        .collect();
    engine
        .datalog_query_mutable(
            "?[id, file_path, kind, name, start_line, end_line, body_hash, embedding] <- $data
             :put code_nodes { id => file_path, kind, name, start_line, end_line, body_hash, embedding }",
            serde_json::json!({ "data": rows }),
        )
        .unwrap();
    engine
}

/// `cozo search --k N` returns N matches whenever N candidates clear the floor, even when
/// filtered rows (external `module` nodes) crowd the nearest HNSW results; the floor is
/// reported, not silent, and `--min-score` lowers it.
#[test]
fn vector_search_fills_k_and_reports_the_relevance_floor() {
    use knobyte::cozo::VectorSearchOptions;
    let query = "token validation session";
    let mut nodes: Vec<(String, &str, String)> = (0..60)
        .map(|i| (format!("mod{}", i), "module", query.to_string()))
        .collect();
    for i in 0..8 {
        nodes.push((format!("fn{}", i), "function", format!("{} helper{}", query, i)));
    }
    // Unrelated symbols: below the default floor.
    for i in 0..20 {
        nodes.push((format!("other{}", i), "function", format!("render canvas pixel{}", i)));
    }
    let engine = engine_with_nodes(&nodes);

    let out = engine
        .vector_search_with(query, "code", &VectorSearchOptions { k: 5, min_score: None })
        .unwrap();
    assert_eq!(out.matches.len(), 5, "{:?}", out);
    assert!(out.matches.iter().all(|m| m.id.starts_with("fn")));
    assert_eq!(out.below_floor, 0);
    assert!(out.exact_fallback, "modules crowded the index results");
    assert_eq!(engine.vector_search(query, "code", 5).unwrap().len(), 5);

    // Ten wanted, eight relevant: the two left out are counted against the floor.
    let out = engine
        .vector_search_with(query, "code", &VectorSearchOptions { k: 10, min_score: None })
        .unwrap();
    assert_eq!(out.matches.len(), 8);
    assert_eq!(out.below_floor, 2);
    assert!((out.min_score - 0.20).abs() < 1e-9);
    // Without a floor, k results.
    let out = engine
        .vector_search_with(query, "code", &VectorSearchOptions { k: 10, min_score: Some(0.0) })
        .unwrap();
    assert_eq!(out.matches.len(), 10);
    assert_eq!(out.below_floor, 0);

    // Nothing relevant: no matches, and every one of the k nearest is reported.
    let out = engine
        .vector_search_with("zzqx unrelated", "code", &VectorSearchOptions { k: 4, min_score: None })
        .unwrap();
    assert!(out.matches.is_empty());
    assert_eq!(out.below_floor, 4);

    // Fewer candidates than k: all of them, no error.
    let small = engine_with_nodes(&[
        ("a".into(), "function", query.into()),
        ("b".into(), "function", format!("{} extra", query)),
    ]);
    let out = small
        .vector_search_with(query, "code", &VectorSearchOptions { k: 10, min_score: Some(0.0) })
        .unwrap();
    assert_eq!(out.matches.len(), 2);
}
