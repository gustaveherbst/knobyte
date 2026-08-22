//! Graph robustness: atomic publication, crash safety, maintenance lock, consistent reads,
//! incremental refresh equivalence, status kinds, corpus policy, repair and CLI parity.

use knobyte::graph::{
    inspect_status, maintenance_error, rebuild_graph, refresh_graph, repair_graph, scan_corpus,
    CorpusPolicy, GraphEngine, ImpactOptions, MaintenanceLock, ABORT_AFTER_FILES_ENV,
};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn db(root: &Path) -> PathBuf {
    root.join(".knobyte").join("graph.db")
}

/// A small multi-language project with cross-file calls, imports and inheritance.
fn fixture(root: &Path) {
    write(
        root,
        "src/lib.rs",
        "pub mod auth;\npub mod models;\n\npub fn entry() {\n    auth::check();\n}\n",
    );
    write(
        root,
        "src/auth.rs",
        "use crate::models::User;\n\npub fn check() -> bool {\n    let u = make_user();\n    validate(&u)\n}\n\npub fn make_user() -> User {\n    User { id: 1 }\n}\n\nfn validate(_u: &User) -> bool {\n    true\n}\n",
    );
    write(
        root,
        "src/models.rs",
        "/// A user.\npub struct User {\n    pub id: u64,\n    pub role: Role,\n}\n\npub enum Role {\n    Admin,\n    Guest,\n}\n\nmod internal {\n    pub fn helper() {}\n}\n",
    );
    write(
        root,
        "web/base.ts",
        "export class Base {\n  run(): number { return 1; }\n}\n\nexport enum Color { Red, Green = 2 }\n",
    );
    write(
        root,
        "web/child.ts",
        "import { Base } from './base';\n\nexport class Child extends Base {\n  name: Base;\n  run(): number { return helper(); }\n}\n\nexport function helper(): number { return 2; }\n\nexport function make(): Child {\n  return new Child();\n}\n",
    );
    write(
        root,
        "py/app.py",
        "from .views import index\n\ndef route(f):\n    return f\n\n@route\ndef home():\n    return index()\n\nclass Model:\n    def save(self):\n        pass\n\nclass Account(Model):\n    def save(self):\n        return Model()\n",
    );
    write(root, "py/views.py", "def index():\n    return 'ok'\n");
    write(root, "py/__init__.py", "");
}

fn count(engine: &GraphEngine, sql: &str) -> i64 {
    engine.connection().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn rebuild(root: &Path) -> GraphEngine {
    let mut e = GraphEngine::open(&db(root)).unwrap();
    e.rebuild(root).unwrap();
    e
}

/// Comparable dump of the derived graph (no timestamps, no row ids).
fn dump(db_path: &Path) -> (BTreeSet<String>, BTreeSet<String>, BTreeSet<String>) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let rows = |sql: &str| -> BTreeSet<String> {
        let mut stmt = conn.prepare(sql).unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let nodes = rows(
        "SELECT id || '|' || kind || '|' || qualified_name || '|' || file_path || '|' || start_line || '|' || end_line \
         || '|' || COALESCE(body_hash,'') || '|' || COALESCE(container_id,'') FROM nodes",
    );
    let edges = rows(
        "SELECT source || '|' || target || '|' || kind || '|' || COALESCE(line,'') || '|' || confidence || '|' \
         || COALESCE(resolution_method,'') || '|' || COALESCE(provenance,'') FROM edges",
    );
    let refs = rows(
        "SELECT ref_key || '|' || reference_kind || '|' || status || '|' || COALESCE(candidates,'') FROM unresolved_refs",
    );
    (nodes, edges, refs)
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_knobyte")
}

// ---------------------------------------------------------------------------
// Atomic publication & crash safety
// ---------------------------------------------------------------------------

#[test]
fn crash_mid_rebuild_leaves_previous_graph_intact() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let engine = rebuild(root);
    let nodes_before = count(&engine, "SELECT COUNT(*) FROM nodes");
    let edges_before = count(&engine, "SELECT COUNT(*) FROM edges");
    assert!(nodes_before > 10 && edges_before > 10);
    drop(engine);

    // More sources, then a rebuild process that dies after extracting two files.
    write(root, "src/extra.rs", "pub fn extra() {}\n");
    let out = Command::new(bin())
        .args(["graph", "rebuild", "--json", "--root"])
        .arg(root)
        .env(ABORT_AFTER_FILES_ENV, "2")
        .output()
        .unwrap();
    assert!(!out.status.success(), "the rebuild was expected to abort");

    // The live graph is exactly the previous one, still readable and not corrupt.
    let engine = GraphEngine::open(&db(root)).unwrap();
    assert_eq!(count(&engine, "SELECT COUNT(*) FROM nodes"), nodes_before);
    assert_eq!(count(&engine, "SELECT COUNT(*) FROM edges"), edges_before);
    let health = inspect_status(&db(root), root);
    assert_eq!(health.status, "stale", "{:?}", health.diagnostics);
    assert_eq!(health.changes.added, vec!["src/extra.rs".to_string()]);
    let leftovers = fs::read_dir(root.join(".knobyte"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".candidate-"))
        .count();
    assert!(leftovers >= 1, "the aborted candidate should be left behind");

    // The next maintenance run cleans the abandoned candidate and publishes normally.
    let mut engine = engine;
    engine.rebuild(root).unwrap();
    assert!(count(&engine, "SELECT COUNT(*) FROM nodes") > nodes_before);
    let leftovers = fs::read_dir(root.join(".knobyte"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".candidate-"))
        .count();
    assert_eq!(leftovers, 0);
    assert_eq!(inspect_status(&db(root), root).status, "fresh");
}

