//! Regression tests for wiki index safety and correctness fixes: schema-version handling,
//! corpus limits, config-sensitive refresh, parser edge cases, for-code grouping, bounded
//! listings, exit codes and export safety.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use knobyte::wiki::index::{error_code, WikiIndex, WIKI_INDEX_SCHEMA_VERSION};
use rusqlite::Connection;
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
        let p = self.scaffold.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn db(&self) -> PathBuf {
        self.scaffold.join("wiki.db")
    }

    fn index(&self) -> WikiIndex {
        let mut idx = WikiIndex::open(&self.db()).unwrap();
        idx.refresh(&self.scaffold).unwrap();
        idx
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        // The CLI project guard requires a completed setup (ROUTER.md).
        let router = self.scaffold.join("ROUTER.md");
        if !router.exists() {
            fs::write(router, "# Router\n").unwrap();
        }
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

fn set_version(db: &PathBuf, v: &str) {
    let c = Connection::open(db).unwrap();
    c.execute(
        "UPDATE wiki_meta SET value = ?1 WHERE key = 'schema_version'",
        [v],
    )
    .unwrap();
}

fn entity_count(db: &PathBuf) -> i64 {
    let c = Connection::open(db).unwrap();
    c.query_row("SELECT COUNT(*) FROM wiki_entities", [], |r| r.get(0))
        .unwrap()
}

// ---------------------------------------------------------------------------
// 1. Schema version mismatch never wipes on read paths
// ---------------------------------------------------------------------------

#[test]
fn version_mismatch_is_reported_not_wiped_on_read() {
    let f = Fixture::new();
    f.write("context/a.md", "---\nid: kb_a\ntitle: A\n---\n# A\n");
    drop(f.index());
    assert_eq!(entity_count(&f.db()), 1);

    set_version(&f.db(), "1");
    let err = WikiIndex::open(&f.db()).err().expect("older index must not open");
    assert_eq!(error_code(&err), Some("WIKI_INDEX_REBUILD_REQUIRED"));
    let err = WikiIndex::open_read_only(&f.db()).err().unwrap();
    assert_eq!(error_code(&err), Some("WIKI_INDEX_REBUILD_REQUIRED"));
    // Rows survive the read attempts.
    assert_eq!(entity_count(&f.db()), 1);

    // CLI read path: index exit code, rows kept.
    let (code, out, err) = f.run(&["wiki", "list", "--json"]);
    assert_eq!(code, 3, "{}", err);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["diagnostics"][0]["code"], "WIKI_INDEX_REBUILD_REQUIRED");
    let (code, _, err) = f.run(&["wiki", "list"]);
    assert_eq!(code, 3);
    assert!(err.contains("WIKI_INDEX_REBUILD_REQUIRED"), "{}", err);
    assert_eq!(entity_count(&f.db()), 1);

    // Explicit rebuild resets an older index.
    let (code, out, err) = f.run(&["wiki", "rebuild-index"]);
    assert_eq!(code, 0, "{}{}", out, err);
    let idx = WikiIndex::open(&f.db()).unwrap();
    assert!(idx.show("kb_a").unwrap().is_some());
}

