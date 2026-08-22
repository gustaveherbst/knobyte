//! Regression tests for graph read gating, store safety and query semantics.

use knobyte::graph::{inspect_status, GraphEngine};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use tempfile::tempdir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn db(root: &Path) -> PathBuf {
    root.join(".knobyte").join("graph.db")
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_knobyte")
}

fn knobyte(root: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .current_dir(root)
        .env("NO_COLOR", "1")
        .output()
        .unwrap()
}

fn rebuild(root: &Path) {
    let out = knobyte(root, &["graph", "rebuild", "--json", "--root", root.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

fn jsonl(out: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("bad JSONL line {:?}: {}", l, e)))
        .collect()
}

fn of_type<'a>(records: &'a [Value], t: &str) -> Vec<&'a Value> {
    records.iter().filter(|r| r["type"] == t).collect()
}

fn auth_fixture(root: &Path) {
    write(root, ".git/HEAD", "ref: refs/heads/main\n");
    write(
        root,
        "src/auth.ts",
        "export function hashPassword(p: string): string {\n  return p + 'salt';\n}\n\nexport function login(p: string): string {\n  return hashPassword(p);\n}\n",
    );
    write(
        root,
        "src/users.ts",
        "import { hashPassword } from './auth';\n\nexport function register(p: string): string {\n  const h = hashPassword(p);\n  return h;\n}\n",
    );
}

// ---------------------------------------------------------------------------
// 1. Stale graph reads are gated
// ---------------------------------------------------------------------------

#[test]
fn edited_files_are_excluded_and_declared_never_returned_as_ok() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    auth_fixture(root);
    rebuild(root);

    // Fresh: who-calls returns both callers, no status record.
    let out = knobyte(root, &["graph", "query", "who-calls", "hashPassword", "--detail", "source"]);
    let recs = jsonl(&out);
    assert!(out.status.success(), "{:?}", recs);
    assert!(of_type(&recs, "status").is_empty());
    assert_eq!(of_type(&recs, "result").len(), 2, "{:?}", recs);

    // Edit users.ts: shift lines so the indexed coordinates are wrong.
    write(
        root,
        "src/users.ts",
        "import { hashPassword } from './auth';\n\n// a\n// b\n// c\nexport function register(p: string): string {\n  const h = hashPassword(p);\n  return h;\n}\n",
    );
    let out = knobyte(root, &["graph", "query", "who-calls", "hashPassword", "--detail", "source"]);
    let recs = jsonl(&out);
    let status = of_type(&recs, "status");
    assert_eq!(status.len(), 1, "{:?}", recs);
    assert_eq!(status[0]["graphStatus"], "stale");
    assert_eq!(status[0]["excludedFiles"][0], "src/users.ts");
    // No fact and no source from the edited file.
    for r in &recs {
        assert_ne!(r["filePath"], "src/users.ts", "stale record returned: {}", r);
    }
    let summary = recs.last().unwrap();
    assert_eq!(summary["type"], "summary");
    assert_ne!(summary["status"], "ok");

    // A target defined only in a drifted file is refused, not answered.
    write(
        root,
        "src/auth.ts",
        "\n\nexport function hashPassword(p: string): string {\n  return p + 'pepper';\n}\n",
    );
    let out = knobyte(root, &["graph", "query", "who-calls", "hashPassword", "--jsonl"]);
    let recs = jsonl(&out);
    assert!(!out.status.success());
    assert_eq!(recs.last().unwrap()["code"], "TARGET_SOURCE_DRIFTED", "{:?}", recs);
}

#[test]
fn impact_declares_a_new_caller_file_instead_of_missing_it_silently() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    auth_fixture(root);
    rebuild(root);
    write(
        root,
        "src/admin.ts",
        "import { hashPassword } from './auth';\nexport function reset(p: string) { return hashPassword(p); }\n",
    );
    let out = knobyte(root, &["impact", "hashPassword", "--jsonl"]);
    let recs = jsonl(&out);
    let status = of_type(&recs, "status");
    assert_eq!(status.len(), 1, "{:?}", recs);
    assert!(status[0]["excludedFiles"].as_array().unwrap().iter().any(|f| f == "src/admin.ts"));
    assert_eq!(recs.last().unwrap()["status"], "degraded");
}

