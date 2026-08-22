//! Publication delta, snapshot provenance and status diagnostics, immutable read sessions,
//! in-place schema migration, maintenance cancellation and lock timeouts.

use knobyte::graph::read::{ReadGate, READER_DATABASE_CHANGED};
use knobyte::graph::{
    inspect_status, maintenance_error, rebuild_graph_with, refresh_graph, refresh_graph_with, repair_graph,
    scan_corpus, CorpusPolicy, GraphEngine, MaintenanceLock, MaintenanceOptions,
};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn db(root: &Path) -> PathBuf {
    root.join(".knobyte").join("graph.db")
}

fn fixture(root: &Path) {
    write(root, "src/lib.rs", "pub mod auth;\npub mod models;\n\npub fn entry() {\n    auth::check();\n}\n");
    write(
        root,
        "src/auth.rs",
        "use crate::models::User;\n\npub fn check() -> bool {\n    let u = make_user();\n    validate(&u)\n}\n\npub fn make_user() -> User {\n    User { id: 1 }\n}\n\nfn validate(_u: &User) -> bool {\n    true\n}\n",
    );
    write(root, "src/models.rs", "/// A user.\npub struct User {\n    pub id: u64,\n}\n\npub const MAX_USERS: u64 = 10;\n");
    write(
        root,
        "web/child.ts",
        "import { Base } from './base';\n\nexport class Child extends Base {\n  run(): number { return helper(); }\n}\n\nexport function helper(): number { return 2; }\n",
    );
    write(root, "web/base.ts", "export class Base {\n  run(): number { return 1; }\n}\n");
    write(root, "py/app.py", "from .views import index\n\nLIMIT = 3\n\ndef home():\n    return index()\n");
    write(root, "py/views.py", "def index():\n    return 'ok'\n");
}

fn scan(root: &Path) -> knobyte::graph::CorpusScan {
    scan_corpus(root, &CorpusPolicy::for_project(root)).unwrap()
}

fn rebuild(root: &Path) -> GraphEngine {
    let mut e = GraphEngine::open(&db(root)).unwrap();
    e.rebuild(root).unwrap();
    e
}

fn conn(root: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(db(root)).unwrap()
}

/// Every derived row of a graph, minus row ids and bookkeeping timestamps.
fn full_dump(db_path: &Path) -> Vec<BTreeSet<String>> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let rows = |sql: &str| -> BTreeSet<String> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    vec![
        rows("SELECT json_array(id, kind, name, qualified_name, container_id, identity_key, file_path, language, start_line, end_line, start_column, end_column, docstring, signature, visibility, is_exported, is_async, is_static, is_abstract, return_type, body_hash) FROM nodes"),
        rows("SELECT json_array(source, target, kind, metadata, line, col, provenance, confidence, resolution_method) FROM edges"),
        rows("SELECT json_array(ref_key, from_node_id, reference_name, reference_kind, line, col, candidates, file_path, status) FROM unresolved_refs"),
        rows("SELECT json_array(binding_key, file_path, local_name, imported_name, module_specifier, resolved_file_path, target_id) FROM import_bindings"),
        rows("SELECT json_array(path, content_hash, language, size, node_count, parse_status, extractor_version) FROM files"),
        rows("SELECT json_array(path, content_hash, extractor_version, length(payload)) FROM file_extraction_cache"),
        rows("SELECT json_array(file_path, content_hash, start_line, end_line, path_terms, identifier_terms) FROM code_chunks"),
        rows("SELECT json_array(node_id, hex(minhash), token_count) FROM node_minhash"),
        rows("SELECT json_array(band, bucket, node_id) FROM node_lsh"),
        rows("SELECT json_array(id, name, qualified_name) FROM nodes_fts"),
    ]
}

