//! Swift code graph: declarations, extensions, overloads, module-scoped resolution and the
//! SwiftPM manifest in drift checks.

use knobyte::drift::checkers::{command, dependency, CheckContext};
use knobyte::drift::extract_claims_from_str;
use knobyte::graph::extractor::{extract_file, is_supported_path, language_for_path};
use knobyte::graph::GraphEngine;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn copy_fixture(rel: &str, to: &Path) {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/graph").join(rel);
    fn copy(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for e in fs::read_dir(from).unwrap() {
            let e = e.unwrap();
            if e.path().is_dir() {
                copy(&e.path(), &to.join(e.file_name()));
            } else {
                fs::copy(e.path(), to.join(e.file_name())).unwrap();
            }
        }
    }
    copy(&from, to);
}

fn build(root: &Path) -> GraphEngine {
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    engine.rebuild(root).unwrap();
    engine
}

fn has_edge(engine: &GraphEngine, kind: &str, from: &str, to: &str) -> bool {
    engine
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target
             WHERE e.kind = ?1 AND s.qualified_name = ?2 AND t.qualified_name = ?3",
            [kind, from, to],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
        > 0
}

/// (qualified name, visibility, is_exported, is_static, docstring) of the nodes of a kind.
type NodeRow = (String, Option<String>, bool, bool, Option<String>);

fn nodes(engine: &GraphEngine, kind: &str) -> Vec<NodeRow> {
    let mut stmt = engine
        .connection()
        .prepare(
            "SELECT qualified_name, visibility, is_exported, is_static, docstring FROM nodes
             WHERE kind = ?1 ORDER BY qualified_name",
        )
        .unwrap();
    stmt.query_map([kind], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get::<_, i64>(3)? != 0, r.get(4)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

fn names(engine: &GraphEngine, kind: &str) -> Vec<String> {
    nodes(engine, kind).into_iter().map(|n| n.0).collect()
}

fn node<'a>(rows: &'a [NodeRow], q: &str) -> &'a NodeRow {
    rows.iter().find(|n| n.0 == q).unwrap_or_else(|| panic!("{} in {:?}", q, rows))
}

#[test]
fn swift_grammar_loads_and_paths_are_indexed() {
    assert!(is_supported_path(Path::new("Sources/App/main.swift")));
    assert_eq!(language_for_path("Sources/App/main.swift"), "swift");
    let r = extract_file("a.swift", "struct A { func f() {} }\n").expect("swift grammar loads");
    assert_eq!(r.language, "swift");
    assert_eq!(r.parse_status, "ok");
    assert_eq!(r.symbols.iter().map(|s| s.qualified_name.as_str()).collect::<Vec<_>>(), vec!["A", "A.f"]);
    assert!(!knobyte::graph::corpus::OTHER_KNOWN_SOURCE_EXTENSIONS.contains(&".swift"));
}

#[test]
fn swift_package_declarations() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    copy_fixture("swift", root);
    let engine = build(root);

    assert_eq!(names(&engine, "interface"), vec!["Drawable", "Shape"]);
    // Actors are classes; nested types are qualified by their enclosing type.
    let classes = names(&engine, "class");
    for c in ["BaseShape", "Circle", "Counter", "Logger", "Marker", "CircleTests"] {
        assert!(classes.contains(&c.to_string()), "{} in {:?}", c, classes);
    }
    assert_eq!(names(&engine, "struct"), vec!["Box", "History", "Point"]);
    assert_eq!(names(&engine, "enum"), vec!["Circle.Style", "Level"]);
    assert_eq!(
        names(&engine, "enum_member"),
        vec!["Circle.Style.dashed", "Circle.Style.filled", "Circle.Style.outlined", "Level.error", "Level.info"]
    );
    assert_eq!(names(&engine, "type_alias"), vec!["Coordinates"]);
    // Generic parameters are not unresolved type references.
    let generic_refs: i64 = engine
        .connection()
        .query_row("SELECT COUNT(*) FROM unresolved_refs WHERE reference_name = 'Element'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(generic_refs, 0);
    assert!(has_edge(&engine, "aliases", "Coordinates", "Point"));

    let methods = names(&engine, "method");
    // Overloads are qualified by their argument labels; unique names stay plain.
    for m in [
        "Circle.move(to:)",
        "Circle.move(by:_:)",
        "Circle.init(name:)",
        "Circle.init(radius:)",
        "BaseShape.init",
        "BaseShape.deinit",
        "Circle.subscript",
        "Circle.square",
        "Shape.move",
        "Shape.init",
        "Counter.increment",
        "Circle.draw",
        "Circle.render",
    ] {
        assert!(methods.contains(&m.to_string()), "{} in {:?}", m, methods);
    }
    assert!(names(&engine, "parameter").contains(&"Circle.move(by:_:).dy".to_string()));

    // Properties: stored, computed, static; protocol requirements are abstract members.
    let props = nodes(&engine, "property");
    assert!(node(&props, "Circle.count").3, "static property");
    assert!(!node(&props, "Circle.area").3);
    assert_eq!(node(&props, "Circle.center").1.as_deref(), Some("private"));
    assert_eq!(node(&props, "Circle.radius").1.as_deref(), Some("internal"));
    assert!(node(&props, "Shape.area").2);
    assert!(names(&engine, "constant").contains(&"defaultRadius".to_string()));
    assert!(names(&engine, "variable").contains(&"shapeCount".to_string()));

    // Visibility and docstrings.
    let classes = nodes(&engine, "class");
    assert_eq!(node(&classes, "BaseShape").1.as_deref(), Some("open"));
    assert!(node(&classes, "BaseShape").2);
    assert_eq!(node(&classes, "Circle").4.as_deref(), Some("A circle."));
    assert_eq!(node(&nodes(&engine, "interface"), "Shape").4.as_deref(), Some("Anything with an area."));
    let methods = nodes(&engine, "method");
    assert_eq!(node(&methods, "Circle.record").1.as_deref(), Some("fileprivate"));
    assert!(node(&methods, "Circle.square").3, "static method");

    // Extension members belong to the extended type, declared in another file.
    assert!(has_edge(&engine, "contains", "Circle", "Circle.draw"));
    assert!(has_edge(&engine, "contains", "Circle", "Circle.Style"));
    assert!(has_edge(&engine, "contains", "Circle.Style", "Circle.Style.outlined"));

    // Imports are module nodes.
    let importers = engine.query_who_imports("Foundation").unwrap();
    assert_eq!(importers.len(), 1);
    assert_eq!(importers[0].file_path, "Sources/Geometry/Shapes.swift");
    assert_eq!(engine.query_who_imports("Geometry").unwrap().len(), 2);
}

