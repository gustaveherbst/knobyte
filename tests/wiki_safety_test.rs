//! Regression tests: write containment through symlinks, bounded relation reads, refresh
//! failures in envelopes, one index-state vocabulary across adapters, symlink discovery
//! diagnostics, and smaller CLI/migration/validation fixes.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use knobyte::config::KnobyteConfig;
use knobyte::mcp::tools::execute_tool_with_config;
use knobyte::wiki::index::{coded_error, WikiIndex};
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    scaffold: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap();
        let scaffold = root.join(".knobyte");
        fs::create_dir_all(scaffold.join("context")).unwrap();
        fs::write(scaffold.join("ROUTER.md"), "# Router\n").unwrap();
        Self {
            _dir: dir,
            root,
            scaffold,
        }
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.scaffold.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.scaffold.join(rel)).unwrap()
    }

    fn rebuild(&self) {
        let mut idx = WikiIndex::open_for_rebuild(&self.scaffold.join("wiki.db")).unwrap();
        idx.rebuild(&self.scaffold).unwrap();
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

    fn json(&self, args: &[&str]) -> (i32, Value) {
        let (code, out, err) = self.run(args);
        let v: Value = serde_json::from_str(&out)
            .unwrap_or_else(|e| panic!("not JSON ({}): {}\nstderr: {}", e, out, err));
        (code, v)
    }

    fn config(&self) -> KnobyteConfig {
        KnobyteConfig::new(self.root.clone(), self.scaffold.clone())
    }

    fn mcp(&self, name: &str, args: Value) -> Value {
        let r = execute_tool_with_config(name, &args, &self.config());
        serde_json::from_str(&r.content[0].text).unwrap()
    }
}

