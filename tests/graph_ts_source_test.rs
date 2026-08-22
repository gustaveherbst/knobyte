//! TypeScript / JavaScript source-only extraction gaps (always run; no Node or TypeScript
//! needed): module-level constants and variables, anonymous callback nodes, `export { default
//! as X } from` re-exports and namespace-qualified members.

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

/// (kind, qualified name, is_exported, signature) of every node of a file.
fn nodes(engine: &GraphEngine, file: &str) -> Vec<(String, String, bool, Option<String>)> {
    let mut stmt = engine
        .connection()
        .prepare("SELECT kind, qualified_name, is_exported, signature FROM nodes WHERE file_path = ?1 ORDER BY start_line, start_column")
        .unwrap();
    stmt.query_map([file], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get(3)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
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
fn module_level_constants_and_variables_are_nodes() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/config.ts",
        "export const MAX_RETRIES: number = 3;\nexport let current = load();\nconst local = { a: 1 };\nvar legacy = 2;\nconst fs = require('fs');\nexport const { a, b } = local;\nexport const make = () => 1;\nfunction load() { const inner = 1; return inner; }\n",
    );
    let engine = build(root);
    let ns = nodes(&engine, "src/config.ts");
    let find = |q: &str| ns.iter().find(|n| n.1 == q).unwrap_or_else(|| panic!("no node {} in {:?}", q, ns));
    let max = find("MAX_RETRIES");
    assert_eq!(max.0, "constant");
    assert!(max.2, "exported const is exported");
    assert!(max.3.as_deref().unwrap_or("").starts_with("const MAX_RETRIES: number = 3"), "{:?}", max.3);
    assert_eq!(find("current").0, "variable");
    assert_eq!(find("local").0, "constant");
    assert!(!find("local").2);
    assert_eq!(find("legacy").0, "variable");
    assert_eq!(find("make").0, "function");
    // `require` imports, destructuring patterns and function-local bindings are not nodes.
    assert!(ns.iter().all(|n| n.1 != "fs" && n.1 != "a" && n.1 != "inner"), "{:?}", ns);
    // The initializer's call is made by the binding.
    let calls = edges(&engine, &["calls"]);
    assert!(has_edge(&calls, "current", "load", "calls"), "{:?}", calls);
    let exports = edges(&engine, &["exports"]);
    assert!(has_edge(&exports, "src/config.ts", "MAX_RETRIES", "exports"), "{:?}", exports);
}

#[test]
fn anonymous_callbacks_are_function_nodes() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/app.ts",
        "function audit(x: number) { return x; }\nexport function run(items: number[]) {\n  items.forEach((item) => audit(item));\n  items.map(function (i) { return audit(i); });\n  return () => audit(0);\n}\nsetTimeout(() => audit(1), 10);\nsetTimeout(() => audit(2), 20);\nexport const handler = wrap(async (req: string) => audit(3));\nfunction wrap<T>(f: T): T { return f; }\n",
    );
    let engine = build(root);
    let ns = nodes(&engine, "src/app.ts");
    let q: Vec<&str> = ns.iter().map(|n| n.1.as_str()).collect();
    for expected in [
        "<callback:items.forEach[0]>",
        "<callback:items.map[0]>",
        "<callback:return_statement>",
        "<callback:setTimeout[0]>",
        "<callback:setTimeout[0]>#2",
        "<callback:wrap[0]>",
    ] {
        assert!(q.contains(&expected), "missing {} in {:?}", expected, q);
    }
    let all = edges(&engine, &["calls", "contains"]);
    assert!(has_edge(&all, "<callback:items.forEach[0]>", "audit", "calls"), "{:?}", all);
    assert!(has_edge(&all, "<callback:setTimeout[0]>#2", "audit", "calls"), "{:?}", all);
    assert!(has_edge(&all, "run", "<callback:items.map[0]>", "contains"), "{:?}", all);
    // The callback of a module-level binding's initializer is contained by the binding.
    assert!(has_edge(&all, "handler", "<callback:wrap[0]>", "contains"), "{:?}", all);
    // The enclosing function no longer claims the callback's calls.
    assert!(!has_edge(&all, "run", "audit", "calls"), "{:?}", all);
}