#[test]
fn swift_inheritance_and_calls() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    copy_fixture("swift", root);
    let engine = build(root);

    // Superclass, protocol conformances (also from an extension in another file), overrides.
    assert!(has_edge(&engine, "extends", "Circle", "BaseShape"));
    assert!(has_edge(&engine, "implements", "Circle", "Shape"));
    assert!(has_edge(&engine, "implements", "Circle", "Drawable"));
    assert!(has_edge(&engine, "implements", "Marker", "Drawable"));
    assert!(!has_edge(&engine, "extends", "Marker", "Drawable"));
    assert!(has_edge(&engine, "overrides", "Circle.describe", "BaseShape.describe"));
    // A `required init` and a new initializer are not overrides by name.
    assert!(!has_edge(&engine, "overrides", "Circle.init(radius:)", "BaseShape.init"));

    // Implicit self, extension members, super, static, overload selection by labels.
    assert!(has_edge(&engine, "calls", "Circle.describe", "Circle.record"));
    assert!(has_edge(&engine, "calls", "Circle.describe", "BaseShape.describe"));
    assert!(has_edge(&engine, "calls", "Circle.draw", "Circle.describe"));
    assert!(has_edge(&engine, "calls", "Circle.draw", "Circle.render"));
    assert!(has_edge(&engine, "calls", "Circle.init(radius:)", "BaseShape.init"));
    assert!(has_edge(&engine, "calls", "Circle.area", "Circle.square"));
    assert!(has_edge(&engine, "calls", "Circle.move(by:_:)", "Circle.move(to:)"));
    assert!(!has_edge(&engine, "calls", "Circle.move(by:_:)", "Circle.move(by:_:)"));
    // Receiver types: property annotations, `self.prop`, locals from `T()` and `let x: T`.
    assert!(has_edge(&engine, "calls", "Circle.move(to:)", "History.push"));
    assert!(has_edge(&engine, "calls", "Circle.record", "History.push"));
    assert!(has_edge(&engine, "calls", "makeCircle", "Circle.move(to:)"));
    assert!(has_edge(&engine, "calls", "Marker.draw", "Logger.fail"));
    // Initializer calls instantiate; enum cases with payloads are referenced.
    assert!(has_edge(&engine, "instantiates", "makeCircle", "Circle"));
    assert!(has_edge(&engine, "instantiates", "Circle.move(by:_:)", "Point"));
    assert!(has_edge(&engine, "instantiates", "Logger.shared", "Logger"));
    assert!(has_edge(&engine, "references", "Logger.fail", "Level.error"));

    // Another target sees the module's public API through `import`; tests see internals
    // through `@testable import`.
    assert!(has_edge(&engine, "calls", "circle", "makeCircle"));
    assert!(has_edge(&engine, "calls", "Sources/App/main.swift", "Circle.draw"));
    assert!(has_edge(&engine, "calls", "Sources/App/main.swift", "Circle.move(by:_:)"));
    assert!(has_edge(&engine, "calls", "CircleTests.testMove", "makeCircle"));
    assert!(has_edge(&engine, "instantiates", "CircleTests.testMove", "History"));
    assert!(has_edge(&engine, "calls", "CircleTests.testMove", "History.push"));
    assert!(has_edge(&engine, "calls", "CircleTests.testMove", "Circle.move(to:)"));
}

