//! Variable/constant declarations, Rust module files, Express mounts.

use knobyte::graph::GraphEngine;
use std::collections::BTreeSet;
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

/// (kind, qualified_name) of every node in `file`.
fn nodes_in(engine: &GraphEngine, file: &str) -> BTreeSet<(String, String)> {
    let mut stmt = engine
        .connection()
        .prepare("SELECT kind, qualified_name FROM nodes WHERE file_path = ?1")
        .unwrap();
    stmt.query_map([file], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn has_node(set: &BTreeSet<(String, String)>, kind: &str, q: &str) -> bool {
    set.contains(&(kind.to_string(), q.to_string()))
}

/// (source qualified, source kind, target qualified, target kind, method) of edges of `kind`.
fn edges(engine: &GraphEngine, kind: &str) -> Vec<(String, String, String, String, String)> {
    let mut stmt = engine
        .connection()
        .prepare(
            "SELECT s.qualified_name, s.kind, t.qualified_name, t.kind, COALESCE(e.resolution_method, '') FROM edges e
             JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target WHERE e.kind = ?1",
        )
        .unwrap();
    stmt.query_map([kind], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

#[test]
fn python_module_and_class_variables_are_nodes() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "app/settings.py",
        r#"
MAX_RETRIES = 3
default_timeout: float = 2.5
_private_cache = {}
MAX_RETRIES = 4
a, b = 1, 2

class Config:
    name: str = "x"
    retries = MAX_RETRIES

    def method(self):
        local = 1
        return local

def helper():
    inner = 2
    return inner

if True:
    FLAG = make_flag()

def make_flag():
    return True
"#,
    );
    let engine = build(root);
    let n = nodes_in(&engine, "app/settings.py");
    assert!(has_node(&n, "constant", "MAX_RETRIES"), "{:?}", n);
    assert!(has_node(&n, "variable", "default_timeout"), "{:?}", n);
    assert!(has_node(&n, "variable", "_private_cache"), "{:?}", n);
    assert!(has_node(&n, "variable", "Config.name"), "{:?}", n);
    assert!(has_node(&n, "variable", "Config.retries"), "{:?}", n);
    assert!(has_node(&n, "constant", "FLAG"), "{:?}", n);
    // Function locals and tuple targets are not declarations.
    assert!(!n.iter().any(|(_, q)| q == "local" || q == "inner" || q == "a" || q == "b"), "{:?}", n);
    // The first binding is the declaration (no duplicate node).
    let rows: i64 = engine
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM nodes WHERE file_path = 'app/settings.py' AND qualified_name = 'MAX_RETRIES'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1);
    // Class attributes are contained by their class; calls in an initializer are made by it.
    let contains = edges(&engine, "contains");
    assert!(contains.iter().any(|e| e.0 == "Config" && e.2 == "Config.name"), "{:?}", contains);
    let calls = edges(&engine, "calls");
    assert!(calls.iter().any(|e| e.0 == "FLAG" && e.2 == "make_flag"), "{:?}", calls);
}

#[test]
fn rust_consts_statics_and_mod_declarations() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/lib.rs",
        r#"
pub mod util;
mod nested;
#[path = "other_name.rs"]
mod renamed;
mod inline {
    pub mod deep;
}

pub const LIMIT: usize = 10;
static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub struct Settings;
impl Settings {
    pub const DEFAULT: Settings = Settings;
}
"#,
    );
    write(root, "src/util.rs", "pub fn helper() {}\n");
    write(root, "src/nested/mod.rs", "pub fn n() {}\n");
    write(root, "src/other_name.rs", "pub fn o() {}\n");
    write(root, "src/inline/deep.rs", "pub fn d() {}\n");
    let engine = build(root);
    let n = nodes_in(&engine, "src/lib.rs");
    assert!(has_node(&n, "constant", "LIMIT"), "{:?}", n);
    assert!(has_node(&n, "variable", "COUNTER"), "{:?}", n);
    assert!(has_node(&n, "constant", "Settings::DEFAULT"), "{:?}", n);
    for ns in ["util", "nested", "renamed", "inline", "inline::deep"] {
        assert!(has_node(&n, "namespace", ns), "{} missing in {:?}", ns, n);
    }
    let links: BTreeSet<(String, String)> = edges(&engine, "contains")
        .into_iter()
        .filter(|e| e.4 == "rust-mod-decl")
        .map(|e| (e.0, e.2))
        .collect();
    let expect: BTreeSet<(String, String)> = [
        ("util", "src/util.rs"),
        ("nested", "src/nested/mod.rs"),
        ("renamed", "src/other_name.rs"),
        ("inline::deep", "src/inline/deep.rs"),
    ]
    .iter()
    .map(|(a, b)| (a.to_string(), b.to_string()))
    .collect();
    assert_eq!(links, expect);
    // The const's type is referenced.
    let type_of = edges(&engine, "type_of");
    assert!(type_of.iter().any(|e| e.0 == "Settings::DEFAULT" && e.2 == "Settings"), "{:?}", type_of);
}

