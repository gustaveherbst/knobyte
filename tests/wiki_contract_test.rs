//! Wiki contract: one JSON envelope and exit-code taxonomy across every command, diagnostic
//! positions, the grounding-shape and anchor checks in `wiki validate`, index maintenance
//! (lease, abort, corpus-unchanged assertion, states, dump, doctor), and the snapshot-bound
//! read session with revision-bound cursors.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use knobyte::wiki::index::{error_code, WikiIndex};
use knobyte::wiki::maintenance::{
    acquire_lease, dump_index, inspect_index, MaintenanceContext, LEASE_WAIT,
};
use knobyte::wiki::scope::WikiScope;
use knobyte::wiki::session::{
    open_read_session, Direction, ListRequest, NeighborhoodRequest, RelationRequest, SearchRequest,
};
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
        fs::write(
            scaffold.join("ROUTER.md"),
            "---\nid: kb_router\ntitle: Router\ntype: guide\n---\n# Router\n",
        )
        .unwrap();
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

    fn scope(&self) -> WikiScope {
        WikiScope::load(&self.scaffold)
    }

    fn rebuild(&self) {
        let mut idx = WikiIndex::open_for_rebuild(&self.db()).unwrap();
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
            .unwrap_or_else(|e| panic!("{:?}: not JSON ({}): {}{}", args, e, out, err));
        (code, v)
    }
}

fn corpus(f: &Fixture) {
    f.write(
        "context/a.md",
        "---\nid: kb_a\ntitle: Alpha\ntype: architecture\nsummary: The alpha component.\ndepends_on: [kb_b]\n---\n# Alpha\n\nAlpha body.\n",
    );
    f.write(
        "context/b.md",
        "---\nid: kb_b\ntitle: Beta\ntype: component\nimplements: [kb_c]\n---\n# Beta\n\nBeta body.\n",
    );
    f.write(
        "decisions/c.md",
        "---\nid: kb_c\ntitle: Gamma decision\ntype: decision\n---\n# Gamma decision\n\nGamma.\n",
    );
}

fn assert_envelope(v: &Value) {
    assert_eq!(v["schemaVersion"], 1, "{}", v);
    assert!(v["ok"].is_boolean(), "{}", v);
    assert!(v.get("data").is_some(), "{}", v);
    assert!(v["diagnostics"].is_array(), "{}", v);
}

/// Every key of an object tree (except user-data maps) is camelCase.
fn assert_camel(v: &Value, path: &str) {
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                if ["metadata", "payload", "nodes", "prompt"].contains(&k.as_str()) {
                    continue;
                }
                assert!(!k.contains('_'), "snake_case key {}.{}", path, k);
                assert_camel(val, &format!("{}.{}", path, k));
            }
        }
        Value::Array(a) => a.iter().for_each(|x| assert_camel(x, path)),
        _ => {}
    }
}

#[test]
fn every_wiki_command_answers_with_one_camelcase_envelope() {
    let f = Fixture::new();
    corpus(&f);
    assert_eq!(f.run(&["wiki", "rebuild-index"]).0, 0);
    fs::write(
        f.root.join("ops.json"),
        r#"[{"opId":"c1","type":"set-property","entityId":"kb_c","payload":{"property":"summary","value":"Why gamma."}}]"#,
    )
    .unwrap();
    let commands: Vec<Vec<&str>> = vec![
        vec!["wiki", "list", "--json"],
        vec!["wiki", "show", "kb_a", "--json"],
        vec!["wiki", "query", "alpha", "--json"],
        vec!["wiki", "related", "kb_a", "--json"],
        vec!["wiki", "backlinks", "kb_b", "--json"],
        vec!["wiki", "for-code", "function:src/x.rs:y", "--json"],
        vec!["wiki", "graph", "kb_a", "--json"],
        vec!["wiki", "trace", "kb_c", "--json"],
        vec!["wiki", "validate", "--json"],
        vec!["wiki", "rebuild-index", "--json"],
        vec!["wiki", "apply", "ops.json", "--dry-run", "--json"],
        vec!["wiki", "regenerate-views", "--dry-run", "--json"],
        vec!["wiki", "migrate", "--json"],
        vec!["wiki", "index", "status", "--json"],
        vec!["wiki", "index", "dump", "--json"],
        vec!["wiki", "index", "doctor", "--json"],
        vec!["wiki", "synthesis", "build", "--json"],
    ];
    for args in &commands {
        let (code, v) = f.json(args);
        assert_envelope(&v);
        assert_eq!(code, 0, "{:?}: {}", args, v);
        assert_eq!(v["ok"], true, "{:?}: {}", args, v);
        assert_camel(&v, &args.join(" "));
    }
    let (_, v) = f.json(&["wiki", "show", "kb_a", "--json"]);
    assert_eq!(v["data"]["entityType"], "architecture");
    assert_eq!(v["data"]["startLine"], 1);
}

