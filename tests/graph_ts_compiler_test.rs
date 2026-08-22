//! Optional TypeScript type-checker mode (`graph.typescript.compiler: "tsc"`).
//!
//! The fallback tests always run. The checker tests run only when `node` and an existing
//! `typescript` package are found locally (`KNOBYTE_TEST_TYPESCRIPT`, a global Node install, or
//! a package-manager cache); otherwise they print a skip message. Nothing is ever installed.
//!
//! Example (Homebrew TypeScript):
//! `KNOBYTE_TEST_TYPESCRIPT=/opt/homebrew/opt/typescript/libexec/lib/node_modules/typescript cargo test --test graph_ts_compiler_test`

use knobyte::graph::GraphEngine;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn node_on_path() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(if cfg!(windows) { "node.exe" } else { "node" }))
        .find(|c| c.is_file())
}

fn is_typescript_package(dir: &Path) -> bool {
    dir.join("lib").join("typescript.js").is_file()
        && fs::read_to_string(dir.join("package.json"))
            .is_ok_and(|s| s.contains("\"name\": \"typescript\"") || s.contains("\"name\":\"typescript\""))
}

/// An already-present `typescript` package directory, if any.
fn find_typescript(node: &Path) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("KNOBYTE_TEST_TYPESCRIPT").map(PathBuf::from) {
        if is_typescript_package(&p) {
            return Some(p);
        }
    }
    // Global installs next to this Node (`<prefix>/lib/node_modules/typescript`).
    let node = node.canonicalize().unwrap_or_else(|_| node.to_path_buf());
    if let Some(prefix) = node.parent().and_then(|bin| bin.parent()) {
        for c in [prefix.join("lib/node_modules/typescript"), prefix.join("node_modules/typescript")] {
            if is_typescript_package(&c) {
                return Some(c);
            }
        }
    }
    // Homebrew's `typescript` formula.
    for prefix in ["/opt/homebrew", "/usr/local"] {
        let c = Path::new(prefix).join("opt/typescript/libexec/lib/node_modules/typescript");
        if is_typescript_package(&c) {
            return Some(c);
        }
    }
    // Extracted package-manager caches (bun keeps unpacked packages).
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let mut cached: Vec<PathBuf> = fs::read_dir(home.join(".bun/install/cache"))
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("typescript@5") || n.starts_with("typescript@4"))
        })
        .filter(|p| is_typescript_package(p))
        .collect();
    cached.sort();
    cached.pop()
}

/// `node` plus a `typescript` package, or a skip message.
fn checker_toolchain(test: &str) -> Option<PathBuf> {
    let Some(node) = node_on_path() else {
        eprintln!("skipping {}: `node` not found on PATH", test);
        return None;
    };
    let Some(ts) = find_typescript(&node) else {
        eprintln!(
            "skipping {}: no local `typescript` package (set KNOBYTE_TEST_TYPESCRIPT to a typescript package directory)",
            test
        );
        return None;
    };
    Some(ts)
}

