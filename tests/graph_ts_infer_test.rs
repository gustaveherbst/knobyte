//! Source-only TypeScript receiver-type inference (`ts-inference` edges), measured against the
//! type-checker mode on the `tests/fixtures/graph/ts_infer` project.
//!
//! The source-only assertions always run. The agreement test runs only when `node` and an
//! existing `typescript` package are found (`KNOBYTE_TEST_TYPESCRIPT`, a global install next to
//! Node, or Homebrew); otherwise it prints a skip message. Nothing is ever installed.

use knobyte::graph::GraphEngine;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let dst = to.join(e.file_name());
        let ft = e.file_type().unwrap();
        if ft.is_symlink() || [".git", ".knobyte", "node_modules"].iter().any(|n| e.file_name() == *n) {
            continue;
        }
        if ft.is_dir() {
            copy_dir(&e.path(), &dst);
        } else {
            fs::copy(e.path(), dst).unwrap();
        }
    }
}

fn fixture(root: &Path) {
    copy_dir(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/graph/ts_infer"), root);
}

/// One call edge: (caller, line, col, callee name) -> (target qualified name, kind, provenance).
type Site = (String, i64, i64, String);

fn call_edges(engine: &GraphEngine) -> HashMap<Site, Vec<(String, String, String)>> {
    let mut stmt = engine
        .connection()
        .prepare(
            "SELECT s.qualified_name, e.line, e.col, t.name, t.qualified_name, e.kind, COALESCE(e.provenance, '')
             FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target
             WHERE e.kind IN ('calls', 'instantiates', 'possible_call', 'calls_trait_method')",
        )
        .unwrap();
    let mut out: HashMap<Site, Vec<(String, String, String)>> = HashMap::new();
    for row in stmt
        .query_map([], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?),
                (r.get(4)?, r.get(5)?, r.get(6)?),
            ))
        })
        .unwrap()
    {
        let (site, edge) = row.unwrap();
        out.entry(site).or_default().push(edge);
    }
    out
}

fn inferred(all: &HashMap<Site, Vec<(String, String, String)>>, from: &str, line: i64, to: &str) -> bool {
    all.iter().any(|(site, edges)| {
        site.0 == from && site.1 == line && edges.iter().any(|e| e.0 == to && e.2 == "ts-inference")
    })
}

fn inferred_any(all: &HashMap<Site, Vec<(String, String, String)>>, from: &str, line: i64) -> Vec<String> {
    all.iter()
        .filter(|(site, _)| site.0 == from && site.1 == line)
        .flat_map(|(_, edges)| edges.iter().filter(|e| e.2 == "ts-inference").map(|e| e.0.clone()))
        .collect()
}