#[test]
fn default_reexports_resolve_to_the_default_export() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/widgets/button.ts", "export default function renderButton(label: string) { return label; }\n");
    write(root, "src/widgets/card.ts", "function renderCard() { return 1; }\nexport default renderCard;\n");
    write(root, "src/widgets/anon.ts", "export default () => 42;\n");
    write(
        root,
        "src/widgets/index.ts",
        "export { default as Button } from './button';\nexport { default as Card } from './card';\nexport { default as Anon } from './anon';\n",
    );
    write(
        root,
        "src/main.ts",
        "import { Button, Card, Anon } from './widgets';\nimport renderCardDefault from './widgets/card';\nexport function main() {\n  Button('ok');\n  Card();\n  Anon();\n  renderCardDefault();\n}\n",
    );
    let engine = build(root);
    let calls = edges(&engine, &["calls"]);
    assert!(has_edge(&calls, "main", "renderButton", "calls"), "{:?}", calls);
    assert!(has_edge(&calls, "main", "renderCard", "calls"), "{:?}", calls);
    assert!(has_edge(&calls, "main", "default", "calls"), "{:?}", calls);
    let default_call = calls
        .iter()
        .filter(|e| e.0 == "main" && e.1 == "renderCard")
        .map(|e| e.3.as_str())
        .collect::<Vec<_>>();
    assert!(default_call.contains(&"default-export"), "{:?}", default_call);
    // `default_export` facts never become edges.
    assert!(edges(&engine, &["default_export"]).is_empty());
}

#[test]
fn namespace_members_are_qualified_and_resolve() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/ns.ts",
        "export namespace Geometry {\n  export function area(r: number) { return helper(r) * r; }\n  function helper(r: number) { return r; }\n  export class Shape {}\n  export namespace Inner {\n    export function deep() { return 1; }\n  }\n  export const UNIT = 1;\n}\nexport function useIt() {\n  Geometry.area(2);\n  Geometry.Inner.deep();\n}\n",
    );
    write(
        root,
        "src/consumer.ts",
        "import { Geometry } from './ns';\nimport { Geometry as G } from './ns';\nexport class Square extends Geometry.Shape {}\nexport function consume() {\n  Geometry.area(1);\n  G.Inner.deep();\n}\n",
    );
    let engine = build(root);
    let ns = nodes(&engine, "src/ns.ts");
    let q: Vec<&str> = ns.iter().map(|n| n.1.as_str()).collect();
    for expected in [
        "Geometry",
        "Geometry.area",
        "Geometry.helper",
        "Geometry.Shape",
        "Geometry.Inner",
        "Geometry.Inner.deep",
        "Geometry.UNIT",
    ] {
        assert!(q.contains(&expected), "missing {} in {:?}", expected, q);
    }
    let all = edges(&engine, &["calls", "extends", "contains"]);
    assert!(has_edge(&all, "useIt", "Geometry.area", "calls"), "{:?}", all);
    assert!(has_edge(&all, "useIt", "Geometry.Inner.deep", "calls"), "{:?}", all);
    // Bare calls inside the namespace still resolve.
    assert!(has_edge(&all, "Geometry.area", "Geometry.helper", "calls"), "{:?}", all);
    assert!(has_edge(&all, "consume", "Geometry.area", "calls"), "{:?}", all);
    assert!(has_edge(&all, "consume", "Geometry.Inner.deep", "calls"), "{:?}", all);
    assert!(has_edge(&all, "Square", "Geometry.Shape", "extends"), "{:?}", all);
    assert!(has_edge(&all, "Geometry", "Geometry.area", "contains"), "{:?}", all);
}
