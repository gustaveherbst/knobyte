use knobyte::wiki::WikiIndex;
use std::fs;
use tempfile::tempdir;

#[test]
fn test_wiki_indexing_and_fts_search() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    let context_dir = root.join("context");
    fs::create_dir_all(&context_dir).unwrap();

    let md_file = context_dir.join("auth.md");
    fs::write(
        &md_file,
        r#"---
id: kb_auth
title: Authentication Architecture
type: architecture
summary: JWT and OAuth authentication design for microservices.
status: active
revision: 1
relations:
  - type: depends_on
    target_id: kb_db
    note: uses session store
---

# Authentication Architecture

All requests must carry a valid bearer token verified by the gateway.
"#,
    )
    .unwrap();

    let db_path = root.join("wiki.db");
    let mut index = WikiIndex::open(&db_path).unwrap();

    let count = index.rebuild(root).unwrap();
    assert_eq!(count, 1);

    // Test show
    let entity = index.show("kb_auth").unwrap().expect("Entity should exist");
    assert_eq!(entity.id, "kb_auth");
    assert_eq!(entity.title, "Authentication Architecture");
    assert_eq!(entity.relations.len(), 1);
    assert_eq!(entity.relations[0].target_id, "kb_db");

    // Test FTS search
    let matches = index.query("JWT microservices").unwrap();
    assert!(!matches.is_empty());
    assert_eq!(matches[0].id, "kb_auth");
}

#[test]
fn test_wiki_validate_reports_parse_errors_duplicates_and_groundings() {
    use knobyte::graph::GraphEngine;
    use knobyte::wiki::parser::parse_markdown_entity_with_diagnostics;

    let dir = tempdir().unwrap();
    let root = dir.path();
    let scaffold = root.join(".knobyte");
    let ctx = scaffold.join("context");
    fs::create_dir_all(&ctx).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/auth.rs"), "pub fn validate_token() {}\npub struct A;\nimpl A { pub fn run(&self) {} }\npub struct B;\nimpl B { pub fn run(&self) {} }\n").unwrap();

    // Invalid YAML (bad indentation / unterminated list).
    let broken = "---\nid: kb_broken\ngrounds_to: [function:src/auth.rs:validate_token\ntitle: x\n---\n# Broken\n";
    fs::write(ctx.join("broken.md"), broken).unwrap();
    let (entity, diags) = parse_markdown_entity_with_diagnostics("context/broken.md", broken);
    assert!(entity.is_some());
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0].code, "FRONTMATTER_PARSE_ERROR");
    assert!(diags[0].line.is_some());

    // Duplicate ids.
    fs::write(
        ctx.join("a.md"),
        "---\nid: kb_dup\ntitle: First\n---\n# First\n",
    )
    .unwrap();
    fs::write(
        ctx.join("b.md"),
        "---\nid: kb_dup\ntitle: Second\n---\n# Second\n",
    )
    .unwrap();

    // Groundings: one resolves, one missing, one ambiguous.
    fs::write(
        ctx.join("g.md"),
        "---\nid: kb_g\ngrounds_to:\n  - function:src/auth.rs:validate_token\n  - function:src/auth.rs:gone\n---\n# G\n<!-- kb-ground: method:src/auth.rs:run -->\n",
    )
    .unwrap();

    let mut graph = GraphEngine::open(&scaffold.join("graph.db")).unwrap();
    graph.rebuild(root).unwrap();

    let mut index = WikiIndex::open(&scaffold.join("wiki.db")).unwrap();
    index.rebuild(&scaffold).unwrap();
    let diags = index.validate().unwrap();
    let codes: Vec<(&str, &str)> = diags
        .iter()
        .map(|d| (d.code.as_str(), d.file.as_str()))
        .collect();

    assert!(
        codes.contains(&("FRONTMATTER_PARSE_ERROR", "context/broken.md")),
        "{:?}",
        diags
    );
    assert!(
        codes.contains(&("DUPLICATE_ENTITY_ID", "context/b.md")),
        "{:?}",
        diags
    );
    assert!(
        !codes.contains(&("DUPLICATE_ENTITY_ID", "context/a.md")),
        "{:?}",
        diags
    );
    assert!(
        codes.contains(&("GROUNDING_UNRESOLVED", "context/g.md")),
        "{:?}",
        diags
    );
    assert!(
        codes.contains(&("AMBIGUOUS_GROUNDING", "context/g.md")),
        "{:?}",
        diags
    );
    assert_eq!(
        diags
            .iter()
            .filter(|d| d.code == "GROUNDING_UNRESOLVED")
            .count(),
        1,
        "{:?}",
        diags
    );

    // for-code matches readable groundings by graph id, and by the readable ref itself.
    let vt_id = graph.query_where_defined("validate_token").unwrap()[0]
        .id
        .clone();
    let by_id = index.for_code(&vt_id).unwrap();
    assert!(by_id.iter().any(|e| e.id == "kb_g"), "{:?}", by_id);
    let by_ref = index
        .for_code("function:src/auth.rs:validate_token")
        .unwrap();
    assert!(by_ref.iter().any(|e| e.id == "kb_g"));

    // The first file wins for a duplicated id.
    assert_eq!(index.show("kb_dup").unwrap().unwrap().title, "First");

    // Without a graph, groundings are reported as unchecked instead of silently passing.
    let unchecked = index.validate_with_graph(None).unwrap();
    assert!(unchecked.iter().any(|d| d.code == "GROUNDINGS_UNCHECKED"));
}