#[test]
fn unusable_store_is_refused_with_graph_unavailable() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    auth_fixture(root);
    rebuild(root);
    // A changed corpus policy makes the staleness unbounded: refuse.
    write(root, ".knobyte/config.json", "{\"graph\": {\"ignore\": [\"vendor/**\"]}}\n");
    let out = knobyte(root, &["graph", "query", "where-defined", "login", "--jsonl"]);
    let recs = jsonl(&out);
    assert!(!out.status.success());
    assert_eq!(recs.len(), 1, "{:?}", recs);
    assert_eq!(recs[0]["code"], "GRAPH_UNAVAILABLE");
    assert_eq!(recs[0]["graphStatus"], "stale");
    assert_eq!(recs[0]["reasonCode"], "GRAPH_CORPUS_POLICY_CHANGED");
    assert!(recs[0]["recoveryCommand"].is_string());
}

// ---------------------------------------------------------------------------
// 2. Reads never write the store
// ---------------------------------------------------------------------------

#[test]
fn reads_without_a_graph_never_create_one() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    auth_fixture(root);
    fs::create_dir_all(root.join(".knobyte")).unwrap();
    for args in [
        vec!["graph", "get", "x"],
        vec!["graph", "get", "x", "--jsonl"],
        vec!["graph", "query", "where-defined", "login"],
        vec!["graph", "query", "who-calls", "login", "--jsonl"],
        vec!["impact", "login", "--json"],
        vec!["graph", "scope", "login", "--jsonl"],
        vec!["graph", "status", "--json"],
    ] {
        let out = knobyte(root, &args);
        assert!(!db(root).exists(), "{:?} created graph.db", args);
        if args[1] != "status" {
            assert!(!out.status.success(), "{:?} succeeded without a graph", args);
        }
    }
    let out = knobyte(root, &["graph", "query", "who-calls", "login", "--jsonl"]);
    let recs = jsonl(&out);
    assert_eq!(recs[0]["code"], "GRAPH_UNAVAILABLE");
    assert_eq!(recs[0]["graphStatus"], "missing");

    // The library engine does not create the file either; a build does.
    let mut engine = GraphEngine::open(&db(root)).unwrap();
    assert!(!db(root).exists());
    engine.rebuild(root).unwrap();
    assert!(db(root).exists());
}

// ---------------------------------------------------------------------------
// 3. Newer-schema databases
// ---------------------------------------------------------------------------

fn set_newer_schema(root: &Path) {
    let conn = rusqlite::Connection::open(db(root)).unwrap();
    conn.execute(
        "INSERT INTO schema_versions (version, applied_at, description) VALUES (99, 0, 'future')",
        [],
    )
    .unwrap();
    conn.execute_batch("CREATE TABLE source_chunks (x INTEGER); CREATE TABLE future_table (y INTEGER);")
        .unwrap();
}

fn tables(root: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(db(root)).unwrap();
    let mut stmt = conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").unwrap();
    stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
}

#[test]
fn newer_schema_is_never_mutated_except_by_rebuild() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    auth_fixture(root);
    rebuild(root);
    set_newer_schema(root);
    let before = tables(root);

    // Reads and refresh refuse without touching the file.
    let out = knobyte(root, &["graph", "query", "where-defined", "login", "--jsonl"]);
    assert_eq!(jsonl(&out)[0]["graphStatus"], "rebuild_required");
    let out = knobyte(root, &["graph", "refresh", "--json"]);
    assert!(!out.status.success());
    drop(GraphEngine::open(&db(root)).unwrap());
    assert_eq!(tables(root), before, "a newer-schema database was mutated");
    assert!(before.contains(&"source_chunks".to_string()));

    // An explicit rebuild resets the schema history completely.
    rebuild(root);
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "fresh", "{:?}", h.diagnostics);
    assert_eq!(h.schema_version, Some(knobyte::graph::schema::CURRENT_SCHEMA_VERSION));
    let conn = rusqlite::Connection::open(db(root)).unwrap();
    let rows: i64 = conn.query_row("SELECT COUNT(*) FROM schema_versions", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 1);
}

