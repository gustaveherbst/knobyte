use knobyte::graph::{GraphEngine, Node, RefResolution};
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

/// (local, imported, module, resolved_file, target qualified name)
type BindingRow = (String, String, String, Option<String>, Option<String>);

fn bindings(engine: &GraphEngine, file: &str) -> Vec<BindingRow> {
    let mut stmt = engine
        .connection()
        .prepare(
            "SELECT b.local_name, b.imported_name, b.module_specifier, b.resolved_file_path, t.qualified_name
             FROM import_bindings b LEFT JOIN nodes t ON t.id = b.target_id
             WHERE b.file_path = ?1 ORDER BY b.binding_key",
        )
        .unwrap();
    stmt.query_map([file], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

fn find_binding<'a>(rows: &'a [BindingRow], local: &str) -> &'a BindingRow {
    rows.iter()
        .find(|b| b.0 == local)
        .unwrap_or_else(|| panic!("no binding {} in {:?}", local, rows))
}

/// (source qualified, target qualified, edge kind)
fn call_edges(engine: &GraphEngine) -> Vec<(String, String, String)> {
    let mut stmt = engine
        .connection()
        .prepare(
            "SELECT s.qualified_name, t.qualified_name, e.kind FROM edges e
             JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target
             WHERE e.kind IN ('calls', 'calls_trait_method', 'possible_call', 'instantiates')",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn node(engine: &GraphEngine, name: &str) -> Node {
    let nodes = engine.query_where_defined(name).unwrap();
    assert_eq!(
        nodes.len(),
        1,
        "expected one node named {}: {:?}",
        name,
        nodes
    );
    nodes.into_iter().next().unwrap()
}

#[test]
fn rust_imports_are_parsed_into_bindings() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/lib.rs", "pub mod auth;\npub mod graph;\n");
    write(
        root,
        "src/auth.rs",
        "pub struct Session;\npub fn validate() -> bool { true }\n",
    );
    write(
        root,
        "src/graph/mod.rs",
        "pub mod engine;\npub use engine::Engine;\n",
    );
    write(root, "src/graph/engine.rs", "pub struct Engine;\n");
    write(
        root,
        "src/app.rs",
        "use crate::auth::{validate as check, Session};\nuse std::collections::HashMap;\nuse crate::graph;\nuse crate::graph::Engine;\nuse super::auth::*;\n\npub fn run() { check(); }\n",
    );
    let engine = build(root);
    let b = bindings(&engine, "src/app.rs");

    let check = find_binding(&b, "check");
    assert_eq!(check.1, "validate");
    assert_eq!(check.2, "crate::auth");
    assert_eq!(check.3.as_deref(), Some("src/auth.rs"));
    assert_eq!(check.4.as_deref(), Some("validate"));

    let session = find_binding(&b, "Session");
    assert_eq!(session.4.as_deref(), Some("Session"));

    let hm = find_binding(&b, "HashMap");
    assert_eq!(hm.2, "std::collections");
    assert!(hm.3.is_none());

    // `use crate::graph;` binds the module file.
    let graph = find_binding(&b, "graph");
    assert_eq!(graph.3.as_deref(), Some("src/graph/mod.rs"));
    // Re-export through graph/mod.rs resolves to the defining file.
    let eng = find_binding(&b, "Engine");
    assert_eq!(eng.3.as_deref(), Some("src/graph/engine.rs"));
    // Wildcard
    assert!(b.iter().any(|r| r.1 == "*" && r.2 == "super::auth"));

    // No node is named after raw statement text or just "crate".
    let crate_nodes: i64 = engine
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE name = 'crate' OR name LIKE 'use %'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(crate_nodes, 0);

    // External module node carries the importer's language; import edges start at the file node.
    let (lang, src_kind): (String, String) = engine
        .connection()
        .query_row(
            "SELECT t.language, s.kind FROM edges e JOIN nodes t ON t.id = e.target JOIN nodes s ON s.id = e.source
             WHERE e.kind = 'imports' AND t.name = 'std::collections'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(lang, "rust");
    assert_eq!(src_kind, "file");

    // The aliased import is used to resolve the call.
    assert!(call_edges(&engine).contains(&("run".into(), "validate".into(), "calls".into())));
    let who = engine.query_who_imports("src/auth.rs").unwrap();
    assert!(who.iter().any(|n| n.file_path == "src/app.rs"));
}

#[test]
fn typescript_and_javascript_imports_are_parsed() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "web/util.ts", "export function helper() {}\nexport const shout = (s: string) => s.toUpperCase();\nexport default class Thing {}\n");
    write(root, "web/types.ts", "export type Id = string;\n");
    write(
        root,
        "web/app.ts",
        "import { helper, shout as yell } from './util';\nimport Thing from './util';\nimport * as ns from './util';\nimport type { Id } from './types';\nimport React from 'react';\nimport './polyfill';\nexport { helper as reexported } from './util';\n\nexport function main(id: Id) {\n  helper();\n  yell('x');\n  ns.helper();\n  return new Thing();\n}\n",
    );
    write(root, "web/legacy.js", "const fs = require('fs');\nconst { helper } = require('./util');\nfunction go() { helper(); fs.readFileSync('x'); }\n");
    let engine = build(root);

    let b = bindings(&engine, "web/app.ts");
    let h = find_binding(&b, "helper");
    assert_eq!(h.1, "helper");
    assert_eq!(h.2, "./util");
    assert_eq!(h.3.as_deref(), Some("web/util.ts"));
    assert_eq!(h.4.as_deref(), Some("helper"));
    assert_eq!(find_binding(&b, "yell").1, "shout");
    assert_eq!(find_binding(&b, "Thing").1, "default");
    assert_eq!(find_binding(&b, "ns").1, "*");
    assert_eq!(find_binding(&b, "Id").3.as_deref(), Some("web/types.ts"));
    assert_eq!(find_binding(&b, "React").2, "react");
    assert!(find_binding(&b, "React").3.is_none());
    assert_eq!(find_binding(&b, "reexported").1, "helper");
    assert!(b.iter().any(|r| r.2 == "./polyfill" && r.0.is_empty()));

    let type_only: i64 = engine
        .connection()
        .query_row(
            "SELECT is_type_only FROM import_bindings WHERE local_name = 'Id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(type_only, 1);

    let js = bindings(&engine, "web/legacy.js");
    assert_eq!(find_binding(&js, "fs").2, "fs");
    assert_eq!(
        find_binding(&js, "helper").3.as_deref(),
        Some("web/util.ts")
    );

    // Module nodes are tagged with the importing language, not "rust".
    let react_lang: String = engine
        .connection()
        .query_row(
            "SELECT language FROM nodes WHERE kind = 'module' AND name = 'react'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(react_lang, "typescript");
    let fs_lang: String = engine
        .connection()
        .query_row(
            "SELECT language FROM nodes WHERE kind = 'module' AND name = 'fs'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(fs_lang, "javascript");

    let edges = call_edges(&engine);
    assert!(
        edges.contains(&("main".into(), "helper".into(), "calls".into())),
        "{:?}",
        edges
    );
    assert!(
        edges.contains(&("main".into(), "shout".into(), "calls".into())),
        "{:?}",
        edges
    );
    assert!(
        edges.contains(&("go".into(), "helper".into(), "calls".into())),
        "{:?}",
        edges
    );
    assert!(
        edges.contains(&("main".into(), "Thing".into(), "instantiates".into())),
        "{:?}",
        edges
    );
}

#[test]
fn python_imports_are_parsed() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "pkg/__init__.py", "");
    write(
        root,
        "pkg/models.py",
        "class User:\n    pass\n\ndef load():\n    return User()\n",
    );
    write(root, "pkg/sub/thing.py", "def thing():\n    pass\n");
    write(root, "pkg/helpers.py", "def assist():\n    pass\n");
    write(
        root,
        "pkg/service.py",
        "import os.path\nimport numpy as np\nfrom .models import User, load as fetch\nfrom pkg.sub.thing import thing as t\nfrom . import helpers\nfrom os import *\n\ndef serve():\n    fetch()\n    t()\n    helpers.assist()\n    return User()\n",
    );
    let engine = build(root);
    let b = bindings(&engine, "pkg/service.py");

    let os = find_binding(&b, "os");
    assert_eq!(os.2, "os.path");
    assert!(os.3.is_none());
    assert_eq!(find_binding(&b, "np").2, "numpy");
    let user = find_binding(&b, "User");
    assert_eq!(user.2, ".models");
    assert_eq!(user.3.as_deref(), Some("pkg/models.py"));
    assert_eq!(user.4.as_deref(), Some("User"));
    assert_eq!(find_binding(&b, "fetch").1, "load");
    assert_eq!(find_binding(&b, "t").3.as_deref(), Some("pkg/sub/thing.py"));
    assert_eq!(
        find_binding(&b, "helpers").3.as_deref(),
        Some("pkg/helpers.py")
    );
    assert!(b.iter().any(|r| r.1 == "*" && r.2 == "os"));

    let lang: String = engine
        .connection()
        .query_row(
            "SELECT language FROM nodes WHERE kind = 'module' AND name = 'numpy'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(lang, "python");

    let edges = call_edges(&engine);
    for (target, kind) in [
        ("load", "calls"),
        ("thing", "calls"),
        ("assist", "calls"),
        ("User", "instantiates"),
    ] {
        assert!(
            edges.contains(&("serve".into(), target.into(), kind.into())),
            "serve -> {} missing in {:?}",
            target,
            edges
        );
    }
}

#[test]
fn methods_are_labelled_consistently_across_languages() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        "/// A widget.\n#[derive(Debug, Clone)]\n#[allow(dead_code)]\npub struct Widget;\n\nimpl Widget {\n    /// Make one.\n    #[inline]\n    pub fn create() -> Self { Widget }\n    fn render(&self) -> String { String::new() }\n}\n\npub fn free() -> u8 { 1 }\n",
    );
    write(
        root,
        "web/a.ts",
        "export abstract class Shape {\n  abstract area(): number;\n  describe(): string { return 'x'; }\n  static make = () => 1;\n}\nexport type Id = string;\nexport enum Color { Red, Green }\nexport const arrow = async (x: number): Promise<number> => x;\nconst expr = function () { return 1; };\nfunction plain() {}\ninterface Props { a: string }\n",
    );
    write(root, "py/m.py", "class Repo:\n    def save(self) -> None:\n        pass\n\n    @staticmethod\n    def build():\n        pass\n\ndef helper():\n    pass\n");
    let engine = build(root);

    let kind = |name: &str| node(&engine, name).kind;
    assert_eq!(kind("create"), "method");
    assert_eq!(kind("render"), "method");
    assert_eq!(kind("free"), "function");
    assert_eq!(kind("area"), "method");
    assert_eq!(kind("describe"), "method");
    assert_eq!(kind("make"), "method");
    assert_eq!(kind("arrow"), "function");
    assert_eq!(kind("expr"), "function");
    assert_eq!(kind("plain"), "function");
    assert_eq!(kind("Id"), "type_alias");
    assert_eq!(kind("Color"), "enum");
    assert_eq!(kind("Shape"), "class");
    assert_eq!(kind("Props"), "interface");
    assert_eq!(kind("save"), "method");
    assert_eq!(kind("build"), "method");
    assert_eq!(kind("helper"), "function");

    assert!(node(&engine, "Shape").is_abstract);
    assert!(node(&engine, "area").is_abstract);
    assert!(node(&engine, "arrow").is_async);
    assert!(node(&engine, "arrow").is_exported);
    assert!(node(&engine, "build").is_static);

    // Rust doc comments survive attributes between comment and item.
    assert_eq!(
        node(&engine, "Widget").docstring.as_deref(),
        Some("/// A widget.")
    );
    assert_eq!(
        node(&engine, "create").docstring.as_deref(),
        Some("/// Make one.")
    );

    // container_id / visibility / return_type are populated.
    let widget = node(&engine, "Widget");
    let render = node(&engine, "render");
    assert_eq!(render.container_id.as_deref(), Some(widget.id.as_str()));
    assert_eq!(render.visibility.as_deref(), Some("private"));
    assert_eq!(render.return_type.as_deref(), Some("String"));
    assert_eq!(node(&engine, "create").visibility.as_deref(), Some("pub"));
    assert!(node(&engine, "create").is_static);
    assert_eq!(node(&engine, "save").return_type.as_deref(), Some("None"));
    assert_eq!(
        node(&engine, "save").container_id,
        Some(node(&engine, "Repo").id)
    );

    // Readable grounding refs: `function` matches methods.
    match engine.resolve_ref("function:src/lib.rs:render").unwrap() {
        RefResolution::Resolved(n) => assert_eq!(n.qualified_name, "Widget::render"),
        other => panic!("unexpected {:?}", other),
    }
    assert!(engine
        .resolve_ref("method:py/m.py:Repo.save")
        .unwrap()
        .node()
        .is_some());
    assert!(engine
        .resolve_ref("function:py/m.py:Repo::save")
        .unwrap()
        .node()
        .is_some());
    assert!(engine
        .resolve_ref("function:src/lib.rs:nope")
        .unwrap()
        .node()
        .is_none());
}