/// A fixture project with its own package.json and tsconfig, its `node_modules/typescript`
/// linked to an existing package (the project-first discovery path).
fn fixture(root: &Path, typescript: &Path) {
    write(root, "package.json", r#"{ "name": "fixture", "private": true, "type": "module", "devDependencies": { "typescript": "*" } }"#);
    write(
        root,
        "tsconfig.json",
        r#"{ "compilerOptions": { "strict": true, "target": "ES2022", "module": "ESNext", "moduleResolution": "Bundler", "noEmit": true }, "include": ["src"] }"#,
    );
    write(root, ".knobyte/config.json", r#"{ "graph": { "typescript": { "compiler": "tsc" } } }"#);
    let nm = root.join("node_modules");
    fs::create_dir_all(&nm).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(typescript, nm.join("typescript")).unwrap();
    #[cfg(not(unix))]
    write(
        root,
        ".knobyte/config.json",
        &format!(
            r#"{{ "graph": {{ "typescript": {{ "compiler": "tsc", "typescript_path": {:?} }} }} }}"#,
            typescript.to_string_lossy()
        ),
    );
    write(
        root,
        "src/lib.ts",
        "export class Base {\n  run(): number { return 1; }\n}\nexport class Worker extends Base {\n  run(): number { return super.run() + 1; }\n}\nexport class Idle {\n  run(): number { return 0; }\n}\nexport function make(): Worker { return new Worker(); }\nexport function pick(x: string): string;\nexport function pick(x: number): number;\nexport function pick(x: any): any { return x; }\nexport type Job = Worker;\n",
    );
    write(
        root,
        "src/main.ts",
        "import { make, pick, Worker as W } from \"./lib\";\nexport function main() {\n  const w = make();\n  w.run();\n  new W().run();\n  pick(1);\n}\n",
    );
    write(root, "src/other.ts", "export function other() { return 1; }\n");
}

/// (source qualified, target qualified, kind, provenance)
fn edges(engine: &GraphEngine) -> Vec<(String, String, String, String)> {
    let mut stmt = engine
        .connection()
        .prepare(
            "SELECT s.qualified_name, t.qualified_name, e.kind, COALESCE(e.provenance, '') FROM edges e
             JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn checked(all: &[(String, String, String, String)], from: &str, to: &str, kind: &str) -> bool {
    all.iter()
        .any(|e| e.0 == from && e.1 == to && e.2 == kind && e.3 == "typescript-compiler")
}

fn metadata(engine: &GraphEngine, key: &str) -> String {
    engine
        .connection()
        .query_row("SELECT value FROM project_metadata WHERE key = ?1", [key], |r| r.get(0))
        .unwrap_or_default()
}

fn signature(engine: &GraphEngine, qualified: &str) -> String {
    engine
        .connection()
        .query_row("SELECT signature FROM nodes WHERE qualified_name = ?1", [qualified], |r| r.get(0))
        .unwrap_or_default()
}

#[test]
fn checker_resolves_typed_receivers_overloads_and_aliases() {
    let Some(ts) = checker_toolchain("checker_resolves_typed_receivers_overloads_and_aliases") else {
        return;
    };
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root, &ts);
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    let summary = engine.rebuild(root).unwrap();
    let status = summary.typescript_compiler.clone().unwrap_or_default();
    assert!(status.starts_with("typescript "), "{}", status);
    assert!(metadata(&engine, "typescript_compiler").starts_with("tsc:"));

    let all = edges(&engine);
    // `w.run()` on a receiver typed by `make()`'s return type, through an aliased import.
    assert!(checked(&all, "main", "Worker.run", "calls"), "{:#?}", all);
    assert!(!all.iter().any(|e| e.0 == "main" && e.1 == "Idle.run"), "{:#?}", all);
    // `new W().run()`: the instantiation and the method on the constructed type.
    assert!(checked(&all, "main", "Worker", "instantiates"), "{:#?}", all);
    // `super.run()`.
    assert!(checked(&all, "Worker.run", "Base.run", "calls"), "{:#?}", all);
    // An overloaded call lands on the implementation; its signature lists the overloads.
    assert!(checked(&all, "main", "pick", "calls"), "{:#?}", all);
    let sig = signature(&engine, "pick");
    assert!(sig.contains("pick(x: string): string") && sig.contains("pick(x: number): number"), "{}", sig);
    assert_eq!(signature(&engine, "make"), "make(): Worker");
    // `type Job = Worker`.
    assert!(checked(&all, "Job", "Worker", "aliases"), "{:#?}", all);
    assert!(root.join(".knobyte/ts-compiler/facts-cache.json").is_file());
}

#[test]
fn checker_results_are_cached_by_reverse_import_closure() {
    let Some(ts) = checker_toolchain("checker_results_are_cached_by_reverse_import_closure") else {
        return;
    };
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root, &ts);
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    let first = engine.rebuild(root).unwrap();
    assert!(first.typescript_compiler.unwrap_or_default().contains("3 files checked"));

    // Nothing changed: a refresh is a no-op.
    let noop = engine.refresh(root, None).unwrap();
    assert_eq!(noop.mode, "noop");

    // lib.ts changed: lib.ts and its importer main.ts are re-checked, other.ts is reused.
    let lib = fs::read_to_string(root.join("src/lib.ts")).unwrap();
    write(root, "src/lib.ts", &format!("{}export const VERSION = 2;\n", lib));
    let refreshed = engine.refresh(root, None).unwrap();
    let status = refreshed.summary.typescript_compiler.unwrap_or_default();
    assert!(status.contains("2 files checked, 1 reused"), "{}", status);
    assert!(checked(&edges(&engine), "main", "Worker.run", "calls"));

    // A leaf change re-checks only that file.
    write(root, "src/other.ts", "export function other() { return 2; }\n");
    let refreshed = engine.refresh(root, None).unwrap();
    let status = refreshed.summary.typescript_compiler.unwrap_or_default();
    assert!(status.contains("1 files checked, 2 reused"), "{}", status);
}

#[test]
fn source_only_is_the_default() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/a.ts", "export class A { run() { return 1; } }\nexport class B { run() { return 2; } }\nexport function f(x: A) { x.run(); }\n");
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    let summary = engine.rebuild(root).unwrap();
    assert_eq!(summary.typescript_compiler.as_deref(), Some("source"));
    assert_eq!(metadata(&engine, "typescript_compiler"), "source");
    assert!(edges(&engine).iter().all(|e| e.3 != "typescript-compiler"));
    assert!(!root.join(".knobyte/ts-compiler").exists());
}

#[test]
fn missing_typescript_falls_back_to_source_only() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        ".knobyte/config.json",
        r#"{ "graph": { "typescript": { "compiler": "tsc", "typescript_path": "does/not/exist" } } }"#,
    );
    write(root, "src/a.ts", "export function a() { return b(); }\nexport function b() { return 1; }\n");
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    let summary = engine.rebuild(root).unwrap();
    let status = summary.typescript_compiler.unwrap_or_default();
    assert!(status.starts_with("source (fallback:"), "{}", status);
    assert_eq!(metadata(&engine, "typescript_compiler"), "tsc:unavailable");
    // The source-only graph is complete.
    assert!(edges(&engine).iter().any(|e| e.0 == "a" && e.1 == "b" && e.2 == "calls"));
    // Still unavailable and nothing changed: a refresh is a no-op.
    assert_eq!(engine.refresh(root, None).unwrap().mode, "noop");
}

