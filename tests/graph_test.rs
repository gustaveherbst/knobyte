use knobyte::graph::GraphEngine;
use std::fs;
use tempfile::tempdir;

#[test]
fn test_graph_extraction_and_query() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    let rs_file = src_dir.join("calc.rs");
    fs::write(
        &rs_file,
        r#"
/// Calculate sum of two numbers
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn calculate() {
    let res = add(1, 2);
}
"#,
    )
    .unwrap();

    let db_path = root.join("graph.db");
    let mut engine = GraphEngine::open(&db_path).unwrap();

    let summary = engine.rebuild(root).unwrap();
    assert!(summary.files_indexed >= 1);
    assert!(summary.nodes_indexed >= 2);

    // Test where-defined
    let nodes = engine.query_where_defined("add").unwrap();
    assert!(!nodes.is_empty());
    assert_eq!(nodes[0].name, "add");
    assert_eq!(nodes[0].kind, "function");

    // Test who-calls
    let callers = engine.query_who_calls("add").unwrap();
    assert!(!callers.is_empty());
    assert_eq!(callers[0].name, "calculate");

    // Test status
    let status = engine.status().unwrap();
    assert!(status.node_count >= 2);
    assert!(status.edge_count >= 1);
}

#[test]
fn test_rust_traits_impls_and_method_calls() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let rs_file = src_dir.join("notifications.rs");
    fs::write(
        &rs_file,
        r#"
pub trait NotificationStore {
    async fn place_contact_hold(&self, id: u64);
}

pub struct PostgresNotificationStore;

impl NotificationStore for PostgresNotificationStore {
    async fn place_contact_hold(&self, id: u64) {
        println!("{}", id);
    }
}

pub async fn run_service(store: &PostgresNotificationStore) {
    store.place_contact_hold(42).await;
}
"#,
    )
    .unwrap();

    let db_path = root.join("graph.db");
    let mut engine = GraphEngine::open(&db_path).unwrap();
    let summary = engine.rebuild(root).unwrap();
    assert_eq!(summary.files_indexed, 1);
    assert!(summary.nodes_indexed >= 4);
    assert!(summary.edges_indexed >= 2);

    // 1. where-defined should return both trait method and impl method
    let where_def = engine.query_where_defined("place_contact_hold").unwrap();
    assert_eq!(where_def.len(), 2);
    assert!(where_def
        .iter()
        .any(|n| n.qualified_name == "NotificationStore::place_contact_hold"));
    assert!(where_def
        .iter()
        .any(|n| n.qualified_name == "PostgresNotificationStore::place_contact_hold"));
    // is_async should be true for both!
    assert!(where_def.iter().all(|n| n.is_async));

    // 2. who-calls place_contact_hold should return run_service
    let who_calls = engine.query_who_calls("place_contact_hold").unwrap();
    assert_eq!(who_calls.len(), 1);
    assert_eq!(who_calls[0].name, "run_service");
    assert!(who_calls[0].is_async);

    // 3. where-defined NotificationStore returns trait
    let trait_nodes = engine.query_where_defined("NotificationStore").unwrap();
    assert!(!trait_nodes.is_empty());
    assert_eq!(trait_nodes[0].kind, "trait");
}

#[test]
fn test_who_imports_query() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let dep_file = src_dir.join("notifications.rs");
    fs::write(
        &dep_file,
        r#"
pub fn send_notification() {}
"#,
    )
    .unwrap();

    let consumer_file = src_dir.join("consumer.rs");
    fs::write(
        &consumer_file,
        r#"
use finaxis_notifications::send_notification;

pub fn handle_event() {
    send_notification();
}
"#,
    )
    .unwrap();

    let db_path = root.join("graph.db");
    let mut engine = GraphEngine::open(&db_path).unwrap();
    engine.rebuild(root).unwrap();

    // Query who-imports should execute without SQL error
    let importers = engine.query_who_imports("notifications").unwrap();
    assert!(!importers.is_empty());
    assert_eq!(importers[0].name, "handle_event");
}

