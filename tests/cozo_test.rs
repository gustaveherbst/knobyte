use knobyte::cozo::{CozoEngine, Embedder};

#[test]
fn test_embedder_deterministic_and_normalized() {
    let text = "fn calculate_hash(data: &[u8]) -> u64";
    let v1 = Embedder::embed(text);
    let v2 = Embedder::embed(text);
    assert_eq!(v1, v2);
    assert_eq!(v1.len(), 128);

    let norm: f32 = v1.iter().map(|&x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-4);
}

#[test]
fn test_cozo_engine_init_and_query() {
    let engine = CozoEngine::in_memory().expect("in-memory CozoEngine should initialize");

    // Run simple Datalog query
    let res = engine.datalog_query("?[a, b] <- [['hello', 42]]", serde_json::json!({}))
        .expect("Datalog query should succeed");

    assert_eq!(res["headers"], serde_json::json!(["a", "b"]));
    assert_eq!(res["rows"], serde_json::json!([["hello", 42]]));
}

#[test]
fn test_cozo_vector_search_and_graph_algos() {
    let engine = CozoEngine::in_memory().expect("in-memory CozoEngine should initialize");

    // Insert sample nodes with embeddings into code_nodes
    let sample_nodes = vec![
        ("fn:auth_login", "src/auth.rs", "function", "login", 10, 30, "hash1", "fn login(user: &str, pass: &str) -> Result<Session>"),
        ("fn:auth_logout", "src/auth.rs", "function", "logout", 32, 45, "hash2", "fn logout(session: &Session) -> Result<()>"),
        ("fn:render_canvas", "src/ui/canvas.rs", "function", "render_canvas", 5, 50, "hash3", "fn render_canvas(ctx: &Context, width: u32, height: u32)"),
    ];

    let mut tuples = Vec::new();
    for (id, file_path, kind, name, start, end, body_hash, snippet) in sample_nodes {
        let embedding = Embedder::embed(snippet);
        tuples.push(serde_json::json!([
            id,
            file_path,
            kind,
            name,
            start,
            end,
            body_hash,
            embedding
        ]));
    }

    let put_nodes = r#"
        ?[id, file_path, kind, name, start_line, end_line, body_hash, embedding] <- $data
        :put code_nodes { id => file_path, kind, name, start_line, end_line, body_hash, embedding }
    "#;
    engine.datalog_query(put_nodes, serde_json::json!({ "data": tuples }))
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
    engine.datalog_query(put_edges, serde_json::json!({ "data": edge_tuples }))
        .expect("Inserting edges should succeed");

    // 1. Test Vector Search: Search for "login user password"
    let matches = engine.vector_search("login user password", "code", 2)
        .expect("Vector search should succeed");

    assert!(!matches.is_empty(), "Vector search should return matches");
    assert_eq!(matches[0].id, "fn:auth_login", "Expected login function to match login user password query");

    // 2. Test PageRank
    let ranks = engine.pagerank(Some(0.85), Some(20))
        .expect("PageRank should execute");
    assert!(!ranks.is_empty(), "PageRank should return scored nodes");

    // 3. Test Shortest Path
    let path = engine.shortest_path("fn:auth_login", "fn:render_canvas")
        .expect("ShortestPath should execute");
    assert!(path.is_some(), "Shortest path should be found between login and render_canvas");
    let path_nodes = path.unwrap();
    assert_eq!(path_nodes, vec!["fn:auth_login", "fn:render_canvas"]);
}