#[test]
fn rebuild_preserves_grounding_baselines() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let mut engine = rebuild(root);
    engine
        .connection()
        .execute(
            "INSERT INTO _knobyte_grounded_source (subject_kind, subject_id, node_id, source, body_hash, fingerprint) \
             VALUES ('scaffold', 'specs/a.md', 'function:src/auth.rs:check', 'src', 'h', 'f')",
            [],
        )
        .unwrap();
    engine.rebuild(root).unwrap();
    assert_eq!(count(&engine, "SELECT COUNT(*) FROM _knobyte_grounded_source"), 1);
    // FTS index is rebuilt with the published nodes.
    assert!(count(&engine, "SELECT COUNT(*) FROM nodes_fts WHERE nodes_fts MATCH 'make_user'") >= 1);
}

// ---------------------------------------------------------------------------
// Maintenance lock & consistent reads
// ---------------------------------------------------------------------------

#[test]
fn concurrent_maintenance_is_refused_while_locked() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let mut engine = rebuild(root);
    let nodes_before = count(&engine, "SELECT COUNT(*) FROM nodes");

    let lock = MaintenanceLock::acquire(&db(root)).unwrap();
    // In-process contender.
    let err = engine.rebuild(root).unwrap_err();
    assert_eq!(maintenance_error(&err).unwrap().code, "GRAPH_MAINTENANCE_LOCKED");
    let err = engine.refresh(root, None).unwrap_err();
    assert_eq!(maintenance_error(&err).unwrap().code, "GRAPH_MAINTENANCE_LOCKED");
    // Cross-process contender.
    let out = Command::new(bin())
        .args(["graph", "refresh", "--json", "--root"])
        .arg(root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("GRAPH_MAINTENANCE_LOCKED"));
    assert_eq!(count(&engine, "SELECT COUNT(*) FROM nodes"), nodes_before);

    // Released: maintenance proceeds.
    drop(lock);
    engine.rebuild(root).unwrap();
}

#[test]
fn readers_keep_a_consistent_snapshot_during_rebuild() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let reader = rebuild(root);
    let before = count(&reader, "SELECT COUNT(*) FROM nodes");

    write(root, "src/more.rs", "pub fn a() {}\npub fn b() {}\npub fn c() {}\n");
    {
        let _snapshot = reader.read_snapshot().unwrap();
        let mut writer = GraphEngine::open(&db(root)).unwrap();
        writer.rebuild(root).unwrap();
        // Inside the snapshot the reader still sees the previously published build.
        assert_eq!(count(&reader, "SELECT COUNT(*) FROM nodes"), before);
        assert_eq!(
            count(&reader, "SELECT COUNT(*) FROM nodes WHERE file_path = 'src/more.rs'"),
            0
        );
    }
    assert!(count(&reader, "SELECT COUNT(*) FROM nodes") > before);
}