fn copy_tree(from: &Path, to: &Path) {
    for entry in walk(from) {
        let rel = entry.strip_prefix(from).unwrap();
        if rel.starts_with(".knobyte") {
            continue;
        }
        let dest = to.join(rel);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::copy(&entry, &dest).unwrap();
    }
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn codes(h: &knobyte::graph::GraphHealth) -> Vec<String> {
    h.diagnostics.iter().map(|d| d.code.clone()).collect()
}

// ---------------------------------------------------------------------------
// Publication delta
// ---------------------------------------------------------------------------

#[test]
fn delta_refresh_publishes_only_changed_rows_and_equals_a_rebuild() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    rebuild(root);
    let nodes_before: i64 = conn(root).query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0)).unwrap();

    write(root, "src/models.rs", "/// A user.\npub struct User {\n    pub id: u64,\n    pub name: String,\n}\n\npub const MAX_USERS: u64 = 20;\n");
    write(root, "py/extra.py", "def extra():\n    return 1\n");
    fs::remove_file(root.join("py/views.py")).unwrap();
    let out = refresh_graph(&db(root), root, &scan(root), None).unwrap();
    assert_eq!(out.mode, "incremental");
    let publication = out.publication.expect("publication report");
    assert_eq!(publication.mode, "delta", "{:?}", publication.fallback_reason);
    let delta = publication.delta.unwrap();
    // Only a fraction of the graph was written.
    assert!(delta.rows_written() > 0);
    assert!((delta.written.get("nodes").copied().unwrap_or(0) as i64) < nodes_before, "{:?}", delta);
    assert!(delta.deleted.get("files").copied().unwrap_or(0) == 1, "{:?}", delta);

    // The delta-published graph equals a full rebuild of the same tree.
    let other = tempdir().unwrap();
    copy_tree(root, other.path());
    rebuild(other.path());
    let a = full_dump(&db(root));
    let b = full_dump(&db(other.path()));
    let names = ["nodes", "edges", "refs", "bindings", "files", "cache", "chunks", "minhash", "lsh", "fts"];
    for (i, n) in names.iter().enumerate() {
        assert_eq!(a[i], b[i], "table {} differs after a delta publication", n);
    }
    // FTS stays consistent and finds new symbols.
    let c = conn(root);
    c.execute_batch("INSERT INTO nodes_fts(nodes_fts, rank) VALUES('integrity-check', 1);").unwrap();
    let hits: i64 = c
        .query_row("SELECT COUNT(*) FROM nodes_fts WHERE nodes_fts MATCH 'extra'", [], |r| r.get(0))
        .unwrap();
    assert!(hits >= 1);
    assert_eq!(inspect_status(&db(root), root).status, "fresh");
}

#[test]
fn delta_refresh_keeps_unchanged_rows_untouched() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    rebuild(root);
    let rowid_of = |q: &str| -> i64 {
        conn(root)
            .query_row("SELECT rowid FROM nodes WHERE qualified_name = ?1", [q], |r| r.get(0))
            .unwrap()
    };
    let (validate_before, user_before) = (rowid_of("validate"), rowid_of("User"));
    let edge_ids: BTreeSet<i64> = {
        let c = conn(root);
        let mut s = c.prepare("SELECT id FROM edges").unwrap();
        s.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    write(root, "py/views.py", "def index():\n    return 'changed'\n");
    let out = refresh_graph(&db(root), root, &scan(root), None).unwrap();
    assert_eq!(out.publication.unwrap().mode, "delta");
    // Rows of untouched files keep their identity (updated in place or not at all).
    assert_eq!(rowid_of("validate"), validate_before);
    assert_eq!(rowid_of("User"), user_before);
    let after: BTreeSet<i64> = {
        let c = conn(root);
        let mut s = c.prepare("SELECT id FROM edges").unwrap();
        s.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    let kept = edge_ids.intersection(&after).count();
    assert!(kept + 2 >= edge_ids.len(), "only edges of the changed file may be rewritten: kept {} of {}", kept, edge_ids.len());
}

// ---------------------------------------------------------------------------
// Snapshot provenance and status diagnostics
// ---------------------------------------------------------------------------

fn fake_git(root: &Path, branch: &str, head: &str) {
    let git = root.join(".git");
    fs::create_dir_all(git.join("refs/heads")).unwrap();
    fs::write(git.join("HEAD"), format!("ref: refs/heads/{}\n", branch)).unwrap();
    let r = git.join("refs/heads").join(branch);
    fs::create_dir_all(r.parent().unwrap()).unwrap();
    fs::write(r, format!("{}\n", head)).unwrap();
}

#[test]
fn snapshot_records_provenance_and_detects_branch_switch() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let a = "a".repeat(40);
    fake_git(root, "main", &a);
    rebuild(root);
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "fresh", "{:?}", h.diagnostics);
    let snap = h.snapshot.clone().expect("snapshot");
    assert_eq!(snap.indexed_branch.as_deref(), Some("main"));
    assert_eq!(snap.indexed_head.as_deref(), Some(a.as_str()));
    assert_eq!(h.publication_id.as_deref(), Some(snap.publication_id.as_str()));

    // New commit on the same branch, no source change: informational only.
    fs::write(root.join(".git/refs/heads/main"), format!("{}\n", "b".repeat(40))).unwrap();
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "fresh");
    assert!(codes(&h).contains(&"GRAPH_INDEX_HEAD_CHANGED".to_string()));

    // Branch switch: stale with a reason, even with identical sources.
    fake_git(root, "feature", &"c".repeat(40));
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "stale");
    assert!(h.changes.branch_changed);
    assert!(codes(&h).contains(&"GRAPH_INDEX_BRANCH_CHANGED".to_string()));
    assert_eq!(h.next_command(), Some("knobyte graph refresh"));
    // Reads stay served (the drift is exactly known: none).
    assert!(ReadGate::inspect(&db(root), root).is_ok());

    // Refresh re-records provenance without republishing rows.
    let out = refresh_graph(&db(root), root, &scan(root), None).unwrap();
    assert_eq!(out.mode, "noop");
    assert!(out.provenance_updated);
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "fresh", "{:?}", h.diagnostics);
    assert_eq!(h.snapshot.unwrap().indexed_branch.as_deref(), Some("feature"));
}