#[test]
fn call_resolution_does_not_fan_out() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/a.rs",
        "pub struct Alpha;\nimpl Alpha {\n    pub fn new() -> Self { Alpha }\n    pub fn run(&self) { self.step(); }\n    fn step(&self) {}\n    pub fn process(&self) {}\n}\n",
    );
    write(
        root,
        "src/b.rs",
        "pub struct Beta;\nimpl Beta {\n    pub fn new() -> Self { Beta }\n    pub fn run(&self) { self.step(); }\n    fn step(&self) {}\n    pub fn process(&self) {}\n}\npub fn shared() {}\n",
    );
    write(
        root,
        "src/c.rs",
        "pub struct Gamma;\nimpl Gamma { pub fn new() -> Self { Gamma } }\npub fn shared() {}\n",
    );
    write(
        root,
        "src/main.rs",
        "use crate::a::Alpha;\n\nfn main() {\n    let a = Alpha::new();\n    let v = Vec::new();\n    a.process();\n    shared();\n    a.run();\n}\n",
    );
    let mut engine = GraphEngine::open(&root.join("graph.db")).unwrap();
    let summary = engine.rebuild(root).unwrap();
    let edges = call_edges(&engine);

    // Qualified path call resolves to exactly one `new`.
    let news: Vec<_> = edges
        .iter()
        .filter(|e| e.0 == "main" && e.1.ends_with("::new"))
        .collect();
    assert_eq!(
        news,
        vec![&(
            "main".to_string(),
            "Alpha::new".to_string(),
            "calls".to_string()
        )]
    );

    // Same-named methods in different impls each call their own `step` (no caller collapse).
    assert!(edges.contains(&("Alpha::run".into(), "Alpha::step".into(), "calls".into())));
    assert!(edges.contains(&("Beta::run".into(), "Beta::step".into(), "calls".into())));
    assert!(!edges.contains(&("Alpha::run".into(), "Beta::step".into(), "calls".into())));
    assert!(!edges.contains(&("Beta::run".into(), "Alpha::step".into(), "calls".into())));

    // Ambiguous receiver / name: recorded as unresolved, not linked to all candidates.
    assert!(!edges
        .iter()
        .any(|e| e.0 == "main" && e.1.ends_with("process")));
    assert!(!edges.iter().any(|e| e.0 == "main" && e.1 == "shared"));
    let ambiguous: i64 = engine
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM unresolved_refs WHERE status = 'ambiguous' AND reference_name IN ('process', 'shared')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ambiguous, 2);
    // `Vec::new()` is external: unresolved, never linked to a project `new`.
    let vec_new: i64 = engine
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM unresolved_refs WHERE reference_name = 'new' AND qualifier = 'Vec' AND status = 'unresolved'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(vec_new, 1);
    // No Rust method call is mislabelled as a trait call.
    assert!(!edges.iter().any(|e| e.2 == "calls_trait_method"));

    // who-calls still reports the ambiguous caller.
    assert!(engine
        .query_who_calls("process")
        .unwrap()
        .iter()
        .any(|n| n.name == "main"));

    // edges_indexed counts rows actually inserted.
    let edge_rows: i64 = engine
        .connection()
        .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
        .unwrap();
    assert_eq!(summary.edges_indexed as i64, edge_rows);
}

