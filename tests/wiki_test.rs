use tempfile::tempdir;
use std::fs;
use knobyte::wiki::WikiIndex;

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
    ).unwrap();

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