// ---------------------------------------------------------------------------
// Incremental refresh
// ---------------------------------------------------------------------------

#[test]
fn incremental_refresh_matches_full_rebuild() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let mut engine = rebuild(root);

    // No change: nothing is published.
    let built_at = count(&engine, "SELECT COUNT(*) FROM project_metadata WHERE key = 'last_build_time'");
    assert_eq!(built_at, 1);
    let stamp: String = engine
        .connection()
        .query_row("SELECT value FROM project_metadata WHERE key = 'last_build_time'", [], |r| r.get(0))
        .unwrap();
    let noop = engine.refresh(root, None).unwrap();
    assert_eq!(noop.mode, "noop");
    assert!(!noop.published);
    let stamp_after: String = engine
        .connection()
        .query_row("SELECT value FROM project_metadata WHERE key = 'last_build_time'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stamp, stamp_after);

    // Cross-file changes: a callee renamed in one file, its caller updated in another,
    // a file deleted, a file added.
    write(
        root,
        "src/auth.rs",
        "use crate::models::User;\n\npub fn check() -> bool {\n    let u = build_user();\n    validate(&u)\n}\n\npub fn build_user() -> User {\n    User { id: 2 }\n}\n\nfn validate(_u: &User) -> bool {\n    false\n}\n",
    );
    fs::remove_file(root.join("py/views.py")).unwrap();
    write(root, "py/views2.py", "def index():\n    return 'moved'\n");
    write(
        root,
        "web/child.ts",
        "import { Base } from './base';\nimport { extra } from './extra';\n\nexport class Child extends Base {\n  name: Base;\n  run(): number { return extra(); }\n}\n\nexport function make(): Child {\n  return new Child();\n}\n",
    );
    write(root, "web/extra.ts", "export function extra(): number { return 3; }\n");

    let out = engine.refresh(root, None).unwrap();
    assert_eq!(out.mode, "incremental");
    assert!(out.published);
    assert_eq!(out.changes.added, vec!["py/views2.py".to_string(), "web/extra.ts".to_string()]);
    assert_eq!(out.changes.modified, vec!["src/auth.rs".to_string(), "web/child.ts".to_string()]);
    assert_eq!(out.changes.deleted, vec!["py/views.py".to_string()]);
    assert_eq!(out.summary.files_extracted, 4, "only changed files are parsed");
    assert!(out.summary.files_reused >= 5);

    // Equivalent to a full rebuild of the same sources into a fresh database.
    let fresh_db = root.join("fresh").join("graph.db");
    let mut fresh = GraphEngine::open(&fresh_db).unwrap();
    fresh.rebuild(root).unwrap();
    let (n1, e1, r1) = dump(&db(root));
    let (n2, e2, r2) = dump(&fresh_db);
    assert_eq!(n1, n2, "nodes differ");
    assert_eq!(e1, e2, "edges differ");
    assert_eq!(r1, r2, "unresolved refs differ");

    // The cross-file edge was re-resolved to the new target.
    let callees: Vec<String> = engine
        .query_what_calls("Child.run")
        .unwrap()
        .into_iter()
        .map(|n| n.qualified_name)
        .collect();
    assert_eq!(callees, vec!["extra".to_string()]);
    assert_eq!(inspect_status(&db(root), root).status, "fresh");
}

#[test]
fn refresh_after_extractor_metadata_change_reextracts_everything() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let mut engine = rebuild(root);
    engine
        .connection()
        .execute("UPDATE file_extraction_cache SET extractor_version = 'old'", [])
        .unwrap();
    engine
        .connection()
        .execute("UPDATE project_metadata SET value = 'old' WHERE key = 'extractor_version'", [])
        .unwrap();
    assert_eq!(inspect_status(&db(root), root).status, "stale");
    let out = engine.refresh(root, None).unwrap();
    assert_eq!(out.mode, "incremental");
    assert_eq!(out.summary.files_reused, 0);
    assert_eq!(inspect_status(&db(root), root).status, "fresh");
}