#[test]
fn exit_codes_follow_the_taxonomy() {
    let f = Fixture::new();
    corpus(&f);
    // index: no index yet for a read-only command.
    let (code, v) = f.json(&["wiki", "index", "dump", "--json"]);
    assert_eq!(code, 3, "{}", v);
    assert_eq!(v["ok"], false);
    assert_eq!(v["diagnostics"][0]["code"], "WIKI_INDEX_MISSING");
    assert!(v["diagnostics"][0]["remediation"].is_string());

    assert_eq!(f.run(&["wiki", "rebuild-index"]).0, 0);
    // diagnostics: not found.
    let (code, v) = f.json(&["wiki", "show", "kb_nope", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(v["diagnostics"][0]["code"], "ENTITY_NOT_FOUND");
    assert_eq!(v["diagnostics"][0]["entityId"], "kb_nope");
    let (code, _, _) = f.run(&["wiki", "trace", "kb_nope"]);
    assert_eq!(code, 1);

    // usage: unparseable operation file.
    fs::write(f.root.join("bad.json"), "{not json").unwrap();
    let (code, v) = f.json(&["wiki", "apply", "bad.json", "--json"]);
    assert_eq!(code, 2, "{}", v);
    assert_eq!(v["diagnostics"][0]["code"], "INVALID_OPERATION_ENVELOPE");

    // precondition: a stale base revision.
    fs::write(
        f.root.join("stale.json"),
        r#"[{"opId":"s1","type":"set-property","entityId":"kb_c","baseRevision":7,"payload":{"property":"summary","value":"x"}}]"#,
    )
    .unwrap();
    let (code, v) = f.json(&["wiki", "apply", "stale.json", "--json"]);
    assert_eq!(code, 4, "{}", v);
    assert_eq!(v["ok"], false);

    // refused: a write to a team-owned path.
    fs::write(
        f.root.join("team.json"),
        r##"[{"opId":"t1","type":"create-entry","payload":{"file":"team/x.md","type":"fact","title":"X","body":"x"}}]"##,
    )
    .unwrap();
    let (code, v) = f.json(&["wiki", "apply", "team.json", "--json"]);
    assert_eq!(code, 5, "{}", v);

    // diagnostics: validate with an error finding.
    f.write(
        "context/bad.md",
        "---\nid: kb_bad\ntitle: Bad\ntype: fact\ndepends_on: [kb_missing]\n---\n# Bad\n",
    );
    let (code, v) = f.json(&["wiki", "validate", "--json"]);
    assert_eq!(code, 1);
    assert_eq!(v["ok"], false);
    assert!(v["data"]["counts"]["error"].as_u64().unwrap() >= 1);
}

#[test]
fn diagnostics_carry_line_column_and_span() {
    let f = Fixture::new();
    f.write(
        "context/pos.md",
        "---\nid: kb_pos\ntitle: Pos\ntype: fact\nstatus: accepted\nrelations:\n  - type: depends_on\n    target_id: kb_a1\n  - type: depends_on\n    target_id: kb_missing\n---\n# Pos\n",
    );
    f.write(
        "context/a1.md",
        "---\nid: kb_a1\ntitle: A1\ntype: fact\n---\n# A1\n",
    );
    let (_, v) = f.json(&["wiki", "validate", "--json"]);
    let diags = v["diagnostics"].as_array().unwrap();
    let legacy = diags
        .iter()
        .find(|d| d["code"] == "LEGACY_LIFECYCLE_STATE")
        .unwrap();
    let loc = &legacy["location"];
    assert_eq!(loc["startLine"], 5, "{}", legacy);
    assert_eq!(loc["startColumn"], 1);
    assert_eq!(loc["endLine"], 5);
    assert_eq!(
        loc["endColumn"], 17,
        "`status: accepted` spans 16 columns: {}",
        legacy
    );
    assert_eq!(legacy["line"], 5);
    let target = diags
        .iter()
        .find(|d| d["code"] == "INVALID_RELATION_TARGET")
        .unwrap();
    // The second relation item, not the key.
    assert_eq!(target["location"]["startLine"], 9, "{}", target);
    assert_eq!(target["location"]["startColumn"], 3);
    let text = fs::read_to_string(f.scaffold.join("context/pos.md")).unwrap();
    let s = target["location"]["startOffset"].as_u64().unwrap() as usize;
    let e = target["location"]["endOffset"].as_u64().unwrap() as usize;
    assert_eq!(&text[s..e], "- type: depends_on");
}

#[test]
fn validate_reports_grounding_shape_and_anchor_mismatch() {
    let f = Fixture::new();
    f.write(
        "context/mixed.md",
        "---\nid: kb_mixed\ntitle: Mixed\ntype: fact\ngrounds_to:\n  - function:src/a.rs:f\n  - node_id: function:src/b.rs:g\n---\n# Mixed\n",
    );
    f.write(
        "context/anchor.md",
        "---\nid: kb_anchor\ntitle: Anchor\ntype: fact\ngrounds_to:\n  - ref: function:src/a.rs:f\n    body_hash: h1\n  - function:src/old.rs:moved\n---\n# Anchor\n\nText.\n<!-- kb-ground: function:src/a.rs:f #h2 -->\n<!-- kb-ground: function:src/new.rs:moved -->\n",
    );
    let (_, v) = f.json(&["wiki", "validate", "--json"]);
    let diags = v["diagnostics"].as_array().unwrap();
    let shape: Vec<&Value> = diags
        .iter()
        .filter(|d| d["code"] == "GROUNDING_MIXED_SHAPE")
        .collect();
    assert_eq!(shape.len(), 1, "{:#?}", diags);
    assert_eq!(shape[0]["file"], "context/mixed.md");
    assert_eq!(shape[0]["entityId"], "kb_mixed");
    let mismatch: Vec<&Value> = diags
        .iter()
        .filter(|d| d["code"] == "ANCHOR_GROUNDING_MISMATCH")
        .collect();
    assert_eq!(mismatch.len(), 2, "{:#?}", mismatch);
    assert!(mismatch
        .iter()
        .any(|d| d["message"].as_str().unwrap().contains("#h2")));
    assert!(mismatch
        .iter()
        .any(|d| d["message"].as_str().unwrap().contains("src/new.rs")));
    assert_eq!(mismatch[0]["location"]["startColumn"], 1);
    assert!(mismatch
        .iter()
        .all(|d| d["location"]["startLine"].as_u64().unwrap() >= 13));
}

#[test]
fn index_states_dump_and_doctor() {
    let f = Fixture::new();
    corpus(&f);
    let scope = f.scope();
    assert_eq!(inspect_index(&f.db(), &scope, true).state, "missing");
    f.rebuild();
    let st = inspect_index(&f.db(), &scope, true);
    assert_eq!(st.state, "fresh", "{:#?}", st.diagnostics);
    let rev = st.indexed_revision.clone().unwrap();
    assert_eq!(st.schema_version, Some(3));

    // stale after an edit; the revision only moves on refresh.
    f.write(
        "decisions/c.md",
        "---\nid: kb_c\ntitle: Gamma decision\ntype: decision\n---\n# Gamma decision\n\nGamma 2.\n",
    );
    assert_eq!(inspect_index(&f.db(), &scope, true).state, "stale");
    f.rebuild();
    let st = inspect_index(&f.db(), &scope, true);
    assert_eq!(st.state, "fresh");
    assert_ne!(st.indexed_revision.unwrap(), rev);

    // degraded: an indexed parse error.
    f.write("context/broken.md", "---\nid: [unclosed\n---\n# Broken\n");
    f.rebuild();
    assert_eq!(inspect_index(&f.db(), &scope, true).state, "degraded");
    fs::remove_file(f.scaffold.join("context/broken.md")).unwrap();
    f.rebuild();

    // The dump is deterministic: a rebuild of the same corpus dumps to the same bytes.
    let d1 = dump_index(WikiIndex::open_read_only(&f.db()).unwrap().connection()).unwrap();
    f.rebuild();
    let d2 = dump_index(WikiIndex::open_read_only(&f.db()).unwrap().connection()).unwrap();
    assert_eq!(d1, d2);
    assert!(d1.lines().any(|l| l.starts_with("wiki_entities\t")));
    assert!(!d1.contains("last_refresh"));

    // Doctor: consistent, then a drifted row shows up in the diff.
    let report = knobyte::wiki::maintenance::doctor(&f.db(), &scope);
    assert!(report.quick_check.is_empty());
    assert!(
        report.diff.as_ref().unwrap().consistent,
        "{:#?}",
        report.diff
    );
    let c = rusqlite::Connection::open(f.db()).unwrap();
    c.execute(
        "UPDATE wiki_entities SET title = 'Tampered' WHERE id = 'kb_a'",
        [],
    )
    .unwrap();
    drop(c);
    let (code, v) = f.json(&["wiki", "index", "doctor", "--json"]);
    assert_eq!(code, 0, "{}", v);
    assert_eq!(v["data"]["diff"]["consistent"], false);
    assert!(v["data"]["diff"]["onlyInIndex"][0]
        .as_str()
        .unwrap()
        .contains("Tampered"));
    // The doctor's scratch rebuild is gone.
    assert!(fs::read_dir(&f.scaffold).unwrap().all(|e| !e
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("doctor")));

    // rebuild_required and corrupt.
    let c = rusqlite::Connection::open(f.db()).unwrap();
    c.execute(
        "UPDATE wiki_meta SET value = '1' WHERE key = 'schema_version'",
        [],
    )
    .unwrap();
    drop(c);
    assert_eq!(
        inspect_index(&f.db(), &scope, true).state,
        "rebuild_required"
    );
    fs::write(
        f.db(),
        b"this is not a database at all, not even close......",
    )
    .unwrap();
    let _ = fs::remove_file(f.scaffold.join("wiki.db-wal"));
    let _ = fs::remove_file(f.scaffold.join("wiki.db-shm"));
    assert_eq!(inspect_index(&f.db(), &scope, true).state, "corrupt");
    let (code, v) = f.json(&["wiki", "index", "status", "--json"]);
    assert_eq!(code, 3, "{}", v);
}

#[test]
fn maintenance_lease_abort_and_corpus_assertion() {
    let f = Fixture::new();
    corpus(&f);
    f.rebuild();

    // A held lease makes another maintainer fail with WIKI_INDEX_BUSY.
    let lease = acquire_lease(&f.db(), LEASE_WAIT).unwrap();
    let start = std::time::Instant::now();
    let err =
        knobyte::wiki::maintenance::acquire_lease(&f.db(), std::time::Duration::from_millis(100))
            .err()
            .unwrap();
    assert_eq!(error_code(&err), Some("WIKI_INDEX_BUSY"));
    assert!(start.elapsed() < std::time::Duration::from_secs(3));
    drop(lease);
    assert!(acquire_lease(&f.db(), LEASE_WAIT).is_ok());

    // An aborted rebuild publishes nothing.
    let before = dump_index(WikiIndex::open_read_only(&f.db()).unwrap().connection()).unwrap();
    f.write(
        "context/new.md",
        "---\nid: kb_new\ntitle: New\ntype: fact\n---\n# New\n",
    );
    let flag = Arc::new(AtomicBool::new(true));
    let mut idx = WikiIndex::open(&f.db()).unwrap();
    let err = idx
        .refresh_with(&f.scaffold, true, &MaintenanceContext::with_abort(flag))
        .err()
        .unwrap();
    assert_eq!(error_code(&err), Some("OPERATION_INTERRUPTED"));
    drop(idx);
    let after = dump_index(WikiIndex::open_read_only(&f.db()).unwrap().connection()).unwrap();
    assert_eq!(before, after);

    // A Markdown edit while maintenance runs aborts the publish.
    let scaffold = f.scaffold.clone();
    let ctx = MaintenanceContext {
        abort: None,
        progress: Some(Arc::new(move |phase: &str, _: usize, _: usize| {
            if phase == "validate" {
                fs::write(
                    scaffold.join("context/new.md"),
                    "---\nid: kb_new\ntitle: New, edited mid-run\ntype: fact\n---\n# New\n\nLonger body.\n",
                )
                .unwrap();
            }
        })),
    };
    let mut idx = WikiIndex::open(&f.db()).unwrap();
    let err = idx.refresh_with(&f.scaffold, false, &ctx).err().unwrap();
    assert_eq!(error_code(&err), Some("OPERATION_INTERRUPTED"));
    assert!(
        idx.summary("kb_new").unwrap().is_none(),
        "nothing was published"
    );
    let mut seen = Vec::new();
    let phases = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let p2 = phases.clone();
    let ctx = MaintenanceContext {
        abort: None,
        progress: Some(Arc::new(move |phase: &str, _: usize, _: usize| {
            p2.lock().unwrap().push(phase.to_string());
        })),
    };
    idx.refresh_with(&f.scaffold, false, &ctx).unwrap();
    seen.extend(phases.lock().unwrap().iter().cloned());
    seen.dedup();
    assert_eq!(
        seen,
        vec!["discover", "stage", "parse", "resolve", "validate", "publish"]
    );
    assert!(idx.summary("kb_new").unwrap().is_some());
}

#[test]
fn read_session_is_snapshot_bound_with_revision_bound_cursors() {
    let f = Fixture::new();
    corpus(&f);
    for i in 0..6 {
        f.write(
            &format!("context/n{}.md", i),
            &format!(
                "---\nid: kb_n{}\ntitle: Note {}\ntype: fact\n---\n# Note {}\n\nshared word.\n",
                i, i, i
            ),
        );
    }
    f.rebuild();
    let scope = f.scope();
    let s = open_read_session(&f.db(), &scope).unwrap();
    assert_eq!(s.status().state, "fresh");

    // Paged search with a cursor.
    let mut req = SearchRequest {
        query: "shared".into(),
        list: ListRequest {
            limit: Some(4),
            ..Default::default()
        },
    };
    let p1 = s.search(&req).unwrap();
    assert_eq!(p1.items.len(), 4);
    assert!(p1.truncated);
    let cursor = p1.next_cursor.clone().unwrap();
    req.list.cursor = Some(cursor.clone());
    let p2 = s.search(&req).unwrap();
    assert_eq!(p2.items.len(), 2);
    assert!(p2.next_cursor.is_none());

    // A cursor from another request is invalid.
    let other = SearchRequest {
        query: "other".into(),
        list: ListRequest {
            limit: Some(4),
            cursor: Some(cursor.clone()),
            ..Default::default()
        },
    };
    assert_eq!(s.search(&other).err().unwrap().code, "INVALID_REQUEST");
    let garbage = SearchRequest {
        query: "shared".into(),
        list: ListRequest {
            limit: Some(4),
            cursor: Some("not*a*cursor".into()),
            ..Default::default()
        },
    };
    assert_eq!(s.search(&garbage).err().unwrap().code, "INVALID_REQUEST");

    // The session keeps its snapshot while the index is rebuilt underneath.
    f.write(
        "context/n0.md",
        "---\nid: kb_n0\ntitle: Renamed\ntype: fact\n---\n# Renamed\n\nshared word.\n",
    );
    f.rebuild();
    assert_eq!(s.index().summary("kb_n0").unwrap().unwrap().title, "Note 0");
    drop(s);

    // A cursor issued before the change is refused.
    let s2 = open_read_session(&f.db(), &scope).unwrap();
    assert_eq!(
        s2.index().summary("kb_n0").unwrap().unwrap().title,
        "Renamed"
    );
    assert_eq!(s2.search(&req).err().unwrap().code, "REVISION_CONFLICT");

    // Relations with direction and type filters.
    let out = s2
        .relations(&RelationRequest {
            entity_id: "kb_b".into(),
            direction: Direction::Outgoing,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0].relation.target_id, "kb_c");
    let inc = s2
        .relations(&RelationRequest {
            entity_id: "kb_b".into(),
            direction: Direction::Incoming,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(inc.items[0].relation.source_id, "kb_a");
    let none = s2
        .relations(&RelationRequest {
            entity_id: "kb_b".into(),
            direction: Direction::Both,
            relation_types: vec!["supersedes".into()],
            ..Default::default()
        })
        .unwrap();
    assert!(none.items.is_empty());
    assert_eq!(
        s2.relations(&RelationRequest {
            entity_id: "kb_b".into(),
            relation_types: vec!["likes".into()],
            ..Default::default()
        })
        .err()
        .unwrap()
        .code,
        "INVALID_REQUEST"
    );

    // Neighborhood: outgoing only reaches downstream; bounds report truncation.
    let n = s2
        .neighborhood(&NeighborhoodRequest {
            entity_id: "kb_a".into(),
            direction: Direction::Outgoing,
            depth: Some(2),
            ..Default::default()
        })
        .unwrap();
    let ids: Vec<&str> = n.entities.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, vec!["kb_b", "kb_c"]);
    assert_eq!(n.relations.len(), 2);
    let n = s2
        .neighborhood(&NeighborhoodRequest {
            entity_id: "kb_c".into(),
            direction: Direction::Outgoing,
            ..Default::default()
        })
        .unwrap();
    assert!(n.entities.is_empty());
    let n = s2
        .neighborhood(&NeighborhoodRequest {
            entity_id: "kb_a".into(),
            max_entities: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(n.entities.len(), 1);
    assert!(n.truncated);
    assert_eq!(
        s2.neighborhood(&NeighborhoodRequest {
            entity_id: "kb_a".into(),
            depth: Some(9),
            ..Default::default()
        })
        .err()
        .unwrap()
        .code,
        "INVALID_REQUEST"
    );
    assert_eq!(s2.get("kb_zz").err().unwrap().code, "ENTITY_NOT_FOUND");
}

#[test]
fn session_diagnostics_by_id_and_path_with_positions() {
    let f = Fixture::new();
    f.write(
        "context/x.md",
        "---\nid: kb_x\ntitle: X\ntype: fact\nstatus: accepted\n---\n# X\n",
    );
    f.write(
        "notes/y.md",
        "---\nid: kb_y\ntitle: Y\ntype: nonsense\n---\n# Y\n",
    );
    f.rebuild();
    let scope = f.scope();
    let s = open_read_session(&f.db(), &scope).unwrap();
    let by_id = s
        .diagnostics(&knobyte::wiki::session::DiagnosticRequest {
            entity_ids: vec!["kb_x".into()],
            ..Default::default()
        })
        .unwrap();
    assert!(by_id
        .items
        .iter()
        .all(|d| d.entity_id.as_deref() == Some("kb_x")));
    let legacy = by_id
        .items
        .iter()
        .find(|d| d.code == "LEGACY_LIFECYCLE_STATE")
        .unwrap();
    let loc = legacy.location.as_ref().unwrap();
    assert_eq!((loc.start_line, loc.start_column), (5, 1));
    let by_path = s
        .diagnostics(&knobyte::wiki::session::DiagnosticRequest {
            paths: vec!["notes".into()],
            ..Default::default()
        })
        .unwrap();
    assert!(!by_path.items.is_empty());
    assert!(by_path.items.iter().all(|d| d.file == "notes/y.md"));
    assert!(by_path
        .items
        .iter()
        .any(|d| d.code == "INVALID_ENTITY_TYPE"));
    let v = s.validate();
    assert!(!v.valid);
    assert!(v.errors >= 1);
    // A legacy status outranks the stored parse error in the reported state.
    assert_eq!(v.status.state, "migration_required");
}