#[test]
fn failing_checker_falls_back_to_source_only() {
    if node_on_path().is_none() {
        eprintln!("skipping failing_checker_falls_back_to_source_only: `node` not found on PATH");
        return;
    }
    let dir = tempdir().unwrap();
    let root = dir.path();
    // A `typescript` package whose compiler throws when loaded.
    write(root, "node_modules/typescript/package.json", r#"{ "name": "typescript", "version": "0.0.0-broken", "main": "lib/typescript.js" }"#);
    write(root, "node_modules/typescript/lib/typescript.js", "throw new Error('broken compiler');\n");
    write(root, ".knobyte/config.json", r#"{ "graph": { "typescript": { "compiler": "tsc" } } }"#);
    write(root, "src/a.ts", "export function a() { return b(); }\nexport function b() { return 1; }\n");
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    let summary = engine.rebuild(root).unwrap();
    let status = summary.typescript_compiler.unwrap_or_default();
    assert!(status.starts_with("source (fallback:") && status.contains("broken compiler"), "{}", status);
    assert!(edges(&engine).iter().any(|e| e.0 == "a" && e.1 == "b" && e.2 == "calls"));
}

const INTERFACE_FIXTURE: &str = "export interface Runner {\n  run(): number;\n  label: string;\n}\n\
export class Impl implements Runner {\n  label = 'impl';\n  run(): number { return 1; }\n}\n\
export class Other implements Runner {\n  label = 'other';\n  run(): number { return 2; }\n}\n\
export function go() {\n  const i: Runner = new Impl();\n  i.run();\n}\n";

/// (kind, qualified name) of the nodes of `file`.
fn kinds(engine: &GraphEngine, file: &str) -> Vec<(String, String)> {
    let mut stmt = engine
        .connection()
        .prepare("SELECT kind, qualified_name FROM nodes WHERE file_path = ?1")
        .unwrap();
    stmt.query_map([file], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect()
}

#[test]
fn interface_members_are_nodes_and_typed_calls_bind_to_them_source_only() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/run.ts", INTERFACE_FIXTURE);
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    engine.rebuild(root).unwrap();
    let ns = kinds(&engine, "src/run.ts");
    assert!(ns.contains(&("method".into(), "Runner.run".into())), "{:?}", ns);
    assert!(ns.contains(&("property".into(), "Runner.label".into())), "{:?}", ns);
    let all = edges(&engine);
    assert!(all.iter().any(|e| e.0 == "Runner" && e.1 == "Runner.run" && e.2 == "contains"), "{:#?}", all);
    // The explicit interface annotation binds `i.run()` to the interface member.
    assert!(all.iter().any(|e| e.0 == "go" && e.1 == "Runner.run" && e.2 == "calls"), "{:#?}", all);
    assert!(!all.iter().any(|e| e.0 == "go" && (e.1 == "Impl.run" || e.1 == "Other.run")), "{:#?}", all);
}

#[test]
fn checker_binds_interface_typed_calls_to_the_interface_member() {
    let Some(ts) = checker_toolchain("checker_binds_interface_typed_calls_to_the_interface_member") else {
        return;
    };
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root, &ts);
    write(root, "src/run.ts", INTERFACE_FIXTURE);
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    engine.rebuild(root).unwrap();
    let all = edges(&engine);
    assert!(checked(&all, "go", "Runner.run", "calls"), "{:#?}", all);
    let unresolved: i64 = engine
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM unresolved_refs WHERE file_path = 'src/run.ts' AND target_name = 'run'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    assert_eq!(unresolved, 0);
}

/// Without a project `node_modules/typescript` or `typescript_path`, a global install is
/// discovered read-only, and the build summary / `graph status` name the package used.
#[test]
fn global_typescript_is_discovered_and_reported() {
    if std::env::var_os("KNOBYTE_TEST_TYPESCRIPT").is_none() {
        eprintln!("skipping global_typescript_is_discovered_and_reported: KNOBYTE_TEST_TYPESCRIPT is not set");
        return;
    }
    if node_on_path().is_none() {
        eprintln!("skipping global_typescript_is_discovered_and_reported: `node` not found on PATH");
        return;
    }
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, ".knobyte/config.json", r#"{ "graph": { "typescript": { "compiler": "tsc" } } }"#);
    write(root, "src/a.ts", "export function a() { return b(); }\nexport function b() { return 1; }\n");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_knobyte"))
        .args(["graph", "rebuild"])
        .current_dir(root)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{}{}", text, String::from_utf8_lossy(&out.stderr));
    let line = text
        .lines()
        .find(|l| l.starts_with("TypeScript compiler: "))
        .unwrap_or_else(|| panic!("no compiler line: {}", text));
    assert!(line.contains("TypeScript compiler: typescript ") && line.contains(" from "), "{}", line);
    assert!(line.contains("typescript"), "{}", line);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_knobyte"))
        .args(["graph", "status"])
        .current_dir(root)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let status = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(status.contains(line), "{}", status);
}