fn set_snapshot_field(root: &Path, field: &str, value: &str) {
    let c = conn(root);
    let raw: String = c
        .query_row("SELECT value FROM project_metadata WHERE key = 'graph_snapshot_v1'", [], |r| r.get(0))
        .unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v[field] = serde_json::json!(value);
    c.execute(
        "UPDATE project_metadata SET value = ?1 WHERE key = 'graph_snapshot_v1'",
        [v.to_string()],
    )
    .unwrap();
}

#[test]
fn snapshot_tampering_grammar_change_and_legacy_graphs() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    rebuild(root);

    // Grammar change: stale, unbounded (reads refused), refresh republishes.
    set_snapshot_field(root, "grammarHash", "0000000000000000");
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "stale");
    assert!(h.changes.grammar_changed);
    assert!(codes(&h).contains(&"GRAPH_BUILD_MANIFEST_CHANGED".to_string()));
    assert!(ReadGate::inspect(&db(root), root).is_err());
    let out = refresh_graph(&db(root), root, &scan(root), None).unwrap();
    assert_eq!(out.mode, "incremental");
    assert_eq!(inspect_status(&db(root), root).status, "fresh");

    // Rows changed outside a publication: corrupt.
    conn(root).execute("UPDATE files SET content_hash = 'tampered' WHERE path = 'py/app.py'", []).unwrap();
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "corrupt");
    assert!(codes(&h).contains(&"GRAPH_SNAPSHOT_CONTENT_MISMATCH".to_string()));
    rebuild(root);

    // Unparseable snapshot: corrupt.
    conn(root)
        .execute("UPDATE project_metadata SET value = '{not json' WHERE key = 'graph_snapshot_v1'", [])
        .unwrap();
    assert!(codes(&inspect_status(&db(root), root)).contains(&"GRAPH_SNAPSHOT_INVALID".to_string()));
    rebuild(root);

    // A graph published before snapshots: stale until a refresh records provenance.
    conn(root)
        .execute("DELETE FROM project_metadata WHERE key IN ('graph_snapshot_v1', 'publication_id')", [])
        .unwrap();
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "stale");
    assert!(codes(&h).contains(&"GRAPH_SNAPSHOT_LEGACY".to_string()));
    let out = refresh_graph(&db(root), root, &scan(root), None).unwrap();
    assert!(out.provenance_updated);
    assert_eq!(inspect_status(&db(root), root).status, "fresh");

    // Escaping paths violate an invariant.
    conn(root).execute("UPDATE files SET path = '../outside.py' WHERE path = 'py/app.py'", []).unwrap();
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "corrupt");
    assert!(codes(&h).contains(&"GRAPH_INDEX_INVARIANT_FAILED".to_string()));
}

#[test]
fn orphan_rows_suggest_repair_and_sidecars_are_probed() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    rebuild(root);
    let c = conn(root);
    c.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    c.execute(
        "INSERT INTO edges (source, target, kind, confidence) VALUES ('ghost', 'ghost2', 'calls', 1.0)",
        [],
    )
    .unwrap();
    drop(c);
    let h = inspect_status(&db(root), root);
    let d = h.diagnostics.iter().find(|d| d.code == "GRAPH_INDEX_REPAIR_AVAILABLE").expect("repair hint");
    assert_eq!(d.command.as_deref(), Some("knobyte graph repair"));
    let r = repair_graph(&db(root), root).unwrap();
    assert_eq!(r.orphan_edges_removed, 1);
    assert!(!codes(&inspect_status(&db(root), root)).contains(&"GRAPH_INDEX_REPAIR_AVAILABLE".to_string()));

    // A maintenance run in progress is reported.
    let lock = MaintenanceLock::acquire(&db(root)).unwrap();
    assert!(codes(&inspect_status(&db(root), root)).contains(&"GRAPH_MAINTENANCE_ACTIVE".to_string()));
    drop(lock);
    assert!(!codes(&inspect_status(&db(root), root)).contains(&"GRAPH_MAINTENANCE_ACTIVE".to_string()));
}