// ---------------------------------------------------------------------------
// 4. Corrupt database errors are structured
// ---------------------------------------------------------------------------

#[test]
fn corrupt_database_reports_graph_index_corrupt_everywhere() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    auth_fixture(root);
    fs::create_dir_all(root.join(".knobyte")).unwrap();
    fs::write(db(root), b"this is definitely not a sqlite database, just bytes. ".repeat(100)).unwrap();
    let root_s = root.to_str().unwrap();
    for args in [
        vec!["graph", "query", "where-defined", "login", "--json"],
        vec!["graph", "query", "who-calls", "login", "--jsonl"],
        vec!["graph", "get", "x", "--json"],
        vec!["graph", "get", "x", "--jsonl"],
        vec!["impact", "login", "--json"],
        vec!["impact", "login", "--jsonl"],
        vec!["graph", "scope", "login", "--jsonl"],
        vec!["graph", "refresh", "--json", "--root", root_s],
        vec!["graph", "repair", "--json", "--root", root_s],
        vec!["graph", "ground", "--json"],
    ] {
        let out = knobyte(root, &args);
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{:?} succeeded on a corrupt db", args);
        assert!(!stdout.contains("SqliteFailure") && !stderr.contains("SqliteFailure"), "{:?}: {}{}", args, stdout, stderr);
        let v: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|_| panic!("{:?} not JSON: {}", args, stdout));
        let text = v.to_string();
        assert!(
            text.contains("GRAPH_INDEX_CORRUPT") || text.contains("GRAPH_INDEX_NOT_REPAIRABLE"),
            "{:?}: {}",
            args,
            text
        );
    }
}

// ---------------------------------------------------------------------------
// 5. Build counts agree with status
// ---------------------------------------------------------------------------

#[test]
fn build_counts_match_status_and_noop_refresh_reports_totals() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    auth_fixture(root);
    write(root, "src/empty.ts", "// nothing here\n");
    let root_s = root.to_str().unwrap();
    let out = knobyte(root, &["graph", "--json", "--root", root_s]);
    assert!(out.status.success());
    let build: Value = serde_json::from_slice(&out.stdout).unwrap();
    let out = knobyte(root, &["graph", "status", "--json", "--root", root_s]);
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(build["files_indexed"], status["counts"]["files"]);
    assert_eq!(build["nodes_indexed"], status["counts"]["nodes"]);
    assert_eq!(build["edges_indexed"], status["counts"]["edges"]);
    assert_eq!(build["totals"], status["counts"]);
    assert!(build["symbols_indexed"].as_u64().unwrap() < build["nodes_indexed"].as_u64().unwrap());

    let out = knobyte(root, &["graph", "refresh", "--json", "--root", root_s]);
    let noop: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(noop["mode"], "noop");
    assert_eq!(noop["files_indexed"], status["counts"]["files"]);
    assert_eq!(noop["nodes_indexed"], status["counts"]["nodes"]);
    assert_eq!(noop["files_extracted"], 0);
}

// ---------------------------------------------------------------------------
// 6. Framework routes
// ---------------------------------------------------------------------------

/// (route name, handler) pairs extracted from one file.
fn routes(path: &str, content: &str) -> Vec<(String, String)> {
    let r = knobyte::graph::extractor::extract_file(path, content).unwrap();
    r.symbols
        .iter()
        .filter(|s| s.kind == "route")
        .map(|s| {
            assert!(!s.is_exported, "route {} is exported", s.name);
            let handler = s.signature.as_deref().unwrap().rsplit("-> ").next().unwrap().to_string();
            (s.name.clone(), handler)
        })
        .collect()
}

