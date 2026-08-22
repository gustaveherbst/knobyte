//! Graph regression tests from the completeness review: default imports through re-export
//! barrels, graph error/status messages, `.knobyte/.gitignore` creation, interface member
//! signatures and TypeScript package discovery.

use knobyte::graph::GraphEngine;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn build(root: &Path) -> GraphEngine {
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    engine.rebuild(root).unwrap();
    engine
}

/// (source qualified, target qualified, kind, resolution method) of every edge of `kinds`.
fn edges(engine: &GraphEngine, kinds: &[&str]) -> Vec<(String, String, String, String)> {
    let mut stmt = engine
        .connection()
        .prepare(
            "SELECT s.qualified_name, t.qualified_name, e.kind, COALESCE(e.resolution_method, '') FROM edges e
             JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .filter(|e: &(String, String, String, String)| kinds.contains(&e.2.as_str()))
        .collect()
}

fn has_edge(all: &[(String, String, String, String)], from: &str, to: &str, kind: &str) -> bool {
    all.iter().any(|e| e.0 == from && e.1 == to && e.2 == kind)
}

#[test]
fn default_imports_resolve_through_reexport_barrels() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/util.ts", "export default function defFn() { return 1; }\n");
    // `export { default } from` barrel, and a barrel of that barrel.
    write(root, "src/b1.ts", "export { default } from './util';\n");
    write(root, "src/b2.ts", "export { default } from './b1';\n");
    // Import-then-export-default barrel.
    write(root, "src/b3.ts", "import x from './util';\nexport default x;\n");
    // Cyclic barrels must not loop.
    write(root, "src/c1.ts", "export { default } from './c2';\n");
    write(root, "src/c2.ts", "export { default } from './c1';\n");
    write(
        root,
        "src/use.ts",
        "import b from './b1';\nimport b2 from './b2';\nimport b3 from './b3';\nimport c from './c1';\n\
         export function viaB1() { b(); }\nexport function viaB2() { b2(); }\nexport function viaB3() { b3(); }\nexport function viaCycle() { c(); }\n",
    );
    let engine = build(root);
    let calls = edges(&engine, &["calls"]);
    assert!(has_edge(&calls, "viaB1", "defFn", "calls"), "{:?}", calls);
    assert!(has_edge(&calls, "viaB2", "defFn", "calls"), "{:?}", calls);
    assert!(has_edge(&calls, "viaB3", "defFn", "calls"), "{:?}", calls);
    assert!(!calls.iter().any(|e| e.0 == "viaCycle"), "{:?}", calls);
}

fn knobyte(root: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_knobyte"))
        .args(args)
        .current_dir(root)
        .env("NO_COLOR", "1")
        .env("HOME", root)
        .output()
        .unwrap()
}

fn out_text(o: &std::process::Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn corrupt_database_message_has_a_sentence_separator() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/auth.ts", "export function login() { return 1; }\n");
    fs::create_dir_all(root.join(".knobyte")).unwrap();
    fs::write(root.join(".knobyte/graph.db"), b"not a sqlite database at all ".repeat(100)).unwrap();
    for args in [
        vec!["graph", "query", "where-defined", "login", "--jsonl"],
        vec!["graph", "query", "where-defined", "login"],
    ] {
        let o = knobyte(root, &args);
        let t = out_text(&o);
        assert!(!o.status.success(), "{:?}: {}", args, t);
        assert!(t.contains("No graph-derived"), "{:?}: {}", args, t);
        for (i, _) in t.match_indices("No graph-derived") {
            let before = t[..i].trim_end();
            assert!(before.ends_with(['.', '!', '?']), "{:?}: missing separator: {}", args, t);
        }
    }
}

#[test]
fn max_total_bytes_is_configurable_and_named_in_its_error() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.ts", &"export const x = 1;\n".repeat(50));
    write(root, ".knobyte/config.json", r#"{ "graph": { "max_total_bytes": 100 } }"#);
    let policy = knobyte::graph::CorpusPolicy::for_project(root);
    assert_eq!(policy.max_total_bytes, 100);
    let err = knobyte::graph::scan_corpus(root, &policy).unwrap_err();
    assert_eq!(err.limit, "max_total_bytes");
    let msg = err.to_string();
    assert!(msg.contains("\"graph.max_total_bytes\""), "{}", msg);
    assert!(!msg.contains("graph.max_files"), "{}", msg);
    let e = knobyte::graph::corpus::CorpusLimitError { limit: "max_files", observed: 2, allowed: 1 };
    assert!(e.to_string().contains("\"graph.max_files\""));
}

#[test]
fn migratable_schema_reports_stale_with_refresh_recovery() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/auth.ts", "export function login() { return 1; }\n");
    let engine = build(root);
    engine
        .connection()
        .execute_batch("DELETE FROM schema_versions; INSERT INTO schema_versions VALUES (5, 0, 'v5');")
        .unwrap();
    drop(engine);
    let h = knobyte::graph::inspect_status(&root.join(".knobyte/graph.db"), root);
    assert_eq!(h.status, "stale");
    assert_eq!(h.next_command(), Some("knobyte graph refresh"));
    assert!(h.diagnostics.iter().all(|d| d.code != "GRAPH_INDEX_SCHEMA_INCOMPATIBLE"));
    // Reads refuse the older layout, pointing at the same recovery command.
    let o = knobyte(root, &["graph", "query", "where-defined", "login", "--jsonl"]);
    let t = out_text(&o);
    assert!(!o.status.success(), "{}", t);
    assert!(t.contains("\"graphStatus\":\"stale\""), "{}", t);
    assert!(t.contains("knobyte graph refresh") && !t.contains("rebuild_required"), "{}", t);
    // And `knobyte graph refresh` recovers it.
    let o = knobyte(root, &["graph", "refresh", "--json", "--root", root.to_str().unwrap()]);
    assert!(o.status.success(), "{}", out_text(&o));
    assert_eq!(knobyte::graph::inspect_status(&root.join(".knobyte/graph.db"), root).status, "fresh");
}

#[test]
fn bare_graph_without_scaffold_writes_a_gitignore_for_derived_files() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.ts", "export function a() {}\n");
    let o = knobyte(root, &["graph"]);
    assert!(o.status.success(), "{}", out_text(&o));
    assert!(root.join(".knobyte/graph.db").is_file());
    let ignore = fs::read_to_string(root.join(".knobyte/.gitignore")).expect(".knobyte/.gitignore");
    for p in ["graph.db*", "cozo.db*"] {
        assert!(ignore.lines().any(|l| l.trim() == p), "{}", ignore);
    }
    let git = std::process::Command::new("git").args(["init", "-q"]).current_dir(root).status();
    if git.map(|s| s.success()).unwrap_or(false) {
        let out = std::process::Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=all", ".knobyte"])
            .current_dir(root)
            .output()
            .unwrap();
        let listed: Vec<String> =
            String::from_utf8_lossy(&out.stdout).lines().map(|l| l[3..].to_string()).collect();
        assert_eq!(listed, vec![".knobyte/.gitignore".to_string()], "{:?}", listed);
    }
}