// ---------------------------------------------------------------------------
// Status kinds
// ---------------------------------------------------------------------------

#[test]
fn status_kinds_cover_every_state() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    assert_eq!(inspect_status(&db(root), root).status, "missing");
    assert!(!db(root).exists(), "status must not create the database");

    let engine = rebuild(root);
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "fresh", "{:?}", h.diagnostics);
    assert_eq!(h.parse_health.total, 8);

    write(root, "src/models.rs", "pub struct User { pub id: u64 }\n");
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "stale");
    assert_eq!(h.changes.modified, vec!["src/models.rs".to_string()]);
    assert_eq!(h.next_command(), Some("knobyte graph refresh"));
    drop(engine);

    // Syntax errors: fresh but degraded.
    write(root, "src/broken.rs", "pub fn broken( {\n");
    let mut engine = GraphEngine::open(&db(root)).unwrap();
    engine.refresh(root, None).unwrap();
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "degraded");
    assert_eq!(h.parse_health.partial_paths, vec!["src/broken.rs".to_string()]);

    // Older schema lineage: unreadable until upgraded; refresh upgrades it in place.
    engine
        .connection()
        .execute_batch("DELETE FROM schema_versions; INSERT INTO schema_versions VALUES (5, 0, 'v5');")
        .unwrap();
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "stale");
    assert_eq!(h.next_command(), Some("knobyte graph refresh"));
    let out = engine.refresh(root, None).unwrap();
    assert_eq!(out.migrated_from, Some(5));
    assert_eq!(inspect_status(&db(root), root).status, "degraded");
    // Too old to upgrade: refresh refuses, rebuild replaces.
    engine
        .connection()
        .execute_batch("DELETE FROM schema_versions; INSERT INTO schema_versions VALUES (2, 0, 'v2');")
        .unwrap();
    let err = engine.refresh(root, None).unwrap_err();
    assert_eq!(maintenance_error(&err).unwrap().code, "GRAPH_REBUILD_REQUIRED");
    engine.rebuild(root).unwrap();
    assert_eq!(inspect_status(&db(root), root).status, "degraded");
    drop(engine);

    // Garbage bytes: corrupt. Rebuild moves the file aside and recovers.
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{}", db(root).display(), suffix));
    }
    fs::write(db(root), b"this is not a sqlite database, just junk bytes......").unwrap();
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "corrupt");
    assert_eq!(h.next_command(), Some("knobyte graph repair"));
    let err = repair_graph(&db(root), root).unwrap_err();
    assert_eq!(maintenance_error(&err).unwrap().code, "GRAPH_INDEX_NOT_REPAIRABLE");
    let policy = CorpusPolicy::for_project(root);
    let scan = scan_corpus(root, &policy).unwrap();
    let out = rebuild_graph(&db(root), root, &scan, None).unwrap();
    let recovery = PathBuf::from(out.recovery_path.expect("corrupt file retained"));
    assert!(recovery.exists());
    assert_eq!(inspect_status(&db(root), root).status, "degraded");
}

// ---------------------------------------------------------------------------
// Corpus policy & coverage
// ---------------------------------------------------------------------------