#[test]
fn nestjs_same_line_handlers_constants_and_comments() {
    let src = "import { Controller, Get, Post } from '@nestjs/common';
const CONST = 'x';
@Controller('users')
export class UsersController {
  @Get('one') one() { return 1; }
  @Get('two')
  two() { return 2; }
  @Post(CONST)
  create() { return 3; }
  // @Get('commented') ghost() {}
  /* @Post('blocked')
     blocked() {} */
  @Get()
  @UseGuards(AuthGuard)
  async root(): Promise<number> { return 0; }
}
";
    let r = routes("src/users.controller.ts", src);
    assert!(r.contains(&("GET /users/one".into(), "UsersController.one".into())), "{:?}", r);
    assert!(r.contains(&("GET /users/two".into(), "UsersController.two".into())), "{:?}", r);
    assert!(r.contains(&("GET /users".into(), "UsersController.root".into())), "{:?}", r);
    assert!(!r.iter().any(|(n, _)| n.starts_with("POST")), "non-literal path emitted: {:?}", r);
    assert!(!r.iter().any(|(n, _)| n.contains("commented") || n.contains("blocked")), "{:?}", r);
    assert_eq!(r.len(), 3, "{:?}", r);

    // A controller whose prefix is a constant is skipped entirely.
    let src = "import { Controller, Get } from '@nestjs/common';\n@Controller(BASE)\nexport class A {\n  @Get('x')\n  x() {}\n}\n";
    assert!(routes("a.ts", src).is_empty());
}

#[test]
fn python_unreadable_methods_emit_no_route() {
    let fastapi = "from fastapi import FastAPI
app = FastAPI()

@app.api_route('/a', methods=METHODS)
def a():
    pass

@app.api_route('/b')
def b():
    pass

@app.api_route('/c', methods=['GET', 'POST'])
def c():
    pass
";
    let r = routes("main.py", fastapi);
    assert_eq!(
        r,
        vec![("GET /c".to_string(), "c".to_string()), ("POST /c".to_string(), "c".to_string())],
    );

    let flask = "from flask import Flask
app = Flask(__name__)

@app.route('/m', methods=METHODS)
def m():
    pass

@app.route('/d')
def d():
    pass

@app.options('/o')
def o():
    pass

@app.head('/h')
def h():
    pass
";
    let r = routes("app.py", flask);
    assert_eq!(
        r,
        vec![
            ("GET /d".to_string(), "d".to_string()),
            ("OPTIONS /o".to_string(), "o".to_string()),
            ("HEAD /h".to_string(), "h".to_string()),
        ],
    );
}

#[test]
fn route_nodes_are_file_level_and_not_exported() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "app/api/orders/route.ts",
        "export async function GET() { return 1; }\nexport async function POST() { return 2; }\n",
    );
    write(
        root,
        "src/users.controller.ts",
        "import { Controller, Get } from '@nestjs/common';\n@Controller('users')\nexport class UsersController {\n  @Get(':id') findOne() { return 1; }\n}\n",
    );
    rebuild(root);
    let conn = rusqlite::Connection::open(db(root)).unwrap();
    let parents: Vec<(String, String)> = conn
        .prepare(
            "SELECT r.name, p.kind FROM edges e JOIN nodes r ON r.id = e.target JOIN nodes p ON p.id = e.source \
             WHERE e.kind = 'contains' AND r.kind = 'route' ORDER BY r.name",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(parents.len(), 3, "{:?}", parents);
    assert!(parents.iter().all(|(_, k)| k == "file"), "{:?}", parents);
    let exported: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM edges e JOIN nodes r ON r.id = e.target WHERE e.kind = 'exports' AND r.kind = 'route'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exported, 0);
}

