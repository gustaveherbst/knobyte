//! Express routers nested in routers across files and levels, `router.route(path)` chains,
//! array mounts, mounts onto imported routers and cyclic mounts.

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

fn routes(engine: &GraphEngine) -> BTreeSet<String> {
    let mut stmt = engine.connection().prepare("SELECT name FROM nodes WHERE kind = 'route'").unwrap();
    stmt.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
}

/// (route, handler) of framework handler references.
fn handlers(engine: &GraphEngine) -> BTreeSet<(String, String)> {
    let mut stmt = engine
        .connection()
        .prepare(
            "SELECT s.name, t.name FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target
             WHERE e.kind = 'references' AND e.resolution_method = 'express-route-handler'",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|r| r.unwrap()).collect()
}

fn assert_routes(engine: &GraphEngine, expected: &[&str]) {
    let got = routes(engine);
    for r in expected {
        assert!(got.contains(*r), "{} missing in {:?}", r, got);
    }
}

#[test]
fn routers_nested_three_files_deep_compose_their_full_paths() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "src/app.ts",
        "import express from 'express';\nimport api from './api';\nconst app = express();\napp.use('/api', api);\nexport default app;\n",
    );
    write(
        root,
        "src/api.ts",
        "import { Router } from 'express';\nimport { v1Router } from './v1';\nconst api = Router();\napi.use('/v1', v1Router);\nexport default api;\n",
    );
    write(
        root,
        "src/v1.ts",
        "import express from 'express';\nexport const v1Router = express.Router();\nv1Router.use('/users', require('./users'));\nv1Router.get('/status', status);\nfunction status(req, res) {}\n",
    );
    write(
        root,
        "src/users.js",
        r#"const express = require('express');
const router = express.Router();
router
  .route('/:id')
  .get(show)
  .put(requireAuth, update)
  .delete(remove);
router.get('/', list);
function show(req, res) {}
function update(req, res) {}
function remove(req, res) {}
function list(req, res) {}
function requireAuth(req, res, next) { next(); }
module.exports = router;
"#,
    );
    let engine = build(root);
    assert_routes(
        &engine,
        &["GET /api/v1/status", "GET /api/v1/users", "GET /api/v1/users/:id", "PUT /api/v1/users/:id", "DELETE /api/v1/users/:id"],
    );
    let got = routes(&engine);
    assert!(!got.contains("GET /:id") && !got.contains("GET /status"), "{:?}", got);
    let h = handlers(&engine);
    assert!(h.contains(&("PUT /api/v1/users/:id".into(), "update".into())), "{:?}", h);
    assert!(h.contains(&("GET /api/v1/users/:id".into(), "show".into())), "{:?}", h);
}

#[test]
fn routers_nested_in_one_file_compose_through_every_level() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "server.js",
        r#"const express = require('express');
const app = express();
const api = express.Router();
const v1 = new express.Router();
const admin = express.Router({ mergeParams: true });
v1.get('/ping', ping);
admin.post('/flush', flush);
v1.use('/admin', admin);
api.use('/v1', v1);
app.use('/api', api);
function ping(req, res) {}
function flush(req, res) {}
"#,
    );
    let engine = build(root);
    assert_routes(&engine, &["GET /api/v1/ping", "POST /api/v1/admin/flush"]);
    assert!(!routes(&engine).contains("GET /ping"));
}

#[test]
fn array_mounts_middleware_lists_and_path_arrays_are_composed() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "app.js",
        r#"const express = require('express');
const { Router: makeRouter } = require('express');
const adminRouter = require('./admin');
const shared = makeRouter();
const app = express();
shared.get('/x', sharedHandler);
app.use('/admin', [auth, adminRouter]);
app.use(['/a', '/b'], shared);
app.use('/c', auth, logger, [shared]);
function auth(req, res, next) { next(); }
function logger(req, res, next) { next(); }
function sharedHandler(req, res) {}
"#,
    );
    write(
        root,
        "admin.js",
        "const express = require('express');\nconst r = express.Router();\nr.route('/cache').delete(clear);\nfunction clear(req, res) {}\nmodule.exports = r;\n",
    );
    let engine = build(root);
    assert_routes(&engine, &["DELETE /admin/cache", "GET /a/x", "GET /b/x", "GET /c/x"]);
    let got = routes(&engine);
    assert!(!got.contains("GET /x") && !got.contains("DELETE /cache"), "{:?}", got);
}

#[test]
fn mounts_onto_imported_routers_and_routes_on_imported_routers_compose() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/app.js", "const express = require('express');\nconst app = express();\nmodule.exports = app;\n");
    write(
        root,
        "src/server.js",
        "const express = require('express');\nconst app = require('./app');\nconst api = require('./api');\napp.use('/api', api);\n",
    );
    write(
        root,
        "src/api.js",
        "const express = require('express');\nconst api = express.Router();\nmodule.exports = api;\n",
    );
    // Routes declared on the imported `api` router from another module.
    write(
        root,
        "src/api-routes.js",
        "const express = require('express');\nconst router = require('./api');\nrouter.get('/orders', listOrders);\nfunction listOrders(req, res) {}\n",
    );
    let engine = build(root);
    assert_routes(&engine, &["GET /api/orders"]);
    assert!(!routes(&engine).contains("GET /orders"));
}

#[test]
fn object_exports_and_re_exported_routers_resolve() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "routes/users.js",
        "const express = require('express');\nconst usersRouter = express.Router();\nusersRouter.get('/me', me);\nfunction me(req, res) {}\nmodule.exports = { usersRouter };\n",
    );
    write(
        root,
        "routes/index.ts",
        "import express from 'express';\nimport { usersRouter } from './users';\nexport { usersRouter as users };\n",
    );
    write(
        root,
        "main.ts",
        "import express from 'express';\nimport { users } from './routes/index';\nconst app = express();\napp.use('/users', users);\n",
    );
    let engine = build(root);
    assert_routes(&engine, &["GET /users/me"]);
}

#[test]
fn cyclic_mounts_terminate_with_finite_paths() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "cycle.js",
        r#"const express = require('express');
const app = express();
const a = express.Router();
const b = express.Router();
a.use('/b', b);
b.use('/a', a);
a.get('/ping', ping);
app.use('/root', a);
function ping(req, res) {}
"#,
    );
    write(
        root,
        "x.js",
        "const express = require('express');\nconst y = require('./y');\nconst x = express.Router();\nx.use('/y', y);\nx.get('/hx', hx);\nfunction hx() {}\nmodule.exports = x;\n",
    );
    write(
        root,
        "y.js",
        "const express = require('express');\nconst x = require('./x');\nconst y = express.Router();\ny.use('/x', x);\ny.get('/hy', hy);\nfunction hy() {}\nmodule.exports = y;\n",
    );
    let engine = build(root);
    let got = routes(&engine);
    assert!(got.contains("GET /root/ping"), "{:?}", got);
    assert!(got.len() < 64, "{:?}", got);
    assert!(got.iter().all(|r| r.len() < 200), "{:?}", got);
}