// ---------------------------------------------------------------------------
// Immutable read sessions
// ---------------------------------------------------------------------------

#[test]
fn read_sessions_pin_one_publication_and_detect_republication() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    rebuild(root);

    // A gate inspected before a republication refuses to open on the new one.
    let gate = ReadGate::inspect(&db(root), root).unwrap();
    write(root, "src/more.rs", "pub fn more() {}\n");
    rebuild(root);
    let Err(err) = gate.open_engine(&db(root)) else { panic!("opened a republished graph") };
    assert_eq!(err.reason_code, READER_DATABASE_CHANGED);
    assert_eq!(err.record()["reasonCode"], READER_DATABASE_CHANGED);

    // `open_session` re-inspects and binds to the current publication.
    let Ok((engine, gate)) = ReadGate::open_session(&db(root), root, false) else { panic!("session") };
    assert!(engine.is_pinned());
    assert_eq!(engine.publication_id(), gate.health.publication_id);
    let count = |e: &GraphEngine| -> i64 { e.connection().query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0)).unwrap() };
    let before = count(&engine);
    let pinned_id = engine.publication_id();

    // A publication while the session is open does not leak into it.
    write(root, "src/even_more.rs", "pub fn a() {}\npub fn b() {}\n");
    refresh_graph(&db(root), root, &scan(root), None).unwrap();
    assert_eq!(count(&engine), before);
    assert_eq!(engine.publication_id(), pinned_id);
    // Nested snapshots inside the session are no-ops, and the session survives them.
    drop(engine.read_snapshot().unwrap());
    assert!(engine.is_pinned());
    assert_eq!(count(&engine), before);
    engine.unpin_snapshot();
    assert!(count(&engine) > before);
    assert_ne!(engine.publication_id(), pinned_id);
}

// ---------------------------------------------------------------------------
// Schema migration
// ---------------------------------------------------------------------------

#[test]
fn older_schema_is_upgraded_in_place_keeping_groundings() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    rebuild(root);
    let c = conn(root);
    c.execute(
        "INSERT INTO _knobyte_grounded_source (subject_kind, subject_id, node_id, source, body_hash, fingerprint) \
         VALUES ('doc', 'docs/a.md', 'function:src/auth.rs:check', 'x', 'h', 'f')",
        [],
    )
    .unwrap();
    // Simulate a v5 store: v5 history, a v4 leftover table, columns added later missing.
    c.execute_batch(
        "DELETE FROM schema_versions; INSERT INTO schema_versions VALUES (5, 0, 'v5');
         CREATE TABLE node_aliases (alias_id TEXT PRIMARY KEY);
         ALTER TABLE edges DROP COLUMN evidence;
         ALTER TABLE files DROP COLUMN missing_count;
         DROP TABLE node_minhash;",
    )
    .unwrap();
    drop(c);
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "stale");
    assert!(codes(&h).contains(&"GRAPH_INDEX_REPAIR_AVAILABLE".to_string()));
    assert_eq!(h.next_command(), Some("knobyte graph refresh"));

    let r = repair_graph(&db(root), root).unwrap();
    assert_eq!(r.migrated_from, Some(5));
    assert_eq!(r.schema_version, knobyte::graph::schema::CURRENT_SCHEMA_VERSION);
    let c = conn(root);
    let has_col = |t: &str, col: &str| -> bool {
        c.prepare(&format!("SELECT {} FROM {} LIMIT 1", col, t)).is_ok()
    };
    assert!(has_col("edges", "evidence") && has_col("files", "missing_count") && has_col("node_minhash", "minhash"));
    let aliases: i64 = c
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'node_aliases'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(aliases, 0);
    let history: Vec<i64> = {
        let mut s = c.prepare("SELECT version FROM schema_versions ORDER BY version").unwrap();
        s.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    assert_eq!(history, vec![5, knobyte::graph::schema::CURRENT_SCHEMA_VERSION]);
    let grounded: i64 = c.query_row("SELECT COUNT(*) FROM _knobyte_grounded_source", [], |r| r.get(0)).unwrap();
    assert_eq!(grounded, 1);
    drop(c);
    // Reads work again and a refresh re-derives everything.
    assert!(GraphEngine::open_read_only(&db(root)).is_ok());
    let out = refresh_graph(&db(root), root, &scan(root), None).unwrap();
    assert_eq!(out.mode, "incremental");
    assert_eq!(inspect_status(&db(root), root).status, "fresh");
}