// ---------------------------------------------------------------------------
// 7. C# indexers and field initializers
// ---------------------------------------------------------------------------

const GRID_CS: &str = "namespace Shop
{
    public class Helper
    {
        public static Helper Create() { return new Helper(); }
        public int Size() { return 1; }
    }

    public class Grid
    {
        private readonly Helper helper = Helper.Create();
        public int this[int i] { get { return i; } }
        public string this[string key] { get { return key; } }
    }
}
";

#[test]
fn csharp_indexers_keep_their_own_nodes() {
    let r = knobyte::graph::extractor::extract_file("src/Grid.cs", GRID_CS).unwrap();
    let indexers: Vec<&str> = r
        .symbols
        .iter()
        .filter(|s| s.kind == "property" && s.name == "this")
        .map(|s| s.qualified_name.as_str())
        .collect();
    assert_eq!(indexers.len(), 2, "{:?}", indexers);
    assert_ne!(indexers[0], indexers[1], "indexers collapsed: {:?}", indexers);

    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/Grid.cs", GRID_CS);
    rebuild(root);
    let conn = rusqlite::Connection::open(db(root)).unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM nodes WHERE kind = 'property' AND name = 'this'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
}

#[test]
fn csharp_field_initializer_calls_belong_to_the_field() {
    let r = knobyte::graph::extractor::extract_file("src/Grid.cs", GRID_CS).unwrap();
    let call = r.calls.iter().find(|c| c.target_name == "Create").unwrap();
    assert!(call.caller_name.ends_with("helper"), "caller: {:?}", call.caller_name);

    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/Grid.cs", GRID_CS);
    rebuild(root);
    let conn = rusqlite::Connection::open(db(root)).unwrap();
    let callers: Vec<(String, String)> = conn
        .prepare(
            "SELECT s.kind, s.name FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target \
             WHERE t.name = 'Create' AND e.kind IN ('calls', 'possible_call', 'calls_trait_method')",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(callers, vec![("field".to_string(), "helper".to_string())]);
}

// ---------------------------------------------------------------------------
// 8. Query semantics
// ---------------------------------------------------------------------------

fn semantics_fixture(root: &Path) {
    write(root, "src/a.ts", "export function format(x: string) { return x; }\n");
    write(root, "src/b.ts", "export function format(n: number) { return String(n); }\n");
    write(root, "src/hit.ts", "export function hit(v: any) { return v; }\n");
    write(
        root,
        "src/late.ts",
        "import { hit } from './hit';\nexport function late() {\n  const a = 1;\n  const b = 2;\n  return hit(a + b);\n}\n",
    );
    write(root, "src/early.ts", "import { hit } from './hit';\nexport function early() { return hit(1); }\n");
    write(root, "tests/hit.test.ts", "import { hit } from '../src/hit';\nexport function checkHit() { return hit(0); }\n");
    write(
        root,
        "src/twice.ts",
        "import { hit } from './hit';\nexport function twice() {\n  hit('x');\n  hit(2);\n}\n",
    );
    write(root, "src/dyn.ts", "export function caller() { return (globalThis as any).x || dynamicThing(); }\n");
    write(root, "src/lib.rs", "pub mod outer {\n    pub mod inner {\n        pub fn deep() {}\n    }\n}\n");
    write(root, "src/legacy.kt", "fun main() {}\n");
}

#[test]
fn impact_refuses_duplicate_names_instead_of_merging() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    semantics_fixture(root);
    rebuild(root);
    let out = knobyte(root, &["impact", "format", "--jsonl"]);
    let recs = jsonl(&out);
    assert!(!out.status.success());
    let err = recs.last().unwrap();
    assert_eq!(err["code"], "TARGET_AMBIGUOUS", "{:?}", recs);
    assert_eq!(err["candidates"].as_array().unwrap().len(), 2);
    let out = knobyte(root, &["impact", "format", "--json"]);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["code"], "TARGET_AMBIGUOUS");
    // A file target is not ambiguous.
    let out = knobyte(root, &["impact", "src/a.ts", "--jsonl"]);
    assert!(out.status.success(), "{:?}", jsonl(&out));
}