fn codes(v: &Value) -> Vec<String> {
    v["diagnostics"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|d| d["code"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

fn create_entry(file: &str) -> String {
    json!([{
        "opId": format!("c-{}", file.replace('/', "-")),
        "type": "create-entry",
        "payload": { "file": file, "type": "fact", "title": "New", "body": "New body." }
    }])
    .to_string()
}

// 1. Write containment ------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn apply_refuses_writes_through_symlinked_directories_and_files() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let outside = tempdir().unwrap();
    symlink(outside.path(), f.scaffold.join("context/esc")).unwrap();

    fs::write(f.root.join("esc.json"), create_entry("context/esc/new.md")).unwrap();
    // Planned (dry run) and applied alike: refused, exit 5, nothing outside written.
    for args in [
        &["wiki", "apply", "esc.json", "--dry-run", "--json"][..],
        &["wiki", "apply", "esc.json", "--json"][..],
    ] {
        let (code, v) = f.json(args);
        assert_eq!(code, 5, "{}", v);
        assert_eq!(v["ok"], false);
        assert!(codes(&v).contains(&"PATH_OUTSIDE_SCAFFOLD".to_string()), "{}", v);
    }
    assert!(!outside.path().join("new.md").exists());

    // A symlinked Markdown file inside the scaffold is never written through or replaced.
    f.write("context/real.md", "---\nid: kb_real\ntitle: Real\ntype: fact\n---\n# Real\n");
    symlink(outside.path().join("target.md"), f.scaffold.join("context/link.md")).unwrap();
    fs::write(outside.path().join("target.md"), "outside\n").unwrap();
    fs::write(f.root.join("link.json"), create_entry("context/link.md")).unwrap();
    let (code, v) = f.json(&["wiki", "apply", "link.json", "--json"]);
    assert_eq!(code, 5, "{}", v);
    assert!(fs::symlink_metadata(f.scaffold.join("context/link.md"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        fs::read_to_string(outside.path().join("target.md")).unwrap(),
        "outside\n"
    );

    // The audit log directory is checked too.
    let events_out = tempdir().unwrap();
    symlink(events_out.path(), f.scaffold.join("events")).unwrap();
    fs::write(f.root.join("ok.json"), create_entry("context/fine.md")).unwrap();
    let (code, v) = f.json(&["wiki", "apply", "ok.json", "--json"]);
    assert_eq!(code, 5, "{}", v);
    assert!(!f.scaffold.join("context/fine.md").exists());
    assert!(fs::read_dir(events_out.path()).unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn regenerate_views_refuses_a_symlinked_view_file() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let view = "# Decisions\n\n<!-- kb:generated:begin -->\nstale\n<!-- kb:generated:end -->\n";
    f.write("real/decisions.md", view);
    // A contained file symlink is indexed, but never written through.
    symlink(
        f.scaffold.join("real/decisions.md"),
        f.scaffold.join("decisions.md"),
    )
    .unwrap();
    let scope = knobyte::wiki::scope::WikiScope::load(&f.scaffold);
    let r = knobyte::wiki::views::regenerate_views(&scope, false);
    let linked = r.files.iter().find(|x| x.file == "decisions.md").unwrap();
    assert!(!linked.written);
    assert!(r
        .diagnostics
        .iter()
        .any(|d| d.code == "PATH_OUTSIDE_SCAFFOLD" && d.file == "decisions.md"));
    assert!(fs::symlink_metadata(f.scaffold.join("decisions.md"))
        .unwrap()
        .file_type()
        .is_symlink());
}

// 2. Bounded relations ------------------------------------------------------------------

fn hub_fixture() -> Fixture {
    let f = Fixture::new();
    f.write(
        "context/hub.md",
        "---\nid: kb_hub\ntitle: Hub\ntype: architecture\n---\n# Hub\n\nHub body.\n",
    );
    for i in 0..60 {
        f.write(
            &format!("context/s{:02}.md", i),
            &format!(
                "---\nid: kb_s{:02}\ntitle: S{}\ntype: fact\ndepends_on: [kb_hub]\n---\n# S{}\n",
                i, i, i
            ),
        );
    }
    f.rebuild();
    f
}

#[test]
fn show_backlinks_and_get_are_bounded_and_paged() {
    let f = hub_fixture();
    let (code, v) = f.json(&["wiki", "show", "kb_hub", "--json"]);
    assert_eq!(code, 0, "{}", v);
    assert_eq!(v["data"]["backlinks"].as_array().unwrap().len(), 25);
    assert_eq!(v["data"]["backlinksPage"]["total"], 60);
    assert_eq!(v["data"]["backlinksPage"]["truncated"], true);
    assert_eq!(v["data"]["backlinksPage"]["nextOffset"], 25);

    let (_, v) = f.json(&["wiki", "backlinks", "kb_hub", "--limit", "10", "--offset", "50", "--json"]);
    assert_eq!(v["data"]["items"].as_array().unwrap().len(), 10);
    assert_eq!(v["data"]["total"], 60);
    assert_eq!(v["data"]["truncated"], false);
    assert_eq!(v["data"]["items"][0]["id"], "kb_s50");

    // MCP get: body opt-in, links bounded.
    let v = f.mcp("knobyte_wiki_get", json!({ "id": "kb_hub" }));
    assert_eq!(v["ok"], true, "{}", v);
    assert!(v["data"].get("body").is_none(), "{}", v);
    assert_eq!(v["data"]["bodyIncluded"], false);
    assert_eq!(v["data"]["backlinks"].as_array().unwrap().len(), 25);
    assert_eq!(v["data"]["backlinksPage"]["truncated"], true);
    let v = f.mcp(
        "knobyte_wiki_get",
        json!({ "id": "kb_hub", "includeBody": true, "limit": 5, "backlinksOffset": 55 }),
    );
    assert!(v["data"]["body"].as_str().unwrap().contains("Hub body."));
    assert_eq!(v["data"]["backlinks"].as_array().unwrap().len(), 5);
    assert_eq!(v["data"]["backlinksPage"]["truncated"], false);
    let v = f.mcp("knobyte_wiki_get", json!({ "id": "kb_hub", "limit": 0 }));
    assert_eq!(v["ok"], false);

    // The bounded API adapters (the Hub) use.
    let idx = WikiIndex::open_read_only(&f.scaffold.join("wiki.db")).unwrap();
    let d = idx
        .entity_detail(
            "kb_hub",
            &knobyte::wiki::index::DetailOptions {
                include_related: true,
                limit: Some(7),
                ..Default::default()
            },
        )
        .unwrap()
        .unwrap();
    assert!(d.entity.body.is_empty());
    assert_eq!(d.backlinks.items.len(), 7);
    let related = d.related.unwrap();
    assert_eq!((related.items.len(), related.total, related.truncated), (7, 60, true));
}

// 3. Refresh failures in envelopes ------------------------------------------------------

#[test]
fn list_json_reports_a_corpus_limit_refresh_failure() {
    let f = hub_fixture();
    let big = format!(
        "---\nid: kb_big\ntitle: Big\ntype: fact\n---\n# Big\n\n{}\n",
        "x".repeat(9 * 1024 * 1024)
    );
    f.write("context/big.md", &big);
    let (code, v) = f.json(&["wiki", "list", "--json"]);
    assert_eq!(v["ok"], false, "{}", v);
    assert_ne!(code, 0);
    assert!(codes(&v).contains(&"WIKI_CORPUS_LIMIT_EXCEEDED".to_string()), "{}", v);
    // The answer is still served from the existing index.
    assert!(!v["data"]["items"].as_array().unwrap().is_empty());
}

#[test]
fn busy_refresh_is_a_warning_and_a_corpus_bound_an_error() {
    let busy = knobyte::wiki::cli::refresh_failure_diagnostic(&coded_error(
        "WIKI_INDEX_BUSY",
        "held",
    ));
    assert_eq!((busy.code.as_str(), busy.severity.as_str()), ("WIKI_INDEX_BUSY", "warning"));
    let bound = knobyte::wiki::cli::refresh_failure_diagnostic(&coded_error(
        "WIKI_CORPUS_LIMIT_EXCEEDED",
        "too big",
    ));
    assert_eq!(bound.severity, "error");
}

// 4. One index-state vocabulary ---------------------------------------------------------

#[test]
fn newer_schema_index_fails_doctor_and_every_adapter_agrees() {
    let f = hub_fixture();
    let c = rusqlite::Connection::open(f.scaffold.join("wiki.db")).unwrap();
    c.execute("UPDATE wiki_meta SET value = '999' WHERE key = 'schema_version'", [])
        .unwrap();
    drop(c);

    let (code, doctor) = f.json(&["wiki", "index", "doctor", "--json"]);
    assert_eq!(doctor["ok"], false, "{}", doctor);
    assert_eq!(code, 3);
    assert_eq!(doctor["data"]["index"]["state"], "rebuild_required");

    let (code, status) = f.json(&["wiki", "index", "status", "--json"]);
    assert_eq!(code, 3);
    assert_eq!(status["data"]["index"]["state"], "rebuild_required");

    let (_, validate) = f.json(&["wiki", "validate", "--json"]);
    assert_eq!(validate["data"]["index"]["state"], "rebuild_required");

    let mcp = f.mcp("knobyte_wiki_validate", json!({}));
    assert_eq!(mcp["data"]["index"]["state"], "rebuild_required");
    // The newer index is left untouched.
    let c = rusqlite::Connection::open(f.scaffold.join("wiki.db")).unwrap();
    let v: String = c
        .query_row("SELECT value FROM wiki_meta WHERE key = 'schema_version'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, "999");
}

#[test]
fn migration_required_does_not_hide_stale() {
    let f = hub_fixture();
    // Legacy status (migration) written after the build (stale).
    f.write(
        "context/legacy.md",
        "---\nid: kb_legacy\ntitle: L\ntype: fact\nstatus: accepted\n---\n# L\n",
    );
    let (_, status) = f.json(&["wiki", "index", "status", "--json"]);
    assert_eq!(status["data"]["index"]["state"], "migration_required", "{}", status);
    assert_eq!(status["data"]["index"]["stale"], true);
    let c = codes(&status);
    assert!(c.contains(&"WIKI_MIGRATION_REQUIRED".to_string()), "{:?}", c);
    assert!(c.contains(&"INDEX_REFRESH_REQUIRED".to_string()), "{:?}", c);

    let (_, validate) = f.json(&["wiki", "validate", "--json"]);
    assert_eq!(validate["data"]["index"], status["data"]["index"]);
    let mcp = f.mcp("knobyte_wiki_validate", json!({}));
    assert_eq!(mcp["data"]["index"], status["data"]["index"]);
}

// 5. Symlink discovery diagnostics ------------------------------------------------------

#[cfg(unix)]
#[test]
fn escaping_directory_and_non_markdown_symlinks_are_reported() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("x.md"), "# X\n").unwrap();
    fs::write(outside.path().join("data.json"), "{}").unwrap();
    symlink(outside.path(), f.scaffold.join("context/dir")).unwrap();
    symlink(outside.path().join("data.json"), f.scaffold.join("context/data.json")).unwrap();
    symlink(f.scaffold.join("context/missing.txt"), f.scaffold.join("context/broken.txt")).unwrap();
    let scope = knobyte::wiki::scope::WikiScope::load(&f.scaffold);
    let d = scope.discover_checked();
    let at = |file: &str, code: &str| {
        d.diagnostics
            .iter()
            .any(|x| x.file == file && x.code == code)
    };
    assert!(at("context/dir", "PATH_OUTSIDE_SCAFFOLD"), "{:?}", d.diagnostics);
    assert!(at("context/data.json", "PATH_OUTSIDE_SCAFFOLD"), "{:?}", d.diagnostics);
    assert!(at("context/broken.txt", "WIKI_PARSE_ERROR"), "{:?}", d.diagnostics);
    assert!(!d.files.iter().any(|(r, _)| r.starts_with("context/dir")));
}

// 6. Smaller fixes ----------------------------------------------------------------------

#[test]
fn synthesis_usage_and_agent_response_codes() {
    let f = Fixture::new();
    let (code, v) = f.json(&["wiki", "synthesis", "prepare", "--stage", "bogus", "--json"]);
    assert_eq!(code, 2, "{}", v);
    assert_eq!(codes(&v), vec!["INVALID_REQUEST".to_string()]);
    let (code, _, _) = f.run(&["wiki", "synthesis", "prepare", "--stage", "bogus"]);
    assert_eq!(code, 2);

    fs::write(f.root.join("resp.txt"), "this is not json").unwrap();
    let (code, v) = f.json(&["wiki", "synthesis", "propose", "resp.txt", "--json"]);
    assert_eq!(codes(&v), vec!["INVALID_AGENT_RESPONSE".to_string()], "{}", v);
    assert_eq!(code, 1);
    fs::write(f.root.join("resp.json"), r#"{"stage":"global"}"#).unwrap();
    let (_, v) = f.json(&["wiki", "synthesis", "propose", "resp.json", "--json"]);
    assert_eq!(codes(&v), vec!["INVALID_AGENT_RESPONSE".to_string()], "{}", v);
}

#[test]
fn migrate_bumps_revision_once_and_keeps_list_indentation() {
    let f = Fixture::new();
    f.write(
        "context/old.md",
        "---\ntitle: Old\ntype: document\nstatus: accepted\nrevision: 3\ngrounds_to:\n  - node_id: function:src/a.rs:f\n  - node_id: function:src/a.rs:g\n---\n# Old\n\nBody.\n",
    );
    let scope = knobyte::wiki::scope::WikiScope::load(&f.scaffold);
    let r = knobyte::wiki::migrate::migrate(
        &knobyte::wiki::migrate::MigrationOptions {
            scope: &scope,
            graph_db: None,
        },
        false,
    );
    assert!(r.applied, "{:?}", r.report.map(|x| x.diagnostics));
    assert!(r.plan.items.len() >= 4, "{:?}", r.plan.items);
    let text = f.read("context/old.md");
    assert!(text.contains("revision: 4\n"), "{}", text);
    assert!(
        text.contains("grounds_to:\n  - function:src/a.rs:f\n  - function:src/a.rs:g\n"),
        "{}",
        text
    );
}

#[test]
fn supersession_cycle_is_located_at_a_superseding_entity() {
    let f = Fixture::new();
    f.write(
        "decisions/a.md",
        "---\nid: kb_a\ntitle: A\ntype: decision\nsupersedes: [kb_b]\n---\n# A\n",
    );
    f.write(
        "decisions/b.md",
        "---\nid: kb_b\ntitle: B\ntype: decision\nsupersedes: [kb_a]\n---\n# B\n",
    );
    // A duplicate of kb_a without the edge must not be where the cycle is reported.
    f.write(
        "decisions/0dup.md",
        "---\nid: kb_a\ntitle: A copy\ntype: decision\n---\n# A copy\n",
    );
    let (_, v) = f.json(&["wiki", "validate", "--json"]);
    let d = v["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "SUPERSESSION_CYCLE")
        .cloned()
        .unwrap_or_else(|| panic!("{}", v));
    assert_eq!(d["entityId"], "kb_a", "{}", d);
    assert_eq!(d["file"], "decisions/a.md", "{}", d);
    assert!(d["path"].as_str().unwrap().starts_with("relations["), "{}", d);
}

#[test]
fn finalize_failure_message_lists_codes() {
    let mut fin = knobyte::wiki::finalize::WikiFinalization {
        stage: "validation".into(),
        reason: Some("wiki validation found blocking errors".into()),
        ..Default::default()
    };
    for i in 0..10 {
        fin.diagnostics.push(knobyte::wiki::diagnostics::diag(
            if i % 2 == 0 { "INVALID_RELATION_TARGET" } else { "SUPERSESSION_CYCLE" },
            format!("problem {}", i),
            "context/a.md",
        ));
    }
    let msg = fin.failure_message();
    assert!(
        msg.contains("diagnostic codes: INVALID_RELATION_TARGET x5, SUPERSESSION_CYCLE x5"),
        "{}",
        msg
    );
    assert!(msg.contains("and 2 more"), "{}", msg);
}

#[test]
fn export_io_errors_name_the_path_and_operation() {
    let f = Fixture::new();
    f.write("context/a.md", "# A\n");
    fs::write(f.root.join("blocker"), "a file, not a directory").unwrap();
    let (code, out, err) = f.run(&["export", "--out", "blocker/sub/x.md"]);
    assert_ne!(code, 0, "{}", out);
    let all = format!("{}{}", out, err);
    assert!(all.contains("Could not create directory"), "{}", all);
    assert!(all.contains("blocker/sub"), "{}", all);
}
