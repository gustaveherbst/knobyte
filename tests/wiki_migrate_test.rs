//! `knobyte wiki migrate` (Knobyte's own older formats, through audited operations) and the
//! setup wiki finalization built on it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use knobyte::graph::GraphEngine;
use knobyte::wiki::migrate::{migrate, plan_migration, MigrationOptions};
use knobyte::wiki::scope::WikiScope;
use serde_json::Value;
use tempfile::{tempdir, TempDir};

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    scaffold: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let scaffold = root.join(".knobyte");
        fs::create_dir_all(scaffold.join("context")).unwrap();
        Self {
            _dir: dir,
            root,
            scaffold,
        }
    }

    fn write(&self, rel: &str, text: &str) {
        write(&self.scaffold, rel, text);
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.scaffold.join(rel)).unwrap()
    }

    fn scope(&self) -> WikiScope {
        WikiScope::load(&self.scaffold)
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_knobyte"))
            .args(args)
            .current_dir(&self.root)
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }
}

fn write(base: &Path, rel: &str, text: &str) {
    let p = base.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

const SETUP_MD: &str = "# Populating the Knobyte Scaffold\n\nInfrastructure.\n";

fn legacy_scaffold(f: &Fixture) {
    f.write(
        "ROUTER.md",
        "---\nid: kb_router\ntitle: Router\ntype: guide\n---\n# Router\n",
    );
    f.write("SETUP.md", SETUP_MD);
    f.write(
        "context/legacy.md",
        "---\ntitle: Legacy\nstatus: accepted\ntype: document\n---\n# Legacy\n\nBody.\n\n<!-- kb:entity type=decision status=draft -->\n## Sub decision\n\nSub body.\n",
    );
    f.write("notes/plain.md", "# Plain\n\nNo frontmatter at all.\n");
    f.write(
        "decisions/d.md",
        "---\nid: kb_d\ntitle: D\ntype: decision\nstatus: superseded\ngrounds_to:\n  - node_id: function:src/a.rs:f\n    body_hash: abc123\n---\n# D\n\nWhy.\n\n<!-- grounds: function:src/a.rs:g -->\n",
    );
    f.write(
        "context/hashed.md",
        "---\nid: kb_hashed\ntitle: Hashed\ntype: architecture\ngrounds_to:\n  - function:0123456789abcdef0123456789abcdef\n---\n# Hashed\n",
    );
    f.write(
        "context/weird.md",
        "---\nid: kb_weird\ntitle: Weird\ntype: fact\nstatus: sideways\n---\n# Weird\n",
    );
    f.write("team/members/m.md", "# Member\n\nTeam-owned.\n");
}

#[test]
fn plan_inventories_classifies_and_is_deterministic() {
    let f = Fixture::new();
    legacy_scaffold(&f);
    let scope = f.scope();
    let opts = MigrationOptions {
        scope: &scope,
        graph_db: None,
    };
    let plan = plan_migration(&opts);
    assert!(!plan.blocked, "{:#?}", plan.diagnostics);
    let kinds = |id: &str| -> Vec<String> {
        plan.items
            .iter()
            .filter(|i| i.entity_id == id)
            .map(|i| i.kind.clone())
            .collect()
    };
    assert_eq!(
        kinds("kb_legacy"),
        vec!["implicit_id", "legacy_type", "legacy_status"]
    );
    assert_eq!(kinds("kb_plain"), vec!["implicit_id", "implicit_type"]);
    assert_eq!(kinds("kb_d"), vec!["legacy_status", "grounding_shape"]);
    // The marker declares its type inline; its id is derived from file and heading.
    assert_eq!(
        kinds("kb_legacy_sub_decision"),
        vec!["implicit_id", "legacy_status"]
    );
    assert!(kinds("kb_router").is_empty());
    let class = |file: &str| {
        plan.inventory
            .iter()
            .find(|i| i.file == file)
            .map(|i| i.class.clone())
            .unwrap()
    };
    assert_eq!(class("SETUP.md"), "infrastructure");
    assert_eq!(class("team/members/m.md"), "read_only");
    assert_eq!(class("ROUTER.md"), "current");
    assert_eq!(class("context/legacy.md"), "legacy");
    assert_eq!(class("context/hashed.md"), "abstained");
    assert!(plan
        .abstentions
        .iter()
        .any(|a| a.entity_id.as_deref() == Some("kb_hashed") && a.reason.contains("code graph")));
    // An unknown status is a parse error: the whole file is left for review.
    assert_eq!(class("context/weird.md"), "unparseable");
    assert!(plan
        .abstentions
        .iter()
        .any(|a| a.file == "context/weird.md" && a.reason.contains("parse errors")));
    // Deterministic opIds, one precondition per entity.
    assert!(plan
        .items
        .iter()
        .all(|i| i.op_id.starts_with("mig_") && i.op_id.len() == 44));
    let again = plan_migration(&opts);
    assert_eq!(
        plan.items.iter().map(|i| &i.op_id).collect::<Vec<_>>(),
        again.items.iter().map(|i| &i.op_id).collect::<Vec<_>>()
    );
    let first = &plan.operations[0];
    assert!(first["baseContentHash"].is_string());
    assert_eq!(first["actor"]["id"], "migration");
    // Planning writes nothing.
    assert!(f.read("notes/plain.md").starts_with("# Plain"));
    assert!(plan.changed_files.contains(&"notes/plain.md".to_string()));
}

#[test]
fn apply_rewrites_legacy_formats_through_audited_operations() {
    let f = Fixture::new();
    legacy_scaffold(&f);
    let scope = f.scope();
    let opts = MigrationOptions {
        scope: &scope,
        graph_db: None,
    };
    let result = migrate(&opts, false);
    assert!(
        result.applied,
        "{:#?}",
        result.report.as_ref().map(|r| &r.diagnostics)
    );

    let legacy = f.read("context/legacy.md");
    assert!(legacy.starts_with("---\nid: kb_legacy\n"), "{}", legacy);
    assert!(legacy.contains("type: guide"));
    assert!(legacy.contains("status: promoted"));
    assert!(legacy.contains("id: kb_legacy_sub_decision"), "{}", legacy);
    assert!(legacy.contains("status: in_flight"));
    assert!(legacy.contains("Body.\n") && legacy.contains("Sub body.\n"));

    let plain = f.read("notes/plain.md");
    assert!(plain.starts_with("---\nid: kb_plain\n"), "{}", plain);
    assert!(plain.contains("type: guide"));
    assert!(plain.ends_with("# Plain\n\nNo frontmatter at all.\n"));

    let d = f.read("decisions/d.md");
    assert!(d.contains("status: deprecated"));
    assert!(d.contains("- ref: function:src/a.rs:f"), "{}", d);
    assert!(d.contains("body_hash: abc123"));
    assert!(!d.contains("node_id:"));
    assert!(
        d.contains("<!-- kb-ground: function:src/a.rs:g -->"),
        "{}",
        d
    );

    // Untouched: infrastructure, team-owned, abstained, current.
    assert_eq!(f.read("SETUP.md"), SETUP_MD);
    assert_eq!(f.read("team/members/m.md"), "# Member\n\nTeam-owned.\n");
    assert!(f
        .read("context/hashed.md")
        .contains("function:0123456789abcdef0123456789abcdef"));
    assert!(f.read("context/weird.md").contains("status: sideways"));

    // Audit log: every migration operation recorded under the migration actor.
    let log = f.read("events/operations.jsonl");
    let entries: Vec<Value> = log
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let completes: Vec<&Value> = entries
        .iter()
        .filter(|e| e["phase"] == "complete")
        .collect();
    assert_eq!(completes.len(), result.plan.items.len());
    assert!(completes.iter().all(
        |e| e["opId"].as_str().unwrap().starts_with("mig_") && e["actor"]["id"] == "migration"
    ));

    // Idempotent: the migrated scaffold plans nothing; validate no longer reports legacy status.
    let after = plan_migration(&opts);
    assert!(after.items.is_empty(), "{:#?}", after.items);
    let report =
        knobyte::wiki::validate::validate_scaffold(&knobyte::wiki::validate::ValidateOptions {
            scope: &scope,
            project_root: &f.root,
            graph_db: None,
            index: None,
            limit: None,
        });
    assert!(!report
        .diagnostics
        .iter()
        .any(|d| d.code == "LEGACY_LIFECYCLE_STATE" && d.entity_id.as_deref() != Some("kb_weird")));
}

#[test]
fn legacy_edges_become_related_to_relations() {
    let f = Fixture::new();
    f.write(
        "ROUTER.md",
        "---\nid: kb_router\ntitle: Router\ntype: guide\nstatus: promoted\nedges:\n  - target: context/stack.md\n    condition: when picking libraries\n  - target: ../.knobyte/context/arch.md\n  - target: patterns/missing.md\n    condition: gone\n---\n# Router\n",
    );
    f.write("context/stack.md", "---\nid: kb_stack\ntitle: Stack\ntype: architecture\nstatus: promoted\n---\n# Stack\n");
    f.write(
        "context/arch.md",
        "---\nid: kb_arch\ntitle: Arch\ntype: architecture\nstatus: promoted\nedges:\n  - target: stack.md\n---\n# Arch\n",
    );
    let scope = f.scope();
    let opts = MigrationOptions { scope: &scope, graph_db: None };
    let plan = plan_migration(&opts);
    let kinds: Vec<(&str, &str)> = plan.items.iter().map(|i| (i.entity_id.as_str(), i.kind.as_str())).collect();
    assert_eq!(kinds, vec![("kb_router", "legacy_edges"), ("kb_arch", "legacy_edges")], "{:#?}", plan.items);
    assert!(!plan.blocked, "{:#?}", plan.diagnostics);
    let result = migrate(&opts, false);
    assert!(result.applied, "{:#?}", result.report.as_ref().map(|r| &r.diagnostics));

    let router = f.read("ROUTER.md");
    assert!(router.contains("target_id: kb_stack"), "{}", router);
    assert!(router.contains("note: when picking libraries"), "{}", router);
    assert!(router.contains("target_id: kb_arch"), "{}", router);
    // The unresolvable edge stays where drift's DEAD_EDGE check still sees it.
    assert!(router.contains("edges:") && router.contains("patterns/missing.md"), "{}", router);
    assert!(!router.contains("target: context/stack.md"), "{}", router);
    let arch = f.read("context/arch.md");
    assert!(arch.contains("type: related_to") && arch.contains("target_id: kb_stack"), "{}", arch);
    assert!(!arch.contains("edges:"), "{}", arch);

    // Nothing is left to migrate, and nothing is orphaned.
    assert!(plan_migration(&opts).is_current());
    let (_, out, _) = f.run(&["wiki", "validate"]);
    assert!(!out.contains("ORPHANED_ENTITY"), "{}", out);
}

#[test]
fn cli_migrate_dry_run_by_default_then_apply_with_envelope() {
    let f = Fixture::new();
    legacy_scaffold(&f);
    let (code, out, err) = f.run(&["wiki", "migrate", "--json"]);
    assert_eq!(code, 0, "{}{}", out, err);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["schemaVersion"], 1);
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["applied"], false);
    assert_eq!(v["data"]["dryRun"], true);
    assert!(v["data"]["plan"]["items"].as_array().unwrap().len() >= 8);
    assert!(
        v["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "MIGRATION_ABSTAINED" && d["file"] == "context/weird.md"),
        "{}",
        out
    );
    assert!(f.read("notes/plain.md").starts_with("# Plain"));

    // The index reports migration_required until the migration is applied.
    let (code, _, _) = f.run(&["wiki", "rebuild-index"]);
    assert_eq!(code, 0);
    let (_, out, _) = f.run(&["wiki", "index", "status", "--json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["data"]["state"], "migration_required", "{}", out);

    let (code, out, err) = f.run(&["wiki", "migrate", "--apply", "--json"]);
    assert_eq!(code, 0, "{}{}", out, err);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["data"]["applied"], true);
    assert!(f.read("notes/plain.md").starts_with("---\nid: kb_plain\n"));

    // Abstained legacy formats keep the state honest until they are resolved.
    let (_, out, _) = f.run(&["wiki", "index", "status", "--json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["data"]["state"], "migration_required", "{}", out);
    fs::remove_file(f.scaffold.join("context/hashed.md")).unwrap();
    fs::remove_file(f.scaffold.join("context/weird.md")).unwrap();
    let (_, out, _) = f.run(&["wiki", "index", "status", "--json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["data"]["state"], "stale", "{}", out);
    assert_eq!(f.run(&["wiki", "rebuild-index"]).0, 0);
    let (_, out, _) = f.run(&["wiki", "index", "status", "--json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["data"]["state"], "fresh", "{}", out);

    let (code, out, _) = f.run(&["wiki", "migrate", "--apply"]);
    assert_eq!(code, 0);
    assert!(out.contains("nothing to migrate"), "{}", out);
}

fn graph_fixture(root: &Path) {
    write(
        root,
        "src/tax.rs",
        "/// Sales tax in cents.\npub fn compute_sales_tax(amount_cents: i64) -> i64 {\n    amount_cents * 8 / 100\n}\n",
    );
    let mut engine = GraphEngine::open(&root.join(".knobyte/graph.db")).unwrap();
    engine.rebuild(root).unwrap();
}

#[test]
fn legacy_hashed_grounding_ids_become_readable_with_a_graph() {
    let f = Fixture::new();
    graph_fixture(&f.root);
    let conn = rusqlite::Connection::open(f.scaffold.join("graph.db")).unwrap();
    let hashed: String = conn
        .query_row(
            "SELECT id FROM nodes WHERE name = 'compute_sales_tax'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    f.write(
        "context/tax.md",
        &format!(
            "---\nid: kb_tax\ntitle: Tax\ntype: architecture\ngrounds_to:\n  - {}\n---\n# Tax\n",
            hashed
        ),
    );
    let scope = f.scope();
    let graph = f.scaffold.join("graph.db");
    let result = migrate(
        &MigrationOptions {
            scope: &scope,
            graph_db: Some(graph.as_path()),
        },
        false,
    );
    assert!(result.applied, "{:#?}", result.plan);
    let text = f.read("context/tax.md");
    assert!(
        text.contains("function:src/tax.rs:compute_sales_tax"),
        "{}",
        text
    );
    assert!(!text.contains(&hashed));
}

#[test]
fn setup_finalize_migrates_rebuilds_and_validates() {
    let f = Fixture::new();
    legacy_scaffold(&f);
    // Abstentions are warnings, not failures (the hashed id needs a graph); an unknown status
    // is a validation error, so drop that file to keep this scaffold otherwise valid.
    fs::remove_file(f.scaffold.join("context/weird.md")).unwrap();
    let config = knobyte::config::KnobyteConfig::new(f.root.clone(), f.scaffold.clone());
    let out = knobyte::wiki::finalize::finalize_wiki(&config);
    assert!(out.ready, "{}", out.failure_message());
    assert_eq!(out.stage, "complete");
    assert!(out.migrated);
    assert!(out.indexed_entities >= 6);
    assert!(
        out.warnings.iter().any(|w| w.contains("code graph")),
        "{:#?}",
        out.warnings
    );
    assert!(f.read("notes/plain.md").starts_with("---\nid: kb_plain\n"));
    // The abstained hashed id still needs a graph: the index says so.
    let status =
        knobyte::wiki::maintenance::inspect_index(&config.wiki_db_path(), &f.scope(), true);
    assert_eq!(
        status.state, "migration_required",
        "{:#?}",
        status.diagnostics
    );
    let status =
        knobyte::wiki::maintenance::inspect_index(&config.wiki_db_path(), &f.scope(), false);
    assert_eq!(status.state, "fresh", "{:#?}", status.diagnostics);
    let (captured, entities) = knobyte::setup::flow::finalize_setup(&config).unwrap();
    assert_eq!(captured, 0);
    assert_eq!(entities, out.indexed_entities);
}

#[test]
fn setup_finalize_fails_clearly_when_the_wiki_is_not_ready() {
    let f = Fixture::new();
    f.write(
        "context/a.md",
        "---\nid: kb_a\ntitle: A\ntype: architecture\ndepends_on: [kb_missing]\n---\n# A\n",
    );
    let config = knobyte::config::KnobyteConfig::new(f.root.clone(), f.scaffold.clone());
    let err = knobyte::setup::flow::finalize_setup(&config).unwrap_err();
    assert!(
        err.contains("Wiki finalization failed at validation"),
        "{}",
        err
    );
    assert!(err.contains("INVALID_RELATION_TARGET"), "{}", err);
}

#[test]
fn setup_finalize_fails_when_grounding_capture_skips_entries() {
    let f = Fixture::new();
    graph_fixture(&f.root);
    f.write(
        "context/a.md",
        "---\nid: kb_a\ntitle: A\ntype: architecture\ngrounds_to:\n  - function:src/tax.rs:compute_sales_tax\n  - function:src/gone.rs:vanished\n---\n# A\n",
    );
    let config = knobyte::config::KnobyteConfig::new(f.root.clone(), f.scaffold.clone());
    let out = knobyte::wiki::finalize::finalize_wiki(&config);
    assert!(!out.ready);
    assert_eq!(out.stage, "grounding", "{}", out.failure_message());
    assert_eq!(
        out.skipped_groundings.len(),
        1,
        "{:#?}",
        out.skipped_groundings
    );
    assert!(
        out.failure_message().contains("vanished"),
        "{}",
        out.failure_message()
    );
    assert!(out.baselines_captured >= 1);
}