#[test]
fn swift_modules_scope_resolution() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    // Xcode-style app targets: each top-level folder is a module.
    write(root, "MyApp/Model.swift", "struct User {\n    let name: String\n    func greet() -> String { name }\n}\n");
    write(
        root,
        "MyApp/ContentView.swift",
        "import SwiftUI\n\nstruct ContentView: View {\n    var body: some View {\n        Text(User(name: \"a\").greet())\n    }\n}\n",
    );
    write(root, "Other/Model.swift", "struct User {}\n");
    write(root, "Other/Use.swift", "func make() { _ = User() }\n");
    // Internal declarations of another SwiftPM target are not visible through a plain import.
    write(root, "Sources/Lib/Hidden.swift", "func hidden() {}\npublic func shown() {}\n");
    write(root, "Sources/Tool/main.swift", "import Lib\nhidden()\nshown()\n");
    let engine = build(root);

    assert!(has_edge(&engine, "instantiates", "ContentView.body", "User"));
    assert!(has_edge(&engine, "calls", "ContentView.body", "User.greet"));
    assert!(has_edge(&engine, "instantiates", "make", "User"));
    let user_targets: Vec<String> = engine
        .connection()
        .prepare(
            "SELECT t.file_path FROM edges e JOIN nodes s ON s.id = e.source JOIN nodes t ON t.id = e.target
             WHERE e.kind = 'instantiates' AND t.name = 'User' ORDER BY 1",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(user_targets, vec!["MyApp/Model.swift", "Other/Model.swift"]);
    assert!(has_edge(&engine, "calls", "Sources/Tool/main.swift", "shown"));
    assert!(!has_edge(&engine, "calls", "Sources/Tool/main.swift", "hidden"));
}

#[test]
fn swift_package_manifest_in_drift_checks() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    copy_fixture("swift", root);
    let ctx = CheckContext::new(root, &root.join(".knobyte"));
    let md = "# Key Libraries\n\n- **ArgumentParser** — cli\n- **swift-argument-parser 1.3** — cli\n- **Alamofire** — http\n\n# Commands\n\n```sh\nswift build\nswift test\nswift run shapes\nswift run -c release App\nswift run missing-tool\n```\n";
    let claims = extract_claims_from_str(md, "stack.md");
    let deps: Vec<String> = dependency::check_dependencies(&claims, &ctx)
        .into_iter()
        .map(|i| format!("{} {}", i.code, i.message))
        .collect();
    assert_eq!(deps, vec!["DEPENDENCY_MISSING Claimed dependency \"Alamofire\" not found in any manifest"]);
    let cmds: Vec<String> = command::check_commands(&claims, &ctx)
        .into_iter()
        .map(|i| format!("{} {}", i.code, i.message))
        .collect();
    assert_eq!(cmds, vec!["DEAD_COMMAND Executable \"missing-tool\" not found in Package.swift"]);
}

#[test]
fn swift6_syntax_parses_cleanly() {
    let src = r#"enum MyError: Error { case bad }
struct Widget { func make() -> Int { 1 } }

@Observable
final class Model {
    nonisolated(unsafe) static var shared = Model()

    func load() async throws(MyError) -> Widget {
        let w = Widget()
        _ = w.make()
        return w
    }

    func take(_ s: consuming String, _ b: borrowing String) -> String {
        let y = consume s
        return y
    }

    func fetch() async {
        guard let snap = try? await load() else { return }
        _ = snap.make()
    }
}

#Preview {
    let m = Model()
    Widget()
}
"#;
    let r = extract_file("Sources/App/Model.swift", src).unwrap();
    assert_eq!(r.parse_status, "ok");
    let mut qs: Vec<_> = r
        .symbols
        .iter()
        .filter(|s| s.kind != "parameter")
        .map(|s| s.qualified_name.as_str())
        .collect();
    qs.sort();
    // `#Preview`'s body is local code: `m` is not a top-level constant.
    assert_eq!(
        qs,
        vec!["Model", "Model.fetch", "Model.load", "Model.shared", "Model.take", "MyError", "MyError.bad", "Widget", "Widget.make"]
    );
    let load = r.symbols.iter().find(|s| s.qualified_name == "Model.load").unwrap();
    assert!(load.is_async);
    assert_eq!(load.return_type.as_deref(), Some("Widget"));
    let shared = r.symbols.iter().find(|s| s.qualified_name == "Model.shared").unwrap();
    assert!(shared.is_static);

    let dir = tempdir().unwrap();
    write(dir.path(), "Sources/App/Model.swift", src);
    let engine = build(dir.path());
    assert!(has_edge(&engine, "calls", "Model.load", "Widget.make"));
    assert!(has_edge(&engine, "calls", "Model.fetch", "Model.load"));
}