#[test]
fn corpus_policy_ignores_skips_and_reports_coverage() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    write(root, "svc/main.go", "package main\n");
    write(root, "svc/util.go", "package main\n");
    write(root, "ui/App.svelte", "<script></script>\n");
    write(root, "generated/big.rs", &format!("pub fn big() {{}}\n{}", "// x\n".repeat(10)));
    write(root, "src/huge.rs", &"// padding\n".repeat(2000));
    write(
        root,
        ".knobyte/config.json",
        r#"{"graph": {"ignore": ["generated/**", "/etc/**", "../x"], "max_file_bytes": 10000}}"#,
    );

    let policy = CorpusPolicy::for_project(root);
    assert_eq!(policy.ignore, vec!["generated/**".to_string()]);
    let scan = scan_corpus(root, &policy).unwrap();
    assert!(scan.files.iter().all(|f| !f.rel_path.starts_with("generated/")));
    assert_eq!(scan.coverage.skipped.len(), 1);
    assert_eq!(scan.coverage.skipped[0].path, "src/huge.rs");
    assert_eq!(scan.coverage.unindexed_total, 3);
    assert_eq!(scan.coverage.unindexed[0].extension, ".go");
    assert_eq!(scan.coverage.unindexed[0].files, 2);

    let out = rebuild_graph(&db(root), root, &scan, None).unwrap();
    assert_eq!(out.coverage.skipped.len(), 1);
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "fresh", "{:?}", h.diagnostics);
    assert_eq!(h.coverage.unwrap().unindexed_total, 3);

    // Changing the policy makes the index stale.
    write(root, ".knobyte/config.json", r#"{"graph": {"ignore": ["py/**"]}}"#);
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "stale");
    assert!(h.changes.policy_changed);

    // The file-count ceiling aborts instead of indexing part of the corpus.
    write(root, ".knobyte/config.json", r#"{"graph": {"max_files": 3}}"#);
    let err = scan_corpus(root, &CorpusPolicy::for_project(root)).unwrap_err();
    assert_eq!(err.limit, "max_files");
}

// ---------------------------------------------------------------------------
// Repair
// ---------------------------------------------------------------------------

#[test]
fn repair_removes_dangling_rows_and_rebuilds_fts() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let engine = rebuild(root);
    let conn = engine.connection();
    conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    conn.execute(
        "INSERT INTO edges (source, target, kind, confidence) VALUES ('function:gone', 'function:missing', 'calls', 1.0)",
        [],
    )
    .unwrap();
    // Desynchronise the FTS index from its content table.
    conn.execute_batch("INSERT INTO nodes_fts(nodes_fts) VALUES('delete-all');")
        .unwrap();
    fs::write(root.join(".knobyte").join("graph.db.candidate-1-2"), b"stale").unwrap();
    drop(engine);

    let report = repair_graph(&db(root), root).unwrap();
    assert_eq!(report.orphan_edges_removed, 1);
    assert!(report.fts_rebuilt);
    assert_eq!(report.stale_candidates_removed, 1);
    assert_eq!(report.integrity_after, "ok");
    assert_eq!(report.status, "fresh");
    let engine = GraphEngine::open(&db(root)).unwrap();
    assert!(count(&engine, "SELECT COUNT(*) FROM nodes_fts WHERE nodes_fts MATCH 'make_user'") >= 1);

    let missing = tempdir().unwrap();
    let err = repair_graph(&db(missing.path()), missing.path()).unwrap_err();
    assert_eq!(maintenance_error(&err).unwrap().code, "GRAPH_INDEX_MISSING");
}

// ---------------------------------------------------------------------------
// Edge & node kinds, resolver conservatism
// ---------------------------------------------------------------------------

