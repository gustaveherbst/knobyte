//! Persistent (Sled) CozoDB: deletions, relation drops and repair of databases written by
//! cozo's built-in Sled engine (which stored deletions as empty values).

use knobyte::cozo::CozoEngine;

fn relations(e: &CozoEngine) -> Vec<String> {
    let v = e
        .datalog_query("::relations", serde_json::json!({}))
        .expect("relation catalog must stay readable");
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[0].as_str().unwrap().to_string())
        .collect()
}

fn edge_sources(e: &CozoEngine) -> Vec<String> {
    let v = e
        .datalog_query("?[s] := *code_edges{source_id: s}", serde_json::json!({}))
        .unwrap();
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[0].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn sled_remove_and_rm_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("cozo.db");
    {
        let e = CozoEngine::open(&p).unwrap();
        e.datalog_query_mutable(
            "?[source_id, target_id, kind, file_path] <- [['a', 'b', 'calls', 'x'], ['c', 'd', 'calls', 'y']] :put code_edges { source_id, target_id, kind => file_path }",
            serde_json::json!({}),
        )
        .unwrap();
        e.datalog_query_mutable(
            "?[source_id, target_id, kind] <- [['a', 'b', 'calls']] :rm code_edges { source_id, target_id, kind }",
            serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(edge_sources(&e), vec!["c"]);
        e.datalog_query_mutable("::hnsw drop wiki_entities:wiki_vec", serde_json::json!({}))
            .unwrap();
        e.datalog_query_mutable("::remove wiki_entities", serde_json::json!({}))
            .unwrap();
        assert!(!relations(&e).contains(&"wiki_entities".to_string()));
    }
    // Reopen: catalog readable, removed row stays removed, dropped relation recreated.
    let e = CozoEngine::open(&p).unwrap();
    assert_eq!(edge_sources(&e), vec!["c"]);
    let rels = relations(&e);
    assert!(rels.contains(&"wiki_entities".to_string()), "{:?}", rels);
    assert!(
        rels.contains(&"wiki_entities:wiki_vec".to_string()),
        "{:?}",
        rels
    );
}

#[test]
fn databases_corrupted_by_upstream_sled_deletes_are_repaired() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("cozo.db");
    {
        // Upstream engine: `::remove` leaves an empty-valued catalog key behind.
        let db = cozo::DbInstance::new("sled", p.to_str().unwrap(), "").unwrap();
        db.run_default(":create old_rel { k: String => v: String }")
            .unwrap();
        db.run_default(":create keep_rel { k: String => v: String }")
            .unwrap();
        db.run_default("?[k, v] <- [['x', '1'], ['y', '2']] :put keep_rel { k => v }")
            .unwrap();
        db.run_default("?[k] <- [['x']] :rm keep_rel { k }")
            .unwrap();
        db.run_default("::remove old_rel").unwrap();
        assert!(
            db.run_default("::relations").is_err(),
            "upstream bug reproduced"
        );
    }
    let e = CozoEngine::open(&p).unwrap();
    let rels = relations(&e);
    assert!(!rels.contains(&"old_rel".to_string()));
    assert!(rels.contains(&"keep_rel".to_string()));
    let rows = e
        .datalog_query("?[k, v] := *keep_rel{k, v}", serde_json::json!({}))
        .unwrap();
    assert_eq!(rows["rows"], serde_json::json!([["y", "2"]]));
}