#[test]
fn test_query_scope_explained() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let rs_file = src_dir.join("holds.rs");
    fs::write(
        &rs_file,
        r#"
pub struct ContactHold;

pub fn place_contact_hold() {}

pub fn lift_contact_hold() {
    place_contact_hold();
}
"#,
    )
    .unwrap();

    let db_path = root.join("graph.db");
    let mut engine = GraphEngine::open(&db_path).unwrap();
    engine.rebuild(root).unwrap();

    let scoped = engine
        .query_scope_explained(
            "expose contact holds over HTTP: place and lift a contact hold for a party",
        )
        .unwrap();
    assert!(!scoped.is_empty());
    assert!(scoped
        .iter()
        .any(|s| s.node.name == "place_contact_hold" || s.node.name == "lift_contact_hold"));
    // Every scoped node should have an explanation
    for s in &scoped {
        assert!(!s.reason.is_empty());
    }

    // Irrelevant query should return empty, not random nodes!
    let irrelevant = engine
        .query_scope_explained("completely unrelated potato query")
        .unwrap();
    assert!(irrelevant.is_empty());
}

#[test]
fn test_sql_and_adr_extraction() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let migrations_dir = root.join("migrations");
    fs::create_dir_all(&migrations_dir).unwrap();

    let sql_file = migrations_dir.join("001_init.sql");
    fs::write(
        &sql_file,
        r#"
CREATE TABLE contact_holds (
    id UUID PRIMARY KEY,
    party_id UUID NOT NULL,
    CONSTRAINT check_hold_status CHECK (status IN ('active', 'lifted'))
);

CREATE TRIGGER hold_audit_trigger AFTER INSERT ON contact_holds;
"#,
    )
    .unwrap();

    let docs_dir = root.join("docs").join("adr");
    fs::create_dir_all(&docs_dir).unwrap();
    let adr_file = docs_dir.join("0028-flag-governance.md");
    fs::write(
        &adr_file,
        r#"# ADR 0028: No flag may govern a control

## Context
Flags should not dictate controls.
"#,
    )
    .unwrap();

    let db_path = root.join("graph.db");
    let mut engine = GraphEngine::open(&db_path).unwrap();
    let summary = engine.rebuild(root).unwrap();

    assert!(summary.nodes_indexed >= 3);

    let table_node = engine.query_where_defined("contact_holds").unwrap();
    assert!(!table_node.is_empty());
    assert_eq!(table_node[0].kind, "table");

    let constraint_node = engine.query_where_defined("check_hold_status").unwrap();
    assert!(!constraint_node.is_empty());
    assert_eq!(constraint_node[0].kind, "check_constraint");

    let adr_node = engine
        .query_where_defined("ADR 0028: No flag may govern a control")
        .unwrap();
    assert!(!adr_node.is_empty());
    assert_eq!(adr_node[0].kind, "adr");
}

#[test]
fn test_indexing_with_progress_bar() {
    use knobyte::graph::scan_indexable_files;
    use knobyte::progress::IndexProgressBar;

    let dir = tempdir().unwrap();
    let root = dir.path();

    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).unwrap();

    let file_a = src_dir.join("a.rs");
    fs::write(&file_a, "pub fn func_a() -> i32 { 42 }\n").unwrap();

    let file_b = src_dir.join("b.rs");
    fs::write(
        &file_b,
        "pub fn func_b() -> String { \"hello\".to_string() }\n",
    )
    .unwrap();

    let (files, total_bytes) = scan_indexable_files(root);
    assert_eq!(files.len(), 2);
    assert!(total_bytes > 0);

    let pb = IndexProgressBar::new(total_bytes, files.len(), true);
    assert!(pb.is_enabled());

    let db_path = root.join("graph.db");
    let mut engine = GraphEngine::open(&db_path).unwrap();

    let summary = engine.rebuild_files(root, &files, Some(&pb)).unwrap();
    assert_eq!(summary.files_indexed, 2);
    assert_eq!(summary.nodes_indexed, 2);

    assert_eq!(pb.files_indexed(), 2);
    assert_eq!(pb.bytes_indexed(), total_bytes);
    assert_eq!(pb.symbols_indexed(), 2);

    pb.set_phase("Test phase complete");
    pb.finish_and_clear();
}