fn route_names(engine: &GraphEngine) -> BTreeSet<String> {
    let mut stmt = engine
        .connection()
        .prepare("SELECT name FROM nodes WHERE kind = 'route'")
        .unwrap();
    stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
}

#[test]
fn express_router_mounts_compose_across_files() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/app.ts",
        r#"
import express from 'express';
import usersRouter from './routes/users';
import { adminRouter } from './routes/admin';
const app = express();
const health = express.Router();
health.get('/health', ping);
function ping(req, res) { res.send('ok'); }
app.use('/api', usersRouter);
app.use('/admin', auth, adminRouter);
app.use('/internal', health);
app.use('/legacy', require('./routes/legacy'));
app.get('/', ping);
function auth(req, res, next) { next(); }
"#,
    );
    write(
        root,
        "src/routes/users.ts",
        r#"
import { Router } from 'express';
import { v1 } from './v1';
const router = Router();
router.get('/users', list);
router.post('/users/:id', update);
router.use('/v1', v1);
function list(req, res) {}
function update(req, res) {}
export default router;
"#,
    );
    write(
        root,
        "src/routes/v1.ts",
        r#"
import { Router } from 'express';
export const v1 = Router();
v1.get('/status', status);
function status(req, res) {}
"#,
    );
    write(
        root,
        "src/routes/admin.ts",
        r#"
import express from 'express';
const adminRouter = express.Router();
adminRouter.delete('/cache', clear);
function clear(req, res) {}
export { adminRouter };
"#,
    );
    write(
        root,
        "src/routes/legacy.js",
        r#"
const express = require('express');
const legacy = express.Router();
legacy.get('/old', old);
function old(req, res) {}
module.exports = legacy;
"#,
    );
    let engine = build(root);
    let routes = route_names(&engine);
    for r in [
        "GET /api/users",
        "POST /api/users/:id",
        "GET /api/v1/status",
        "DELETE /admin/cache",
        "GET /internal/health",
        "GET /legacy/old",
        "GET /",
    ] {
        assert!(routes.contains(r), "{} missing in {:?}", r, routes);
    }
    assert!(!routes.contains("GET /users"), "{:?}", routes);
    // Handlers still bind in the router's file.
    let refs = edges(&engine, "references");
    assert!(
        refs.iter().any(|e| e.0 == "GET /api/users" && e.2 == "list" && e.4 == "express-route-handler"),
        "{:?}",
        refs
    );
    // No link facts leak into unresolved references.
    let leaked: i64 = engine
        .connection()
        .query_row("SELECT COUNT(*) FROM unresolved_refs WHERE reference_kind LIKE 'link:%'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(leaked, 0);
}

#[test]
fn router_mounted_twice_yields_both_paths_and_unmounted_router_keeps_its_own() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "app.js",
        r#"
const express = require('express');
const shared = require('./shared');
const app = express();
app.use('/a', shared);
app.use('/b', shared);
"#,
    );
    write(
        root,
        "shared.js",
        r#"
const express = require('express');
const r = express.Router();
r.get('/x', h);
function h() {}
module.exports = r;
"#,
    );
    write(
        root,
        "lonely.js",
        r#"
const express = require('express');
const lonely = express.Router();
lonely.get('/alone', h2);
function h2() {}
"#,
    );
    let engine = build(root);
    let routes = route_names(&engine);
    assert!(routes.contains("GET /a/x") && routes.contains("GET /b/x"), "{:?}", routes);
    assert!(routes.contains("GET /alone"), "{:?}", routes);
}