fn edges_of_kind(engine: &GraphEngine, kind: &str) -> BTreeSet<(String, String)> {
    let mut stmt = engine
        .connection()
        .prepare(
            "SELECT s.qualified_name, t.qualified_name FROM edges e JOIN nodes s ON s.id = e.source \
             JOIN nodes t ON t.id = e.target WHERE e.kind = ?1",
        )
        .unwrap();
    stmt.query_map([kind], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn has(set: &BTreeSet<(String, String)>, a: &str, b: &str) -> bool {
    set.contains(&(a.to_string(), b.to_string()))
}

#[test]
fn structural_and_type_edges_are_extracted() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    write(
        root,
        "app/api/users/route.ts",
        "export async function GET() { return list(); }\nfunction list() { return []; }\n",
    );
    let engine = rebuild(root);

    let kinds: BTreeSet<String> = {
        let mut stmt = engine.connection().prepare("SELECT DISTINCT kind FROM nodes").unwrap();
        stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    };
    for k in ["enum_member", "namespace", "property", "route"] {
        assert!(kinds.contains(k), "missing node kind {} in {:?}", k, kinds);
    }

    let contains = edges_of_kind(&engine, "contains");
    assert!(has(&contains, "User", "User::id"));
    assert!(has(&contains, "Role", "Role::Admin"));
    assert!(has(&contains, "internal", "internal::helper"));
    assert!(has(&contains, "Color", "Color.Red"));
    assert!(has(&contains, "src/auth.rs", "check"));

    let exports = edges_of_kind(&engine, "exports");
    assert!(has(&exports, "web/child.ts", "Child"));
    assert!(has(&exports, "src/auth.rs", "check"));
    assert!(!has(&exports, "src/auth.rs", "validate"));

    assert!(has(&edges_of_kind(&engine, "extends"), "Child", "Base"));
    assert!(has(&edges_of_kind(&engine, "extends"), "Account", "Model"));
    let overrides = edges_of_kind(&engine, "overrides");
    assert!(has(&overrides, "Child.run", "Base.run"));
    assert!(has(&overrides, "Account.save", "Model.save"));

    let inst = edges_of_kind(&engine, "instantiates");
    assert!(has(&inst, "make_user", "User"), "{:?}", inst);
    assert!(has(&inst, "make", "Child"));
    assert!(has(&inst, "Account.save", "Model"));

    let returns = edges_of_kind(&engine, "returns");
    assert!(has(&returns, "make_user", "User"));
    assert!(has(&returns, "make", "Child"));
    let type_of = edges_of_kind(&engine, "type_of");
    assert!(has(&type_of, "User::role", "Role"));
    assert!(has(&type_of, "Child.name", "Base"));

    assert!(has(&edges_of_kind(&engine, "decorates"), "home", "route"));
    assert!(has(&edges_of_kind(&engine, "references"), "GET /api/users", "GET"));

    // Structural edges carry tree-sitter provenance; resolved ones are lexical.
    let prov: String = engine
        .connection()
        .query_row("SELECT DISTINCT provenance FROM edges WHERE kind = 'contains'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(prov, "tree-sitter");
}

#[test]
fn global_uniqueness_is_never_proof() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    // `helper` is called without any import: the only evidence is a unique project-wide name.
    write(root, "a.py", "def caller():\n    helper()\n    shared()\n");
    write(root, "b.py", "def helper():\n    pass\n");
    // A glob import is real evidence.
    write(root, "c.py", "from .d import *\n\ndef user():\n    shared()\n");
    write(root, "d.py", "def shared():\n    pass\n");
    write(root, "e.py", "def shared():\n    pass\n");
    write(root, "__init__.py", "");
    let engine = rebuild(root);
    let rows: Vec<(String, String, f64, String)> = {
        let mut stmt = engine
            .connection()
            .prepare(
                "SELECT s.name || '->' || t.name || '@' || t.file_path, e.kind, e.confidence, e.provenance \
                 FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target \
                 WHERE e.kind IN ('calls', 'possible_call')",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    let find = |k: &str| rows.iter().find(|r| r.0 == k).cloned();
    let guess = find("caller->helper@b.py").expect("heuristic edge kept for discovery");
    assert_eq!(guess.1, "possible_call");
    assert!(guess.2 <= 0.5);
    assert_eq!(guess.3, "heuristic");
    // Two `shared` candidates and no import: no edge at all, recorded as ambiguous.
    assert!(rows.iter().all(|r| !r.0.starts_with("caller->shared")));
    let ambiguous = count(
        &engine,
        "SELECT COUNT(*) FROM unresolved_refs WHERE reference_name = 'shared' AND status = 'ambiguous'",
    );
    assert!(ambiguous >= 1);
    // The glob import proves the target.
    let proven = find("user->shared@d.py").expect("glob import resolves");
    assert_eq!(proven.1, "calls");
    assert_eq!(proven.3, "lexical");
}

// ---------------------------------------------------------------------------
// Query & CLI parity
// ---------------------------------------------------------------------------

#[test]
fn impact_depth_callers_only_and_groundings() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    write(
        root,
        ".knobyte/specs/auth.md",
        "---\nid: kb_auth\ntitle: Auth\ntype: spec\ngrounds_to:\n  - function:src/auth.rs:check\n---\n# Auth\n",
    );
    write(
        root,
        ".knobyte/specs/models.md",
        "---\nid: kb_models\ntitle: Models\ntype: spec\ngrounds_to:\n  - struct:src/models.rs:User\n---\n# Models\n",
    );
    let engine = rebuild(root);

    let names = |r: &knobyte::graph::ImpactReport| -> BTreeSet<String> {
        r.impacted.iter().map(|e| e.node.qualified_name.clone()).collect()
    };
    let shallow = engine
        .impact("make_user", ImpactOptions { depth: 1, callers_only: true })
        .unwrap();
    assert_eq!(names(&shallow), ["check".to_string()].into_iter().collect());
    let deep = engine
        .impact("make_user", ImpactOptions { depth: 3, callers_only: true })
        .unwrap();
    assert!(names(&deep).contains("entry"));
    assert!(deep.impacted.iter().all(|e| e.via != "contains"));

    // All dependents of `User` include type users, not only callers.
    let all = engine
        .impact("User", ImpactOptions { depth: 1, callers_only: false })
        .unwrap();
    assert!(names(&all).contains("make_user"));
    let callers = engine
        .impact("User", ImpactOptions { depth: 1, callers_only: true })
        .unwrap();
    assert!(names(&callers).contains("make_user"));
    assert!(!names(&callers).contains("validate"), "type-only use is not a call");

    let with_docs = engine
        .impact_with_groundings(
            "make_user",
            ImpactOptions { depth: 2, callers_only: true },
            &root.join(".knobyte"),
        )
        .unwrap();
    let docs: BTreeSet<String> = with_docs.groundings.iter().map(|g| g.doc.clone()).collect();
    assert_eq!(docs, ["specs/auth.md".to_string()].into_iter().collect());

    // CLI: JSONL records with grounding lines.
    let out = Command::new(bin())
        .args(["impact", "make_user", "--depth", "2", "--callers-only", "--jsonl", "--root"])
        .arg(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"type\":\"caller\""));
    assert!(text.contains("\"type\":\"grounding\"") && text.contains("specs/auth.md"));
}

#[test]
fn what_calls_get_source_and_cli_status() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    // Bare `knobyte graph --root` builds.
    let out = Command::new(bin())
        .args(["graph", "--json", "--root"])
        .arg(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "fresh");

    let engine = GraphEngine::open(&db(root)).unwrap();
    let callees: BTreeSet<String> = engine
        .query_what_calls("check")
        .unwrap()
        .into_iter()
        .map(|n| n.qualified_name)
        .collect();
    assert_eq!(
        callees,
        ["make_user".to_string(), "validate".to_string()].into_iter().collect()
    );

    let check_id = engine.query_where_defined("check").unwrap()[0].id.clone();
    let got = engine
        .get_with_source(root, &[check_id.clone(), "function:src/auth.rs:make_user".into()], 2)
        .unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].source.as_deref(), Some("pub fn check() -> bool {\n    let u = make_user();"));
    assert!(got[0].truncated);
    assert!(!got[0].stale);
    assert_eq!(got[1].node.name, "make_user");

    write(root, "src/auth.rs", "// edited\n");
    let got = engine.get_with_source(root, &[check_id], 400).unwrap();
    assert!(got[0].stale);

    let out = Command::new(bin())
        .args(["graph", "status", "--json", "--root"])
        .arg(root)
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "stale");
    assert_eq!(v["changes"]["modified"][0], "src/auth.rs");

    let out = Command::new(bin())
        .args(["graph", "refresh", "--json", "--root"])
        .arg(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["mode"], "incremental");
    assert_eq!(v["files_extracted"], 1);
}

#[test]
fn refresh_graph_without_index_builds_fully() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let scan = scan_corpus(root, &CorpusPolicy::for_project(root)).unwrap();
    let out = refresh_graph(&db(root), root, &scan, None).unwrap();
    assert_eq!(out.mode, "full");
    assert_eq!(inspect_status(&db(root), root).status, "fresh");
}