#[test]
fn source_only_inference_resolves_typed_receivers() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    engine.rebuild(root).unwrap();
    let all = call_edges(&engine);
    let rename = "UserService.rename";
    // `await this.repo.load(id)` -> `Promise<User>` unwrapped; `rename(): this` chains.
    assert!(inferred(&all, rename, 43, "UserRepo.load"), "{:#?}", all);
    assert!(inferred(&all, rename, 44, "User.rename") && inferred(&all, rename, 44, "User.describe"));
    // Optional chaining on `User | undefined`.
    assert!(inferred(&all, rename, 45, "User.describe"));
    // Constructor parameter property typed by an interface: the interface member.
    assert!(inferred(&all, rename, 46, "Notifier.notify"));
    // Constructor-assigned field from a barrel import; `self(): Clock` chained twice.
    assert!(inferred(&all, rename, 47, "Clock.now"));
    assert!(inferred(&all, rename, 48, "Clock.now"));
    // Inherited member through `extends` across files.
    assert!(inferred(&all, rename, 50, "BaseRepo.count"));
    // Return type of an imported function, then a three-link chain.
    assert!(inferred(&all, rename, 52, "UserRepo.owner") && inferred(&all, rename, 52, "User.describe"));
    // Imported module-level constant; static method then instance method.
    assert!(inferred(&all, rename, 53, "UserRepo.find"));
    assert!(inferred(&all, rename, 54, "UserRepo.create") && inferred(&all, rename, 54, "UserRepo.find"));
    // `new ns.User()` through a namespace import; a type alias annotation.
    assert!(inferred(&all, rename, 56, "User.describe"));
    assert!(inferred(&all, rename, 58, "User.rename"));
    // Destructuring `this`; arrow callback keeps `this`; `as` cast to an interface.
    assert!(inferred(&all, rename, 60, "BaseRepo.count"));
    assert!(inferred(&all, "<callback:[1].forEach[0]>", 61, "UserRepo.find"));
    assert!(inferred(&all, rename, 63, "Entity.describe"));
    assert!(inferred(&all, rename, 65, "Notifier.notify"));
    // Destructured typed parameter.
    assert!(inferred(&all, "UserService.helper", 70, "BaseRepo.count"));
    assert!(inferred(&all, "UserService.helper", 71, "Clock.now"));
    // `super.m()`, tsconfig `paths` import, a JavaScript constructor assignment.
    assert!(inferred(&all, "UserService.start", 40, "Service.start"));
    assert!(inferred(&all, "Service.start", 20, "Logger.info"));
    assert!(inferred(&all, "Service.start", 21, "Logger.info"));
    assert!(inferred(&all, "Legacy.tick", 8, "Clock.now"));
    // Generic parameter constrained by a class.
    assert!(inferred(&all, "Holder.use", 41, "A.run"));

    // Uncertain receivers stay unresolved by inference: unions, untyped values, loop variables.
    assert!(inferred_any(&all, "neg", 17).is_empty(), "{:?}", inferred_any(&all, "neg", 17));
    assert!(inferred_any(&all, "neg", 18).is_empty());
    assert!(inferred_any(&all, "neg", 19).is_empty());
    assert!(inferred_any(&all, "neg", 20).is_empty());
    // Lexical shadowing picks the innermost binding.
    assert_eq!(inferred_any(&all, "neg", 24), vec!["B.run"]);
    assert_eq!(inferred_any(&all, "inner", 27), vec!["B.run"]);
    assert_eq!(inferred_any(&all, "neg", 30), vec!["A.run"]);
    assert_eq!(inferred_any(&all, "neg", 32), vec!["A.run"]);
    assert_eq!(inferred_any(&all, "neg", 34), vec!["B.run"]);
    // A global type is not a same-named project class.
    assert!(inferred_any(&all, "external", 52).is_empty());
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

fn find_typescript(node: &Path) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("KNOBYTE_TEST_TYPESCRIPT").map(PathBuf::from) {
        if is_typescript_package(&p) {
            return Some(p);
        }
    }
    let node = node.canonicalize().unwrap_or_else(|_| node.to_path_buf());
    let mut cands = Vec::new();
    if let Some(prefix) = node.parent().and_then(|bin| bin.parent()) {
        cands.push(prefix.join("lib/node_modules/typescript"));
    }
    for prefix in ["/opt/homebrew", "/usr/local"] {
        cands.push(Path::new(prefix).join("opt/typescript/libexec/lib/node_modules/typescript"));
    }
    cands.into_iter().find(|c| is_typescript_package(c))
}