#[test]
fn newer_index_is_never_touched() {
    let f = Fixture::new();
    f.write("context/a.md", "---\nid: kb_a\ntitle: A\n---\n# A\n");
    drop(f.index());
    let newer = (WIKI_INDEX_SCHEMA_VERSION.parse::<u32>().unwrap() + 1).to_string();
    set_version(&f.db(), &newer);

    for open in [
        WikiIndex::open(&f.db()).err(),
        WikiIndex::open_read_only(&f.db()).err(),
        WikiIndex::open_for_rebuild(&f.db()).err(),
    ] {
        assert_eq!(
            error_code(&open.expect("newer index must not open")),
            Some("WIKI_INDEX_REBUILD_REQUIRED")
        );
    }
    let (code, _, _) = f.run(&["wiki", "rebuild-index"]);
    assert_eq!(code, 3);
    assert_eq!(entity_count(&f.db()), 1);
    let c = Connection::open(f.db()).unwrap();
    let v: String = c
        .query_row(
            "SELECT value FROM wiki_meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, newer);
}

#[test]
fn read_only_open_of_missing_index_reports_missing() {
    let f = Fixture::new();
    let err = WikiIndex::open_read_only(&f.db()).err().unwrap();
    assert_eq!(error_code(&err), Some("WIKI_INDEX_MISSING"));
    assert!(!f.db().exists());
}

// ---------------------------------------------------------------------------
// 2. Corpus limit aborts the refresh and keeps rows
// ---------------------------------------------------------------------------

#[test]
fn corpus_limit_keeps_existing_rows() {
    let f = Fixture::new();
    f.write("context/a.md", "---\nid: kb_a\ntitle: A\n---\n# A\n");
    f.write("context/b.md", "---\nid: kb_b\ntitle: B\n---\n# B\n");
    let mut idx = f.index();
    assert!(idx.show("kb_b").unwrap().is_some());

    // b.md grows past the per-file bound.
    let mut big = String::from("---\nid: kb_b\ntitle: B\n---\n# B\n");
    big.push_str(&"x".repeat((knobyte::wiki::scope::MAX_FILE_BYTES + 10) as usize));
    f.write("context/b.md", &big);
    let err = idx.refresh(&f.scaffold).expect_err("limit aborts refresh");
    assert_eq!(error_code(&err), Some("WIKI_CORPUS_LIMIT_EXCEEDED"));
    assert!(idx.show("kb_a").unwrap().is_some());
    assert!(
        idx.show("kb_b").unwrap().is_some(),
        "an over-limit file is not treated as deleted"
    );
    let err = idx.rebuild(&f.scaffold).err().unwrap();
    assert_eq!(error_code(&err), Some("WIKI_CORPUS_LIMIT_EXCEEDED"));
    assert!(idx.show("kb_b").unwrap().is_some(), "rebuild keeps rows too");
    let c = Connection::open(f.db()).unwrap();
    let n: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM wiki_diagnostics WHERE code = 'WIKI_CORPUS_LIMIT_EXCEEDED'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

// ---------------------------------------------------------------------------
// 3. Config changes re-evaluate the corpus
// ---------------------------------------------------------------------------

#[test]
fn entity_type_config_change_refreshes_results() {
    let f = Fixture::new();
    f.write(
        "context/a.md",
        "---\nid: kb_a\ntitle: A\ntype: runbook\n---\n# A\n",
    );
    let mut idx = f.index();
    let before = idx.show("kb_a").unwrap().unwrap();
    let c = Connection::open(f.db()).unwrap();
    let invalid: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM wiki_diagnostics WHERE code = 'INVALID_ENTITY_TYPE'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(invalid > 0, "runbook is not a registered type yet ({:?})", before.entity_type);

    f.write(
        "config.json",
        r#"{"wiki": {"entityTypes": ["runbook"]}}"#,
    );
    assert!(idx.freshness(&f.scaffold).unwrap().config_changed);
    let stats = idx.refresh(&f.scaffold).unwrap();
    assert_eq!(stats.files_updated, 1, "config change re-reads unchanged files");
    let after = idx.show("kb_a").unwrap().unwrap();
    assert_eq!(after.entity_type, "runbook");
    let invalid: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM wiki_diagnostics WHERE code = 'INVALID_ENTITY_TYPE'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(invalid, 0);
    let stats = idx.refresh(&f.scaffold).unwrap();
    assert_eq!(stats.files_unchanged, 1);
}

// ---------------------------------------------------------------------------
// Export safety
// ---------------------------------------------------------------------------

#[test]
fn export_refuses_existing_and_replaces_only_previous_bundles() {
    let f = Fixture::new();
    f.write("context/a.md", "# A\n");
    fs::write(f.root.join("notes.md"), "mine").unwrap();
    let err = knobyte::wiki::export::export_scaffold(&f.root, &f.scaffold, Some("notes.md"))
        .err()
        .unwrap();
    assert!(err.contains("already exists"), "{}", err);
    assert_eq!(fs::read_to_string(f.root.join("notes.md")).unwrap(), "mine");

    knobyte::wiki::export::export_scaffold(&f.root, &f.scaffold, Some("bundle.md")).unwrap();
    f.write("context/b.md", "# B\n");
    knobyte::wiki::export::export_scaffold(&f.root, &f.scaffold, Some("bundle.md")).unwrap();
    let text = fs::read_to_string(f.root.join("bundle.md")).unwrap();
    assert!(text.contains("## context/b.md"));
    let leftovers: Vec<_> = fs::read_dir(&f.root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty());

    #[cfg(unix)]
    {
        // A symlink at the target is never followed, even if it points at a bundle.
        std::os::unix::fs::symlink(f.root.join("bundle.md"), f.root.join("link.md")).unwrap();
        let err = knobyte::wiki::export::export_scaffold(&f.root, &f.scaffold, Some("link.md"))
            .err()
            .unwrap();
        assert!(err.contains("already exists"), "{}", err);
    }
}

#[test]
fn json_list_keeps_truncated_flag() {
    let f = Fixture::new();
    for i in 0..3 {
        f.write(
            &format!("context/e{}.md", i),
            &format!("---\nid: kb_e{}\ntitle: E{}\n---\n# E{}\n", i, i, i),
        );
    }
    let (code, out, err) = f.run(&["wiki", "list", "--limit", "2", "--json"]);
    assert_eq!(code, 0, "{}", err);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["data"]["truncated"], true);
    assert_eq!(v["data"]["nextOffset"], 2);
    assert_eq!(v["data"]["items"].as_array().unwrap().len(), 2);
    let (_, out, _) = f.run(&["wiki", "query", "E1", "--json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["data"]["truncated"], false);
    let (_, out, _) = f.run(&["wiki", "list", "--limit", "2", "--jsonl"]);
    assert_eq!(out.lines().count(), 3);
    assert!(out.lines().last().unwrap().contains("\"truncated\":true"));
}

// ---------------------------------------------------------------------------
// 4. Parser, discovery, operations, validation and synthesis edge cases
// ---------------------------------------------------------------------------

#[test]
fn stray_anchor_is_reported_not_attached_to_first_marker() {
    let text = "---\nid: kb_file\ntitle: F\n---\n# F\n\n<!-- kb:entity id=kb_one type=decision -->\n## One\n\nBody.\n\n# Plain\n\n<!-- kb-ground: function:src/a.rs:stray -->\n";
    let p = knobyte::wiki::parser::parse_markdown_file("context/x.md", text);
    for e in &p.entities {
        {
            assert!(
                !e.entity
                    .grounds_to
                    .contains(&"function:src/a.rs:stray".to_string()),
                "{} must not receive the stray anchor",
                e.entity.id
            );
        }
    }
    assert!(
        p.diagnostics.iter().any(|d| d.code == "UNBOUND_ANCHOR"),
        "{:?}",
        p.diagnostics
    );
    // With a file-level entity the anchor attaches to it.
    let text = "---\nid: kb_file\ntitle: F\n---\n# F\n<!-- kb-ground: function:src/a.rs:f -->\n\n<!-- kb:entity id=kb_two -->\n## Two\n";
    let p = knobyte::wiki::parser::parse_markdown_file("context/y.md", text);
    assert!(p
        .entity("kb_file")
        .unwrap()
        .entity
        .grounds_to
        .contains(&"function:src/a.rs:f".to_string()));
}

#[cfg(unix)]
#[test]
fn symlinked_markdown_is_followed_inside_and_reported_outside() {
    let f = Fixture::new();
    f.write("local/shared.md", "---\nid: kb_shared\ntitle: Shared\n---\n# Shared\n");
    std::os::unix::fs::symlink(
        f.scaffold.join("local/shared.md"),
        f.scaffold.join("context/shared.md"),
    )
    .unwrap();
    let outside = tempdir().unwrap();
    fs::write(
        outside.path().join("o.md"),
        "---\nid: kb_out\ntitle: Out\n---\n# Out\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path().join("o.md"), f.scaffold.join("context/out.md"))
        .unwrap();
    let idx = f.index();
    assert!(idx.show("kb_shared").unwrap().is_some());
    assert!(idx.show("kb_out").unwrap().is_none());
    let d = knobyte::wiki::scope::WikiScope::load(&f.scaffold).discover_checked();
    assert!(d
        .diagnostics
        .iter()
        .any(|d| d.code == "PATH_OUTSIDE_SCAFFOLD" && d.file == "context/out.md"));
    assert!(!d.limit_exceeded);
}

#[test]
fn create_entry_insert_before_entity_keeps_one_blank_line() {
    let f = Fixture::new();
    f.write(
        "context/a.md",
        "---\nid: kb_a\ntitle: A\n---\n# A\n\nIntro.\n\n<!-- kb:entity id=kb_b type=decision -->\n## B\n\nB body.\n",
    );
    let ops = serde_json::json!([{
        "opId": "c1", "type": "create-entry",
        "actor": { "kind": "human", "id": "t" }, "timestamp": "2026-01-01T00:00:00Z",
        "payload": {
            "file": "context/a.md", "type": "decision", "title": "New", "body": "New body.",
            "headingDepth": 2, "id": "kb_new",
            "insertAt": { "at": "before-entity", "entityId": "kb_b" }
        }
    }]);
    let scope = knobyte::wiki::scope::WikiScope::load(&f.scaffold);
    let report = knobyte::wiki::ops::apply_operations(
        ops.as_array().unwrap(),
        &knobyte::wiki::ops::ApplyOptions {
            scope: &scope,
            graph_db: None,
            dry_run: false,
            default_actor: knobyte::wiki::ops::OpActor {
                kind: "human".into(),
                id: "t".into(),
                session_id: None,
            },
        },
    );
    assert!(report.ok, "{:?}", report.diagnostics);
    let text = fs::read_to_string(f.scaffold.join("context/a.md")).unwrap();
    assert!(text.contains("New body.\n\n<!-- kb:entity id=kb_b"), "{}", text);
    assert!(!text.contains("\n\n\n"), "{:?}", text);
}

#[test]
fn supersession_cycle_found_through_duplicated_id() {
    let f = Fixture::new();
    f.write(
        "context/a.md",
        "---\nid: kb_x\ntype: decision\ntitle: X\nrelations:\n  - type: supersedes\n    target_id: kb_y\n---\n# X\n",
    );
    f.write(
        "context/b.md",
        "---\nid: kb_y\ntype: decision\ntitle: Y\nrelations:\n  - type: supersedes\n    target_id: kb_x\n---\n# Y\n",
    );
    // A later duplicate of kb_y without relations must not hide the cycle.
    f.write(
        "context/c.md",
        "---\nid: kb_y\ntype: decision\ntitle: Y copy\n---\n# Y copy\n",
    );
    let report = knobyte::wiki::validate::validate_path(&f.scaffold, None, None, None);
    assert!(
        report.diagnostics.iter().any(|d| d.code == "SUPERSESSION_CYCLE"),
        "{:?}",
        report.diagnostics.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
}

#[test]
fn synthesis_no_cluster_message_distinguishes_missing_graph() {
    let f = Fixture::new();
    let graph = f.scaffold.join("graph.db");
    let msg = knobyte::wiki::synthesis::no_clusters_message(&graph);
    assert!(msg.contains("not built"), "{}", msg);
    let c = Connection::open(&graph).unwrap();
    c.execute_batch("CREATE TABLE files (path TEXT PRIMARY KEY); INSERT INTO files VALUES ('src/a.rs');")
        .unwrap();
    drop(c);
    let msg = knobyte::wiki::synthesis::no_clusters_message(&graph);
    assert!(!msg.contains("graph rebuild"), "{}", msg);
    assert!(msg.contains("1 file"), "{}", msg);
}

#[test]
fn synthesis_source_reads_are_contained_and_capped() {
    use knobyte::wiki::synthesis::{read_source_contained, MAX_SYNTHESIS_SOURCE_BYTES};
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("src")).unwrap();
    fs::write(f.root.join("src/a.rs"), "fn a() {}\n").unwrap();
    assert!(read_source_contained(&f.root, "src/a.rs").is_some());
    assert!(read_source_contained(&f.root, "../etc/passwd").is_none());
    assert!(read_source_contained(&f.root, "/etc/passwd").is_none());
    fs::write(
        f.root.join("src/big.rs"),
        "x".repeat(MAX_SYNTHESIS_SOURCE_BYTES as usize + 1),
    )
    .unwrap();
    assert!(read_source_contained(&f.root, "src/big.rs").is_none());
    #[cfg(unix)]
    {
        let outside = tempdir().unwrap();
        fs::write(outside.path().join("secret.rs"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.rs"), f.root.join("src/link.rs"))
            .unwrap();
        assert!(read_source_contained(&f.root, "src/link.rs").is_none());
    }
}

// ---------------------------------------------------------------------------
// 5. for-code: one hit per entity, ranked by matched nodes, archived hidden
// ---------------------------------------------------------------------------

#[test]
fn for_code_groups_per_entity_and_hides_archived() {
    let f = Fixture::new();
    f.write(
        "context/one.md",
        "---\nid: kb_one\ntitle: One\ngrounds_to: [function:src/a.rs:x]\n---\n# One\n",
    );
    f.write(
        "context/both.md",
        "---\nid: kb_both\ntitle: Both\ngrounds_to: [function:src/a.rs:x, function:src/b.rs:y]\n---\n# Both\n",
    );
    f.write(
        "context/old.md",
        "---\nid: kb_old\ntitle: Old\nstatus: archived\ngrounds_to: [function:src/a.rs:x, function:src/b.rs:y]\n---\n# Old\n",
    );
    let idx = f.index();
    let q = vec![
        "function:src/a.rs:x".to_string(),
        "function:src/b.rs:y".to_string(),
    ];
    let page = idx.for_code_many(&q, Some(10)).unwrap();
    let ids: Vec<&str> = page.items.iter().map(|h| h.entity.id.as_str()).collect();
    assert_eq!(ids, vec!["kb_both", "kb_one"], "one hit per entity, overlap first");
    assert_eq!(page.items[0].matched_nodes, q);
    assert_eq!(page.items[1].matched_nodes, vec![q[0].clone()]);

    let all = idx.for_code_filtered(&q, Some(10), true).unwrap();
    assert!(all.items.iter().any(|h| h.entity.id == "kb_old"));

    let one = idx.for_code_many(&q, Some(1)).unwrap();
    assert_eq!(one.items.len(), 1);
    assert!(one.truncated);

    let (code, out, err) = f.run(&["wiki", "for-code", "function:src/a.rs:x", "--json"]);
    assert_eq!(code, 0, "{}", err);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["data"]["truncated"], false);
    assert!(v["data"]["items"][0]["matchedNodes"].is_array());
}

// ---------------------------------------------------------------------------
// 6. Bounded, paged listings and exit codes
// ---------------------------------------------------------------------------

fn tool(f: &Fixture, name: &str, args: Value) -> Value {
    let config = knobyte::config::KnobyteConfig::new(f.root.clone(), f.scaffold.clone());
    let r = knobyte::mcp::tools::execute_tool_with_config(name, &args, &config);
    assert!(r.is_error != Some(true), "{}", r.content[0].text);
    serde_json::from_str(&r.content[0].text).unwrap()
}

#[test]
fn mcp_wiki_list_and_empty_query_are_bounded_and_paged() {
    let f = Fixture::new();
    for i in 0..5 {
        f.write(
            &format!("context/e{}.md", i),
            &format!("---\nid: kb_e{}\ntitle: E{}\n---\n# E{}\n\nBody {}.\n", i, i, i, i),
        );
    }
    f.write(
        "context/arch.md",
        "---\nid: kb_arch\ntitle: Archived\nstatus: archived\n---\n# Archived\n",
    );
    f.write("context/dup.md", "---\nid: kb_e0\ntitle: Dup\n---\n# Dup\n");
    drop(f.index());

    let v = tool(&f, "knobyte_wiki_list", serde_json::json!({ "limit": 2 }));
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    assert_eq!(v["truncated"], true);
    assert_eq!(v["nextOffset"], 2);
    assert!(v["items"][0].get("body").is_none(), "no bodies by default");

    let v = tool(&f, "knobyte_wiki_list", serde_json::json!({ "limit": 50 }));
    let ids: Vec<&str> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 5, "archived and shadowed hidden: {:?}", ids);
    assert!(!ids.contains(&"kb_arch"));

    let v = tool(
        &f,
        "knobyte_wiki_list",
        serde_json::json!({ "offset": 4, "includeBody": true, "includeArchived": true }),
    );
    assert_eq!(v["truncated"], false);
    assert!(v["items"][0]["body"].is_string());

    let v = tool(&f, "knobyte_wiki_query", serde_json::json!({ "text": "", "limit": 3 }));
    assert_eq!(v["items"].as_array().unwrap().len(), 3);
    assert_eq!(v["truncated"], true);
    let v = tool(&f, "knobyte_wiki_query", serde_json::json!({ "text": "E3" }));
    assert_eq!(v["items"][0]["id"], "kb_e3");
}

#[test]
fn wiki_exit_codes_follow_taxonomy() {
    let f = Fixture::new();
    f.write(
        "context/a.md",
        "---\nid: kb_a\ntitle: A\nrelations:\n  - type: depends_on\n    target_id: kb_nope\n---\n# A\n",
    );
    // validate: error-severity findings exit 1 without --strict
    let (code, _, _) = f.run(&["wiki", "validate"]);
    assert_eq!(code, 1);

    // precondition conflict: exit 4
    let ops = serde_json::json!([{
        "opId": "u1", "type": "set-property", "entityId": "kb_a",
        "actor": { "kind": "human", "id": "t" }, "timestamp": "2026-01-01T00:00:00Z",
        "baseRevision": 99,
        "payload": { "property": "status", "value": "deprecated" }
    }]);
    fs::write(f.root.join("ops.json"), ops.to_string()).unwrap();
    let (code, out, err) = f.run(&["wiki", "apply", "ops.json"]);
    assert_eq!(code, 4, "{}{}", out, err);

    // unparseable operation file: usage, exit 2
    fs::write(f.root.join("bad.json"), "not json").unwrap();
    let (code, _, _) = f.run(&["wiki", "apply", "bad.json"]);
    assert_eq!(code, 2);
}