#[test]
fn who_calls_falls_back_to_unresolved_references_and_not_found_has_coverage() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    semantics_fixture(root);
    rebuild(root);
    let out = knobyte(root, &["graph", "query", "who-calls", "dynamicThing", "--jsonl"]);
    let recs = jsonl(&out);
    assert!(out.status.success(), "{:?}", recs);
    let unresolved = of_type(&recs, "unresolved-reference");
    assert_eq!(unresolved.len(), 1, "{:?}", recs);
    assert_eq!(unresolved[0]["file"], "src/dyn.ts");
    assert!(of_type(&recs, "result").is_empty());
    assert_eq!(recs.last().unwrap()["status"], "partial");
    assert_eq!(recs.last().unwrap()["returnedNodes"], 0);

    let out = knobyte(root, &["graph", "query", "who-calls", "neverDefinedAnywhere", "--jsonl"]);
    let recs = jsonl(&out);
    assert!(!out.status.success());
    let err = recs.last().unwrap();
    assert_eq!(err["code"], "TARGET_NOT_FOUND");
    assert_eq!(err["unindexedSources"]["byExtension"][".kt"], 1, "{}", err);
    assert!(err["filesIndexed"].as_u64().unwrap() > 0);
}

#[test]
fn where_defined_is_exact_and_callers_are_ordered_by_call_site() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    semantics_fixture(root);
    rebuild(root);
    let engine = GraphEngine::open(&db(root)).unwrap();
    // No suffix matching: `inner::deep` is not `outer::inner::deep`.
    assert!(engine.query_where_defined("inner::deep").unwrap().is_empty());
    assert_eq!(engine.query_where_defined("outer::inner::deep").unwrap().len(), 1);
    assert_eq!(engine.query_where_defined("deep").unwrap().len(), 1);

    let callers: Vec<String> = engine.query_who_calls("hit").unwrap().into_iter().map(|n| n.name).collect();
    assert_eq!(callers, vec!["early", "twice", "late", "checkHit"], "tests last, then call-site line");

    let out = knobyte(root, &["graph", "query", "who-calls", "hit", "--jsonl"]);
    let names: Vec<String> = of_type(&jsonl(&out), "result")
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["early", "twice", "late", "checkHit"]);
}

#[test]
fn call_edges_are_per_call_site() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    semantics_fixture(root);
    rebuild(root);
    let conn = rusqlite::Connection::open(db(root)).unwrap();
    let lines: Vec<i64> = conn
        .prepare(
            "SELECT e.line FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target \
             WHERE s.name = 'twice' AND t.name = 'hit' AND e.kind = 'calls' ORDER BY e.line",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(lines, vec![3, 4]);
    let out = knobyte(root, &["graph", "query", "where-defined", "hit", "--jsonl"]);
    let recs = jsonl(&out);
    // Five call sites from four distinct callers.
    assert_eq!(of_type(&recs, "result")[0]["callerCount"], 4);
}