/// Builds `project` twice (source-only, then checker mode) in scratch copies and compares call
/// edges: (checker-resolved edges, of which source-only matched, inferred edges contradicting
/// the checker at the same call site).
fn agreement(project: &Path, ts: &Path) -> (usize, usize, Vec<String>) {
    let source_dir = tempdir().unwrap();
    copy_dir(project, source_dir.path());
    let mut engine = GraphEngine::open(&source_dir.path().join(".knobyte").join("graph.db")).unwrap();
    engine.rebuild(source_dir.path()).unwrap();
    let source = call_edges(&engine);

    let checked_dir = tempdir().unwrap();
    let root = checked_dir.path();
    copy_dir(project, root);
    fs::create_dir_all(root.join(".knobyte")).unwrap();
    fs::write(
        root.join(".knobyte/config.json"),
        format!(
            r#"{{ "graph": {{ "typescript": {{ "compiler": "tsc", "typescript_path": {:?} }} }} }}"#,
            ts.to_string_lossy()
        ),
    )
    .unwrap();
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    let summary = engine.rebuild(root).unwrap();
    assert!(summary.typescript_compiler.unwrap_or_default().starts_with("typescript "));
    let checked = call_edges(&engine);

    let compiler: HashMap<&Site, &String> = checked
        .iter()
        .flat_map(|(site, edges)| edges.iter().filter(|e| e.2 == "typescript-compiler").map(move |e| (site, &e.0)))
        .collect();
    let matched = compiler
        .iter()
        .filter(|(site, target)| {
            source
                .get(**site)
                .is_some_and(|edges| edges.iter().any(|e| &e.0 == **target && e.1 != "possible_call"))
        })
        .count();
    let by_inference = compiler
        .iter()
        .filter(|(site, target)| {
            source
                .get(**site)
                .is_some_and(|edges| edges.iter().any(|e| &e.0 == **target && e.2 == "ts-inference"))
        })
        .count();
    eprintln!("matched through ts-inference: {}", by_inference);
    // Checker targets per call position (chained calls share a position).
    let mut at_position: HashMap<(&str, i64, i64), HashSet<&String>> = HashMap::new();
    for (site, target) in &compiler {
        at_position.entry((site.0.as_str(), site.1, site.2)).or_default().insert(target);
    }
    let mut contradictions = Vec::new();
    for (site, edges) in &source {
        for e in edges.iter().filter(|e| e.2 == "ts-inference") {
            let same_site = compiler.get(site);
            let wrong = match same_site {
                Some(t) => *t != &e.0,
                // The checker answered this position with other declarations only.
                None => at_position
                    .get(&(site.0.as_str(), site.1, site.2))
                    .is_some_and(|ts| !ts.contains(&e.0) && ts.iter().all(|t| !t.ends_with(&format!(".{}", site.3)))),
            };
            if wrong {
                contradictions.push(format!("{}:{} {} -> {}", site.0, site.1, site.3, e.0));
            }
        }
    }
    (compiler.len(), matched, contradictions)
}

#[test]
fn source_only_inference_agrees_with_the_checker() {
    let Some(ts) = node_on_path().and_then(|n| find_typescript(&n)) else {
        eprintln!("skipping source_only_inference_agrees_with_the_checker: no node + typescript package");
        return;
    };
    let project = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/graph/ts_infer");
    let (total, matched, contradictions) = agreement(&project, &ts);
    let rate = matched as f64 / total as f64;
    eprintln!("ts-inference agreement: {}/{} ({:.1}%)", matched, total, rate * 100.0);
    assert!(contradictions.is_empty(), "{:#?}", contradictions);
    assert!(total >= 60, "{}", total);
    assert!(rate >= 0.9, "{}", rate);
}

/// Agreement on any project: `KNOBYTE_TS_INFER_PROJECT=<dir> cargo test --test graph_ts_infer_test
/// -- --ignored --nocapture` (the directory is copied; it is never modified).
#[test]
#[ignore]
fn measure_agreement_on_a_project() {
    let Some(project) = std::env::var_os("KNOBYTE_TS_INFER_PROJECT").map(PathBuf::from) else { return };
    let ts = node_on_path().and_then(|n| find_typescript(&n)).expect("node + typescript package");
    let (total, matched, contradictions) = agreement(&project, &ts);
    eprintln!(
        "checker-resolved call edges: {}; source-only matches: {} ({:.1}%); contradicting inferred edges: {}",
        total,
        matched,
        100.0 * matched as f64 / total.max(1) as f64,
        contradictions.len()
    );
    for c in &contradictions {
        eprintln!("  {}", c);
    }
}