#[test]
fn unmigratable_schema_falls_back_to_rebuild_without_changes() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    rebuild(root);
    let c = conn(root);
    // A required NOT NULL column without default cannot be added in place.
    c.execute_batch(
        "DELETE FROM schema_versions; INSERT INTO schema_versions VALUES (4, 0, 'v4');
         ALTER TABLE files DROP COLUMN content_hash;",
    )
    .unwrap();
    drop(c);
    let err = refresh_graph(&db(root), root, &scan(root), None).unwrap_err();
    assert_eq!(maintenance_error(&err).unwrap().code, "GRAPH_REBUILD_REQUIRED");
    let v: i64 = conn(root).query_row("SELECT MAX(version) FROM schema_versions", [], |r| r.get(0)).unwrap();
    assert_eq!(v, 4, "a failed migration must roll back");
    rebuild(root);
    assert_eq!(inspect_status(&db(root), root).status, "fresh");
}

// ---------------------------------------------------------------------------
// Maintenance options
// ---------------------------------------------------------------------------

fn candidates(root: &Path) -> usize {
    fs::read_dir(root.join(".knobyte"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".candidate-"))
        .count()
}

#[test]
fn cancellation_leaves_the_live_graph_untouched() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    rebuild(root);
    let before = full_dump(&db(root));
    let id_before = inspect_status(&db(root), root).publication_id;
    write(root, "src/new.rs", "pub fn new_one() {}\n");

    // Cancelled at every possible point: from the first poll up to well into the build.
    for after in [0usize, 1, 2, 3, 4, 6] {
        let polls = Arc::new(AtomicUsize::new(0));
        let p = polls.clone();
        let opts = MaintenanceOptions::default().with_cancel(Arc::new(move || p.fetch_add(1, Ordering::SeqCst) >= after));
        for incremental in [false, true] {
            polls.store(0, Ordering::SeqCst);
            let res = if incremental {
                refresh_graph_with(&db(root), root, &scan(root), None, &opts).map(|_| ())
            } else {
                rebuild_graph_with(&db(root), root, &scan(root), None, &opts).map(|_| ())
            };
            let err = res.expect_err("cancelled run must fail");
            assert_eq!(maintenance_error(&err).unwrap().code, "GRAPH_MAINTENANCE_CANCELLED", "after {}", after);
            assert_eq!(full_dump(&db(root)), before);
            assert_eq!(inspect_status(&db(root), root).publication_id, id_before);
            assert_eq!(candidates(root), 0);
        }
    }
    // Without cancellation the same run publishes.
    let opts = MaintenanceOptions::default().with_cancel(Arc::new(|| false));
    refresh_graph_with(&db(root), root, &scan(root), None, &opts).unwrap();
    assert_eq!(inspect_status(&db(root), root).status, "fresh");
}

#[test]
fn lock_timeout_waits_for_a_concurrent_run() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fixture(&root);
    rebuild(&root);
    write(&root, "src/new.rs", "pub fn new_one() {}\n");

    let lock = MaintenanceLock::acquire(&db(&root)).unwrap();
    let err = refresh_graph(&db(&root), &root, &scan(&root), None).unwrap_err();
    assert_eq!(maintenance_error(&err).unwrap().code, "GRAPH_MAINTENANCE_LOCKED");
    let short = MaintenanceOptions::default().with_lock_timeout(Duration::from_millis(150));
    let err = refresh_graph_with(&db(&root), &root, &scan(&root), None, &short).unwrap_err();
    assert_eq!(maintenance_error(&err).unwrap().code, "GRAPH_MAINTENANCE_LOCKED");

    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        drop(lock);
    });
    let patient = MaintenanceOptions::default().with_lock_timeout(Duration::from_secs(20));
    let out = refresh_graph_with(&db(&root), &root, &scan(&root), None, &patient).unwrap();
    releaser.join().unwrap();
    assert_eq!(out.mode, "incremental");
}

#[test]
fn cli_accepts_lock_timeout() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_knobyte"))
        .args(["graph", "rebuild", "--json", "--lock-timeout", "5", "--root"])
        .arg(root)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_knobyte"))
        .args(["graph", "refresh", "--json", "--lock-timeout", "5", "--root"])
        .arg(root)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["mode"], "noop");
}