#[test]
fn human_get_resolves_readable_refs_and_reports_missing_ids() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    semantics_fixture(root);
    rebuild(root);
    let out = knobyte(root, &["graph", "get", "function:src/late.ts:late"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("late"));

    let out = knobyte(root, &["graph", "get", "function:src/late.ts:late", "--source"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("return hit(a + b);"));

    let out = knobyte(root, &["graph", "get", "no-such-node"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("NODE_NOT_FOUND"));
    let out = knobyte(root, &["graph", "get", "no-such-node", "--json"]);
    assert!(!out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["errors"][0]["code"], "NODE_NOT_FOUND");
}

// ---------------------------------------------------------------------------
// 9. Corpus policy defaults
// ---------------------------------------------------------------------------

fn corpus(root: &Path, policy: &knobyte::graph::CorpusPolicy) -> Vec<String> {
    knobyte::graph::scan_corpus(root, policy).unwrap().files.into_iter().map(|f| f.rel_path).collect()
}

#[test]
fn default_ignores_follow_build_outputs_not_arbitrary_names() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/main.ts", "export const a = 1;\n");
    for d in ["build", "coverage", ".next", "out", "dist", "pkg/build"] {
        write(root, &format!("{}/gen.js", d), "export const g = 1;\n");
    }
    // `reference/` and a `target/` that is not a Cargo build directory are ordinary source.
    write(root, "reference/impl.ts", "export const r = 1;\n");
    write(root, "src/target/aim.ts", "export const t = 1;\n");
    // A crate's `target/` is build output.
    write(root, "crate/Cargo.toml", "[package]\nname = \"c\"\n");
    write(root, "crate/src/lib.rs", "pub fn f() {}\n");
    write(root, "crate/target/debug/build.rs", "fn main() {}\n");
    let files = corpus(root, &Default::default());
    assert_eq!(
        files,
        vec!["crate/src/lib.rs", "reference/impl.ts", "src/main.ts", "src/target/aim.ts"],
    );
}

#[test]
fn total_bytes_limit_is_enforced() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "a.ts", &"export const a = 1;\n".repeat(20));
    write(root, "b.ts", &"export const b = 1;\n".repeat(20));
    let policy = knobyte::graph::CorpusPolicy { max_total_bytes: 500, ..Default::default() };
    let err = knobyte::graph::scan_corpus(root, &policy).unwrap_err();
    assert_eq!(err.limit, "max_total_bytes");
    assert_eq!(knobyte::graph::CorpusPolicy::default().max_total_bytes, 512 * 1024 * 1024);
    assert!(knobyte::graph::scan_corpus(root, &Default::default()).is_ok());
}

#[test]
fn binary_source_file_is_skipped_not_a_degraded_graph() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    auth_fixture(root);
    let p = root.join("src/blob.ts");
    fs::write(&p, b"export const x = 1;\0\0\x01\x02binary").unwrap();
    rebuild(root);
    let h = inspect_status(&db(root), root);
    assert_eq!(h.status, "fresh", "{:?}", h.diagnostics);
    assert_eq!(h.parse_health.failed, 0);
    let skipped = &h.coverage.as_ref().unwrap().skipped;
    assert!(skipped.iter().any(|s| s.path == "src/blob.ts" && s.code == "GRAPH_SOURCE_BINARY"), "{:?}", skipped);
}

#[test]
fn corrupt_copies_are_bounded_and_local_files_are_ignored_by_git() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    auth_fixture(root);
    fs::create_dir_all(root.join(".knobyte")).unwrap();
    for i in 0..5 {
        fs::write(db(root), format!("not a database {}", i).repeat(50)).unwrap();
        rebuild(root);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let copies: Vec<String> = fs::read_dir(root.join(".knobyte"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("graph.db.corrupt-") && !n.ends_with("-wal") && !n.ends_with("-shm"))
        .collect();
    assert_eq!(copies.len(), knobyte::graph::maintenance::CORRUPT_COPIES_RETAINED, "{:?}", copies);

    let ignore = fs::read_to_string(root.join(".knobyte/.gitignore")).unwrap();
    assert!(ignore.lines().any(|l| l.trim() == "graph.db*"), "{}", ignore);
    // With git available, nothing under .knobyte/ but the .gitignore itself is untracked.
    let git = Command::new("git").arg("init").arg("-q").current_dir(root).status();
    if git.map(|s| s.success()).unwrap_or(false) {
        let out = Command::new("git")
            .args(["status", "--porcelain", "--untracked-files=all", ".knobyte"])
            .current_dir(root)
            .output()
            .unwrap();
        let listed: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(|l| l[3..].to_string()).collect();
        assert_eq!(listed, vec![".knobyte/.gitignore".to_string()], "untracked graph files: {:?}", listed);
    }
}