#[test]
fn status_up_to_date_tracks_disk_changes() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.rs", "pub fn a() {}\n");
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    assert!(!engine.status().unwrap().up_to_date, "never built");
    engine.rebuild(root).unwrap();
    assert!(engine.status().unwrap().up_to_date);

    write(root, "src/a.rs", "pub fn a() { let _changed = 1; }\n");
    assert!(!engine.status().unwrap().up_to_date, "modified file");
    engine.rebuild(root).unwrap();
    assert!(engine.status().unwrap().up_to_date);

    write(root, "src/b.rs", "pub fn b() {}\n");
    assert!(!engine.status().unwrap().up_to_date, "new file");
    engine.rebuild(root).unwrap();
    assert!(engine.status().unwrap().up_to_date);

    fs::remove_file(root.join("src/b.rs")).unwrap();
    assert!(!engine.status().unwrap().up_to_date, "deleted file");
}

#[test]
fn scope_expands_to_callees() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/holds.rs",
        "pub fn lift_contact_hold() { audit_release(); }\nfn audit_release() {}\n",
    );
    let engine = build(root);
    let scoped = engine.query_scope_explained("lift a hold").unwrap();
    let callee = scoped
        .iter()
        .find(|s| s.node.name == "audit_release")
        .expect("callee should be in scope");
    assert!(
        callee.reason.contains("called by `lift_contact_hold`"),
        "{}",
        callee.reason
    );
    assert!(scoped
        .iter()
        .all(|s| s.node.kind != "file" && s.node.kind != "module"));
}

#[test]
fn calls_inside_macro_arguments_create_edges() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        r#"pub mod auth {
    pub fn validate_token(t: &str) -> bool { !t.is_empty() }
}
pub struct Session;
impl Session {
    pub fn user(&self) -> &str { "u" }
}
fn describe(s: &Session) -> String {
    format!("{}: {}", s.user(), auth::validate_token("x"))
}
pub fn report(s: &Session) {
    println!("{}", describe(s));
    assert!(crate::auth::validate_token("y"), "{}", format!("{}", 1));
}
"#,
    );
    let engine = build(root);
    let callers = |name: &str| -> Vec<String> {
        let mut stmt = engine
            .connection()
            .prepare(
                "SELECT DISTINCT s.name FROM edges e
                 JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target
                 WHERE t.name = ?1 AND e.kind LIKE 'calls%' ORDER BY s.name",
            )
            .unwrap();
        stmt.query_map([name], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert_eq!(callers("describe"), vec!["report"]);
    assert_eq!(callers("validate_token"), vec!["describe", "report"]);
    // `s.user()` has an untyped receiver; like the same call outside a macro it stays unresolved
    // rather than guessing.
    // Nested macro names such as `format!` are not calls.
    assert!(callers("format").is_empty());
}
