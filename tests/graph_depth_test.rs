//! Code-graph depth: C# extraction, framework routes and TypeScript type-aware approximation.

use knobyte::graph::GraphEngine;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn fixture(name: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/graph").join(name))
        .unwrap()
}

fn build(root: &Path) -> GraphEngine {
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    engine.rebuild(root).unwrap();
    engine
}

/// (source qualified, target qualified, edge kind, resolution method)
fn edges(engine: &GraphEngine, kind: &str) -> Vec<(String, String, String)> {
    let mut stmt = engine
        .connection()
        .prepare(
            "SELECT s.qualified_name, t.qualified_name, COALESCE(e.resolution_method, '') FROM edges e
             JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target
             WHERE e.kind = ?1 ORDER BY 1, 2",
        )
        .unwrap();
    stmt.query_map([kind], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn has_edge(engine: &GraphEngine, kind: &str, from: &str, to: &str) -> bool {
    edges(engine, kind).iter().any(|(s, t, _)| s == from && t == to)
}

fn nodes_of_kind(engine: &GraphEngine, kind: &str) -> Vec<(String, Option<String>)> {
    let mut stmt = engine
        .connection()
        .prepare("SELECT qualified_name, signature FROM nodes WHERE kind = ?1 ORDER BY qualified_name")
        .unwrap();
    stmt.query_map([kind], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

#[test]
fn csharp_declarations_members_and_relationships() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/UserService.cs", &fixture("csharp/UserService.cs"));
    let engine = build(root);

    let names = |k: &str| nodes_of_kind(&engine, k).into_iter().map(|(q, _)| q).collect::<Vec<_>>();
    assert_eq!(names("namespace"), vec!["Acme.Services"]);
    let classes = names("class");
    for c in ["Acme.Services.UserService", "Acme.Services.BaseService", "Acme.Services.User"] {
        assert!(classes.contains(&c.to_string()), "{} in {:?}", c, classes);
    }
    assert_eq!(names("interface"), vec!["Acme.Services.IUserService"]);
    assert_eq!(names("struct"), vec!["Acme.Services.Point"]);
    assert_eq!(names("enum"), vec!["Acme.Services.Role"]);
    let methods = names("method");
    // Overloads keep one node each, qualified by parameter types; the constructor is a method.
    assert!(methods.contains(&"Acme.Services.UserService.Find(int)".to_string()), "{:?}", methods);
    assert!(methods.contains(&"Acme.Services.UserService.Find(string, bool)".to_string()));
    assert!(methods.contains(&"Acme.Services.UserService.UserService".to_string()));
    assert!(names("property").contains(&"Acme.Services.UserService.Name".to_string()));
    assert!(names("property").contains(&"Acme.Services.User.Email".to_string()), "record positional property");
    assert!(names("constant").contains(&"Acme.Services.UserService.MaxUsers".to_string()));
    assert!(names("field").contains(&"Acme.Services.UserService._repo".to_string()));
    let params = names("parameter");
    assert!(params.contains(&"Acme.Services.UserService.Find(string, bool).email".to_string()), "{:?}", params);

    // Containment: class -> method, method -> parameter.
    assert!(has_edge(&engine, "contains", "Acme.Services.UserService", "Acme.Services.UserService.Find(int)"));
    assert!(has_edge(
        &engine,
        "contains",
        "Acme.Services.UserService.Find(int)",
        "Acme.Services.UserService.Find(int).id"
    ));
    // Inheritance: first base entry extends, the rest implements.
    assert!(has_edge(&engine, "extends", "Acme.Services.UserService", "Acme.Services.BaseService"));
    assert!(has_edge(&engine, "implements", "Acme.Services.UserService", "Acme.Services.IUserService"));
    // Calls: implicit-this call to an inherited method is not guessed, explicit this.M() is.
    assert!(has_edge(
        &engine,
        "calls",
        "Acme.Services.UserService.Find(string, bool)",
        "Acme.Services.UserService.Validate"
    ));
    assert!(has_edge(&engine, "instantiates", "Acme.Services.UserService.Find(string, bool)", "Acme.Services.User"));
    assert!(has_edge(&engine, "returns", "Acme.Services.UserService.Find(int)", "Acme.Services.User"));
    // `using` directives are imports of the file.
    let imports = engine.query_who_imports("System.Collections.Generic").unwrap();
    assert_eq!(imports.len(), 1);
    assert_eq!(imports[0].file_path, "src/UserService.cs");
}

#[test]
fn framework_routes_link_to_their_handlers() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "server/routes.js", &fixture("frameworks/express/routes.js"));
    write(root, "api/main.py", &fixture("frameworks/fastapi/main.py"));
    write(root, "web/app.py", &fixture("frameworks/flask/app.py"));
    write(root, "src/users.controller.ts", &fixture("frameworks/nestjs/users.controller.ts"));
    write(root, "pages/api/users/index.ts", &fixture("frameworks/nextjs/pages/api/users/index.ts"));
    write(
        root,
        "app/api/orders/route.ts",
        "export async function GET() { return new Response('[]'); }\nexport const POST = async (req: Request) => new Response('ok');\n",
    );
    let engine = build(root);

    let routes: Vec<String> = nodes_of_kind(&engine, "route").into_iter().map(|(q, _)| q).collect();
    for r in [
        "GET /users",
        "POST /users",
        "GET /health",
        "GET /items/{item_id}",
        "GET /items",
        "POST /items",
        "GET /",
        "GET /accounts/<int:account_id>",
        "DELETE /accounts/<int:account_id>",
        "POST /accounts/login",
        "GET /users/:id",
        "ALL /api/users",
        "GET /api/orders",
        "POST /api/orders",
    ] {
        assert!(routes.iter().any(|x| x == r), "missing route {} in {:?}", r, routes);
    }
    // Docstring examples and inline (anonymous) handlers are not routes.
    assert!(!routes.iter().any(|r| r.contains("not-a-route")));
    assert!(!routes.iter().any(|r| r == "DELETE /users/:id"));

    let refs = edges(&engine, "references");
    let linked = |route: &str, handler: &str, method: &str| {
        refs.iter().any(|(s, t, m)| s == route && t == handler && m == method)
    };
    assert!(linked("GET /health", "health", "fastapi-route-handler"), "{:?}", refs);
    assert!(linked("GET /items/{item_id}", "read_item", "fastapi-route-handler"));
    assert!(linked("POST /accounts/login", "login", "flask-route-handler"));
    assert!(linked("ALL /api/users", "usersHandler", "nextjs-route-handler"));
    assert!(linked("GET /api/orders", "GET", "nextjs-route-handler"));
    assert!(linked("POST /api/orders", "POST", "nextjs-route-handler"));
    // Express: the last argument is the handler (middleware before it is skipped).
    assert!(refs.iter().any(|(s, t, m)| s.starts_with("POST /users") && t == "createUser" && m == "express-route-handler"));
    assert!(refs.iter().any(|(s, t, m)| s.starts_with("GET /users") && t == "listUsers" && m == "express-route-handler"));
    // NestJS: controller prefix + method decorator, bound to the class method.
    assert!(refs.iter().any(|(s, t, m)| s == "GET /users/:id" && t == "UsersController.findOne" && m == "nestjs-route-handler"));
}

#[test]
fn typescript_paths_reexports_aliases_and_overloads() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "tsconfig.json",
        r#"{
  // JSONC: comments and trailing commas are accepted
  "compilerOptions": {
    "baseUrl": ".",
    "paths": { "@app/*": ["src/*"], "@models": ["src/models/index.ts"], },
  },
}"#,
    );
    write(root, "src/models/user.ts", "export class User { constructor(public id: string) {} }\n");
    write(root, "src/models/index.ts", "export { User } from './user';\nexport * from './account';\n");
    write(root, "src/models/account.ts", "export class Account {}\n");
    write(root, "src/util/format.ts", "export function fmt(x: string): string { return x; }\n");
    write(
        root,
        "src/service.ts",
        r#"import { User, Account } from '@models';
import { fmt } from '@app/util/format';
import { fmt as fmt2 } from 'src/util/format';

export type Person = User;
export type Owner = Person;

export function load(id: string): Owner;
export function load(id: number): Owner;
export function load(id: string | number): Owner {
  fmt(String(id));
  return new User(String(id));
}

export function open(): Account { return new Account(); }
"#,
    );
    let engine = build(root);

    let mut stmt = engine
        .connection()
        .prepare(
            "SELECT b.local_name, b.resolved_file_path, t.qualified_name, json_extract(b.metadata, '$.resolution')
             FROM import_bindings b LEFT JOIN nodes t ON t.id = b.target_id WHERE b.file_path = 'src/service.ts'",
        )
        .unwrap();
    type BindingRow = (String, Option<String>, Option<String>, Option<String>);
    let rows: Vec<BindingRow> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let find = |local: &str| rows.iter().find(|r| r.0 == local).unwrap_or_else(|| panic!("{} in {:?}", local, rows));
    // `paths` exact pattern + barrel re-export chain.
    assert_eq!(find("User").2.as_deref(), Some("User"), "{:?}", rows);
    assert_eq!(find("User").3.as_deref(), Some("reexport_chain"));
    // `export *` re-export.
    assert_eq!(find("Account").2.as_deref(), Some("Account"));
    // `paths` wildcard pattern.
    assert_eq!(find("fmt").1.as_deref(), Some("src/util/format.ts"));
    assert_eq!(find("fmt").3.as_deref(), Some("tsconfig_paths"));
    // `baseUrl` resolution.
    assert_eq!(find("fmt2").1.as_deref(), Some("src/util/format.ts"));

    assert!(has_edge(&engine, "calls", "load", "fmt"));
    assert!(has_edge(&engine, "instantiates", "load", "User"));
    // Type aliases: `aliases` edges and references reaching the aliased type through chains.
    assert!(has_edge(&engine, "aliases", "Person", "User"));
    assert!(has_edge(&engine, "aliases", "Owner", "Person"));
    assert!(has_edge(&engine, "returns", "load", "Owner"));
    let returns = edges(&engine, "returns");
    assert!(
        returns.iter().any(|(s, t, m)| s == "load" && t == "User" && m == "type-alias"),
        "{:?}",
        returns
    );
    // Overloads: one node, signature lists the overloads then the implementation.
    let load: Vec<_> = nodes_of_kind(&engine, "function").into_iter().filter(|(q, _)| q == "load").collect();
    assert_eq!(load.len(), 1);
    let sig = load[0].1.clone().unwrap();
    assert!(sig.starts_with("function load(id: string): Owner; function load(id: number): Owner;"), "{}", sig);
    assert!(sig.ends_with("function load(id: string | number): Owner"), "{}", sig);
}
