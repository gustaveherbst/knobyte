//! Wiki parity: entity model, multi-entity Markdown, operations, validation, query, views,
//! export and synthesis.

use std::fs;
use std::path::{Path, PathBuf};

use knobyte::wiki::index::{QueryFilter, WikiIndex};
use knobyte::wiki::ops::{apply_operations, read_audit_log, ApplyOptions, ApplyReport, OpActor};
use knobyte::wiki::parser::parse_markdown_file;
use knobyte::wiki::scope::WikiScope;
use knobyte::wiki::validate::validate_path;
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

    fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.scaffold.join(rel)).unwrap()
    }

    fn apply(&self, ops: Value, dry_run: bool) -> ApplyReport {
        let raw = match ops {
            Value::Array(a) => a,
            v => vec![v],
        };
        let scope = WikiScope::load(&self.scaffold);
        apply_operations(
            &raw,
            &ApplyOptions {
                scope: &scope,
                graph_db: Some(&self.scaffold.join("graph.db")),
                dry_run,
                default_actor: OpActor {
                    kind: "human".into(),
                    id: "tester".into(),
                    session_id: None,
                },
            },
        )
    }

    fn codes(&self) -> Vec<(String, String)> {
        validate_path(
            &self.scaffold,
            Some(&self.scaffold.join("graph.db")),
            None,
            None,
        )
        .diagnostics
        .into_iter()
        .map(|d| (d.code, d.entity_id.unwrap_or(d.file)))
        .collect()
    }

    fn index(&self) -> WikiIndex {
        let mut idx = WikiIndex::open(&self.scaffold.join("wiki.db")).unwrap();
        idx.refresh(&self.scaffold).unwrap();
        idx
    }
}

fn has(codes: &[(String, String)], code: &str) -> bool {
    codes.iter().any(|(c, _)| c == code)
}

// ---------------------------------------------------------------------------
// Model + Markdown
// ---------------------------------------------------------------------------

#[test]
fn legacy_files_keep_parsing_with_diagnostics() {
    let p = parse_markdown_file(
        "context/legacy.md",
        "---\nid: kb_legacy\ntitle: Legacy\nstatus: accepted\ntype: weird_type\nrelations:\n  - type: depends_on\n    target_id: kb_db\n---\n# Legacy\n\nBody.\n",
    );
    let e = &p.entities[0].entity;
    assert_eq!(e.id, "kb_legacy");
    assert_eq!(e.status, "promoted");
    assert_eq!(e.entity_type, "weird_type");
    let codes: Vec<&str> = p.diagnostics.iter().map(|d| d.code.as_str()).collect();
    assert!(codes.contains(&"LEGACY_LIFECYCLE_STATE"), "{:?}", codes);
    assert!(codes.contains(&"INVALID_ENTITY_TYPE"), "{:?}", codes);

    // No frontmatter at all: implicit entity from the path, as before.
    let p = parse_markdown_file("patterns/retry.md", "# Retry with backoff\n\nText\n");
    assert_eq!(p.entities[0].entity.id, "kb_retry");
    assert_eq!(p.entities[0].entity.entity_type, "pattern");
    assert_eq!(p.entities[0].entity.title, "Retry with backoff");
}

#[test]
fn model_fields_sources_provenance_topics_and_sdd_shorthand() {
    let p = parse_markdown_file(
        "specs/login.md",
        r#"---
id: kb_req_login
type: requirement
title: Login requirement
status: in_flight
derived_from: kb_spec_auth
verified_by: [kb_ac_login]
constrained_by: kb_constraint_tls
topics: [Security, kb_topic_auth]
sources:
  - type: commit
    ref: abc1234
  - type: url
    ref: https://example.com/rfc
  - "manual:"
provenance:
  created_by: { kind: agent, id: synthesis }
  created_at: "2026-01-01T00:00:00Z"
aliases: [login]
metadata: { owner: auth-team }
---
# Login requirement
"#,
    );
    let e = &p.entities[0].entity;
    let rels: Vec<(&str, &str)> = e
        .relations
        .iter()
        .map(|r| (r.rel_type.as_str(), r.target_id.as_str()))
        .collect();
    assert!(rels.contains(&("derived_from", "kb_spec_auth")));
    assert!(rels.contains(&("verified_by", "kb_ac_login")));
    assert!(rels.contains(&("constrained_by", "kb_constraint_tls")));
    assert_eq!(e.sources.len(), 3);
    assert_eq!(e.provenance.as_ref().unwrap().created_by.kind, "agent");
    assert_eq!(e.topics, vec!["Security", "kb_topic_auth"]);
    assert_eq!(e.aliases, vec!["login"]);
    assert_eq!(e.status, "in_flight");
}

#[test]
fn inline_entities_with_crlf_bom_and_code_fences() {
    let text = "\u{feff}---\r\nid: kb_file\r\n---\r\n# File\r\n\r\nIntro\r\n\r\n```markdown\r\n<!-- kb:entity id=kb_fake -->\r\n## Not real\r\n```\r\n\r\n<!-- kb:entity id=kb_a type=decision -->\r\n## Decision A\r\n\r\nA body\r\n\r\n<!-- kb:entity\r\nid: kb_b\r\ntype: fact\r\n-->\r\n### Fact B\r\n\r\nB body\r\n";
    let p = parse_markdown_file("context/x.md", text);
    let ids: Vec<&str> = p.entities.iter().map(|e| e.entity.id.as_str()).collect();
    assert_eq!(ids, vec!["kb_file", "kb_a", "kb_b"]);
    assert!(p.entities[0].entity.body.contains("```markdown"));
    assert_eq!(p.entities[1].entity.body.trim(), "A body");
    assert_eq!(p.entities[2].entity.heading_depth, 3);
    assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[test]
fn validation_codes_without_an_index() {
    let f = Fixture::new();
    f.write(
        "context/decisions.md",
        r#"---
id: kb_decisions
type: decision
title: Decisions
relations:
  - type: depends_on
    target_id: kb_missing
  - type: bogus_type
    target_id: kb_d1
---
# Decisions

<!-- kb:entity id=kb_d1 type=decision -->
## D1

<!-- kb:entity
id: kb_d2
type: decision
relations:
  - type: contradicts
    target_id: kb_d1
  - type: supersedes
    target_id: kb_d3
  - type: supersedes
    target_id: kb_d3
-->
## D2

<!-- kb:entity
id: kb_d3
type: decision
supersedes: [kb_d2]
topics: [Nowhere]
-->
## D3

<!-- kb:entity id=kb_lonely type=fact -->
## Lonely fact

<!-- kb:entity id=kb_unbound -->
no heading follows
"#,
    );
    f.write(
        "topics/t.md",
        "---\nid: kb_t1\ntype: topic\ntitle: T1\nparent: kb_t2\n---\n# T1\n\n<!-- kb:entity id=kb_t2 type=topic parent=kb_t1 -->\n## T2\n",
    );
    let codes = f.codes();
    for code in [
        "INVALID_RELATION_TARGET",
        "INVALID_RELATION_TYPE",
        "CONTRADICTORY_ACTIVE_DECISIONS",
        "DUPLICATE_RELATION",
        "SUPERSESSION_CYCLE",
        "UNKNOWN_TOPIC",
        "ORPHANED_ENTITY",
        "UNBOUND_ENTITY_METADATA",
        "TOPIC_CYCLE",
    ] {
        assert!(has(&codes, code), "missing {} in {:?}", code, codes);
    }
    assert!(codes.contains(&("ORPHANED_ENTITY".to_string(), "kb_lonely".to_string())));

    // A waiver on the relation silences the contradiction.
    let text = f.read("context/decisions.md").replace(
        "  - type: contradicts\n    target_id: kb_d1\n",
        "  - type: contradicts\n    target_id: kb_d1\n    waived: true\n",
    );
    f.write("context/decisions.md", &text);
    assert!(!has(&f.codes(), "CONTRADICTORY_ACTIVE_DECISIONS"));
}

#[test]
fn source_and_log_diagnostics() {
    let f = Fixture::new();
    f.write(
        "context/s.md",
        "---\nid: kb_s\ntitle: S\nsources:\n  - type: commit\n    ref: not-a-sha\n  - type: file\n    ref: src/missing.rs\n  - type: manual\n  - type: url\n    ref: https://x.dev/a\n  - type: url\n    ref: https://X.dev/a\n---\n# S\n",
    );
    fs::create_dir_all(f.scaffold.join("events")).unwrap();
    fs::write(f.scaffold.join("events/operations.jsonl"), "{not json\n").unwrap();
    let codes = f.codes();
    for code in [
        "INVALID_COMMIT_FORMAT",
        "SOURCE_FILE_MISSING",
        "MALFORMED_SOURCE",
        "DUPLICATE_SOURCE",
        "UNRESOLVED_EXTERNAL_SOURCE",
        "MALFORMED_OPERATION_LOG",
    ] {
        assert!(has(&codes, code), "missing {} in {:?}", code, codes);
    }
}

#[test]
fn grounding_stale_and_missing_against_the_code_graph() {
    use knobyte::graph::grounding::ground_documents;
    use knobyte::graph::GraphEngine;
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("src")).unwrap();
    fs::write(
        f.root.join("src/lib.rs"),
        "pub fn keep() -> u32 { 1 }\npub fn gone() {}\n",
    )
    .unwrap();
    f.write(
        "context/g.md",
        "---\nid: kb_g\ntitle: G\ngrounds_to:\n  - function:src/lib.rs:keep\n  - function:src/lib.rs:gone\n---\n# G\n",
    );
    let mut graph = GraphEngine::open(&f.scaffold.join("graph.db")).unwrap();
    graph.rebuild(&f.root).unwrap();
    ground_documents(graph.connection(), &f.root, &f.scaffold).unwrap();
    assert!(!has(&f.codes(), "GROUNDING_STALE"));

    fs::write(
        f.root.join("src/lib.rs"),
        "pub fn keep() -> u32 { 2 + 40 }\n",
    )
    .unwrap();
    graph.rebuild(&f.root).unwrap();
    let codes = f.codes();
    assert!(has(&codes, "GROUNDING_STALE"), "{:?}", codes);
    assert!(has(&codes, "GROUNDING_MISSING"), "{:?}", codes);

    let idx = f.index();
    let e = idx.show("kb_g").unwrap().unwrap();
    assert_eq!(e.health.as_deref(), Some("missing"));
    let page = idx
        .list_filtered(&QueryFilter {
            health: vec!["missing".into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(page.items.len(), 1);
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

fn op(op_id: &str, ty: &str, entity: Option<&str>, payload: Value) -> Value {
    let mut v = json!({
        "opId": op_id,
        "type": ty,
        "actor": { "kind": "agent", "id": "test" },
        "timestamp": "2026-10-01T00:00:00Z",
        "payload": payload,
    });
    if let Some(e) = entity {
        v["entityId"] = json!(e);
    }
    v
}

#[test]
fn all_eleven_operations_round_trip() {
    let f = Fixture::new();
    f.write(
        "context/arch.md",
        "---\nid: kb_arch\ntitle: Architecture\ntype: architecture\n# a comment that must survive\nstatus: promoted\n---\n# Architecture\n\nOverview.\n",
    );
    f.write("topics/auth.md", "---\nid: kb_topic_auth\ntype: topic\ntitle: Authentication\naliases: [auth]\n---\n# Authentication\n");

    // create-entry (section) + create-entry (new file) in one batch.
    let r = f.apply(
        json!([
            op(
                "o1",
                "create-entry",
                None,
                json!({
                    "file": "context/arch.md", "type": "decision", "title": "Use JWT",
                    "body": "We use JWT.", "status": "promoted", "headingDepth": 2, "id": "kb_jwt",
                    "topics": ["auth"], "sources": [{ "type": "manual", "note": "design review" }]
                })
            ),
            op(
                "o2",
                "create-entry",
                None,
                json!({
                    "file": "context/session.md", "type": "component", "title": "Session store",
                    "body": "Stores sessions.", "id": "kb_session",
                    "relations": [{ "type": "implements", "target": "kb_jwt" }]
                })
            ),
        ]),
        false,
    );
    assert!(r.ok, "{:?}", r.diagnostics);
    let arch = f.read("context/arch.md");
    assert!(arch.contains("# a comment that must survive"));
    assert!(arch.contains("<!-- kb:entity\nid: kb_jwt"));
    assert!(arch.contains("topics:\n- kb_topic_auth"), "{}", arch);
    assert!(f
        .read("context/session.md")
        .starts_with("---\nid: kb_session\n"));

    // update-entry, set-property, add/remove-relation, add/remove-source, set-grounding.
    let r = f.apply(
        json!([
            op(
                "o3",
                "update-entry",
                Some("kb_jwt"),
                json!({ "title": "Use signed JWTs", "body": "Signed tokens only." })
            ),
            op(
                "o4",
                "set-property",
                Some("kb_arch"),
                json!({ "property": "summary", "value": "System overview" })
            ),
            op(
                "o5",
                "add-relation",
                Some("kb_session"),
                json!({ "relation": { "type": "depends_on", "target": "kb_arch" } })
            ),
            op(
                "o6",
                "remove-relation",
                Some("kb_session"),
                json!({ "type": "implements", "target": "kb_jwt" })
            ),
            op(
                "o7",
                "add-source",
                Some("kb_jwt"),
                json!({ "source": { "type": "url", "ref": "https://jwt.io" } })
            ),
            op(
                "o8",
                "remove-source",
                Some("kb_jwt"),
                json!({ "sourceIdentity": "manual||design review" })
            ),
            op(
                "o9",
                "set-grounding",
                Some("kb_session"),
                json!({ "groundsTo": ["function:src/session.rs:store"] })
            ),
        ]),
        false,
    );
    assert!(r.ok, "{:?}", r.diagnostics);
    let arch = f.read("context/arch.md");
    assert!(arch.contains("## Use signed JWTs"));
    assert!(arch.contains("Signed tokens only."));
    assert!(arch.contains("summary: System overview"));
    assert!(arch.contains("# a comment that must survive"));
    let parsed = parse_markdown_file("context/arch.md", &arch);
    let jwt = parsed.entity("kb_jwt").unwrap();
    assert_eq!(jwt.entity.revision, 4);
    assert_eq!(jwt.entity.sources.len(), 1);
    let session = parse_markdown_file("context/session.md", &f.read("context/session.md"));
    let s = &session.entities[0].entity;
    assert_eq!(s.relations.len(), 1);
    assert_eq!(s.relations[0].rel_type, "depends_on");
    assert_eq!(s.grounds_to, vec!["function:src/session.rs:store"]);

    // supersede-entry with an inline replacement, move-entry, archive-entry.
    let r = f.apply(
        json!([
            op("o10", "supersede-entry", Some("kb_jwt"), json!({ "replacement": {
                "file": "context/arch.md", "type": "decision", "title": "Use PASETO",
                "body": "PASETO tokens.", "headingDepth": 2, "id": "kb_paseto", "status": "promoted"
            }})),
            op("o11", "move-entry", Some("kb_paseto"), json!({ "file": "context/tokens.md", "insertAt": { "at": "end-of-file" } })),
            op("o12", "archive-entry", Some("kb_jwt"), json!({})),
        ]),
        false,
    );
    assert!(r.ok, "{:?}", r.diagnostics);
    let tokens = parse_markdown_file("context/tokens.md", &f.read("context/tokens.md"));
    let paseto = tokens.entity("kb_paseto").unwrap();
    assert!(paseto
        .entity
        .relations
        .iter()
        .any(|r| r.rel_type == "supersedes" && r.target_id == "kb_jwt"));
    let arch = parse_markdown_file("context/arch.md", &f.read("context/arch.md"));
    assert!(arch.entity("kb_paseto").is_none());
    assert_eq!(arch.entity("kb_jwt").unwrap().entity.status, "archived");

    // Archived entities are hidden by default but resolvable by id.
    let idx = f.index();
    let visible = idx.list_filtered(&QueryFilter::default()).unwrap();
    assert!(!visible.items.iter().any(|e| e.id == "kb_jwt"));
    let all = idx
        .list_filtered(&QueryFilter {
            include_archived: true,
            ..Default::default()
        })
        .unwrap();
    assert!(all.items.iter().any(|e| e.id == "kb_jwt"));
    assert!(idx.show("kb_jwt").unwrap().is_some());

    // Audit log: intent + complete per operation.
    let (entries, diags) = read_audit_log(&f.scaffold);
    assert!(diags.is_empty());
    assert_eq!(entries.iter().filter(|e| e.phase == "complete").count(), 12);
    assert!(!has(&f.codes(), "WRITE_SCOPE_VIOLATION"));
}

#[test]
fn preconditions_dry_run_replay_and_atomic_batches() {
    let f = Fixture::new();
    f.write(
        "context/a.md",
        "---\nid: kb_a\ntitle: A\nrevision: 2\n---\n# A\n\nText\n",
    );
    let before = f.read("context/a.md");

    // Dry run writes nothing and logs nothing.
    let r = f.apply(
        op(
            "p1",
            "set-property",
            Some("kb_a"),
            json!({ "property": "status", "value": "deprecated" }),
        ),
        true,
    );
    assert!(r.ok && r.dry_run);
    assert!(r.operations[0].changes[0]
        .diff
        .contains("+status: deprecated"));
    assert_eq!(f.read("context/a.md"), before);
    assert!(!f.scaffold.join("events/operations.jsonl").exists());

    // Revision and content-hash preconditions.
    let mut stale = op(
        "p2",
        "set-property",
        Some("kb_a"),
        json!({ "property": "status", "value": "deprecated" }),
    );
    stale["baseRevision"] = json!(1);
    let r = f.apply(stale, false);
    assert!(!r.ok);
    assert_eq!(r.diagnostics[0].code, "REVISION_CONFLICT");
    let mut stale = op(
        "p3",
        "set-property",
        Some("kb_a"),
        json!({ "property": "status", "value": "deprecated" }),
    );
    stale["baseContentHash"] = json!("0".repeat(64));
    assert_eq!(
        f.apply(stale, false).diagnostics[0].code,
        "CONTENT_HASH_CONFLICT"
    );

    // A failing operation aborts the whole batch.
    let r = f.apply(
        json!([
            op(
                "p4",
                "set-property",
                Some("kb_a"),
                json!({ "property": "status", "value": "deprecated" })
            ),
            op(
                "p5",
                "add-relation",
                Some("kb_a"),
                json!({ "relation": { "type": "depends_on", "target": "kb_nope" } })
            ),
        ]),
        false,
    );
    assert!(!r.ok);
    assert_eq!(f.read("context/a.md"), before);

    // Replay idempotency.
    let good = op(
        "p6",
        "set-property",
        Some("kb_a"),
        json!({ "property": "status", "value": "deprecated" }),
    );
    assert!(f.apply(good.clone(), false).ok);
    let after = f.read("context/a.md");
    assert!(after.contains("revision: 3"));
    let replay = f.apply(good, false);
    assert!(replay.ok && replay.operations[0].replayed);
    assert_eq!(f.read("context/a.md"), after);
    let reused = f.apply(
        op(
            "p6",
            "set-property",
            Some("kb_a"),
            json!({ "property": "status", "value": "promoted" }),
        ),
        false,
    );
    assert!(!reused.ok);
    assert_eq!(reused.diagnostics[0].code, "INVALID_OPERATION_ENVELOPE");

    // Unknown operation type, invalid lifecycle, self relation.
    assert_eq!(
        f.apply(op("p7", "rename-entry", Some("kb_a"), json!({})), false)
            .diagnostics[0]
            .code,
        "UNKNOWN_OPERATION_TYPE"
    );
    assert_eq!(
        f.apply(
            op(
                "p8",
                "set-property",
                Some("kb_a"),
                json!({ "property": "status", "value": "stale" })
            ),
            false
        )
        .diagnostics[0]
            .code,
        "INVALID_LIFECYCLE_STATE"
    );
    assert_eq!(
        f.apply(
            op(
                "p9",
                "add-relation",
                Some("kb_a"),
                json!({ "relation": { "type": "related_to", "target": "kb_a" } })
            ),
            false
        )
        .diagnostics[0]
            .code,
        "SELF_RELATION"
    );
}

#[test]
fn read_only_paths_and_inline_marker_rewrites() {
    let f = Fixture::new();
    fs::write(
        f.scaffold.join("config.json"),
        r#"{ "wiki": { "readOnly": ["imported/**"], "exclude": ["drafts/**"] } }"#,
    )
    .unwrap();
    f.write("imported/x.md", "---\nid: kb_x\ntitle: X\n---\n# X\n");
    f.write("drafts/y.md", "---\nid: kb_y\ntitle: Y\n---\n# Y\n");
    f.write("team/z.md", "---\nid: kb_z\ntitle: Z\n---\n# Z\n");
    let r = f.apply(
        op(
            "r1",
            "set-property",
            Some("kb_x"),
            json!({ "property": "summary", "value": "s" }),
        ),
        false,
    );
    assert_eq!(r.diagnostics.last().unwrap().code, "WRITE_SCOPE_VIOLATION");
    let r = f.apply(
        op(
            "r2",
            "set-property",
            Some("kb_z"),
            json!({ "property": "summary", "value": "s" }),
        ),
        false,
    );
    assert_eq!(r.diagnostics.last().unwrap().code, "WRITE_SCOPE_VIOLATION");
    // Excluded paths are not part of the wiki at all.
    let r = f.apply(op("r3", "archive-entry", Some("kb_y"), json!({})), false);
    assert_eq!(r.diagnostics[0].code, "ENTITY_NOT_FOUND");
    let idx = f.index();
    assert!(idx.show("kb_y").unwrap().is_none());
    assert!(idx.show("kb_x").unwrap().is_some());

    // Inline attribute markers are rewritten to block form, leaving neighbours untouched.
    f.write("context/m.md", "# M\n\n<!-- kb:entity id=kb_m1 type=fact -->\n## One\n\nfirst\n\n<!-- kb:entity id=kb_m2 type=fact -->\n## Two\n\nsecond\n");
    let r = f.apply(
        op(
            "r4",
            "set-property",
            Some("kb_m1"),
            json!({ "property": "status", "value": "in_flight" }),
        ),
        false,
    );
    assert!(r.ok, "{:?}", r.diagnostics);
    let text = f.read("context/m.md");
    assert!(
        text.contains(
            "<!-- kb:entity\nid: kb_m1\ntype: fact\nstatus: in_flight\nrevision: 2\n-->\n## One"
        ),
        "{}",
        text
    );
    assert!(text.contains("<!-- kb:entity id=kb_m2 type=fact -->\n## Two"));
}

// ---------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------

#[test]
fn query_ranking_filters_related_graph_and_incremental_refresh() {
    let f = Fixture::new();
    f.write(
        "topics/cache.md",
        "---\nid: kb_topic_cache\ntype: topic\ntitle: Caching\naliases: [cache]\n---\n# Caching\n",
    );
    f.write(
        "context/c.md",
        "---\nid: kb_cache_policy\ntype: decision\ntitle: Cache policy\nsummary: How long entries live.\ntopics: [cache]\n---\n# Cache policy\n\nEviction is LRU.\n\n<!-- kb:entity id=kb_body_hit type=fact status=in_flight depends_on=kb_cache_policy -->\n## Unrelated title\n\nThe cache is warmed at boot.\n\n<!-- kb:entity id=kb_old type=fact status=archived related_to=kb_body_hit -->\n## Old cache note\n",
    );
    let mut idx = WikiIndex::open(&f.scaffold.join("wiki.db")).unwrap();
    let s = idx.refresh(&f.scaffold).unwrap();
    assert_eq!(s.files_added, 2);
    let s = idx.refresh(&f.scaffold).unwrap();
    assert_eq!((s.files_unchanged, s.files_updated), (2, 0));

    let hits = idx.search("cache", &QueryFilter::default()).unwrap().items;
    let order: Vec<&str> = hits.iter().map(|h| h.entity.id.as_str()).collect();
    assert_eq!(order.first(), Some(&"kb_cache_policy"), "{:?}", order);
    assert!(!order.contains(&"kb_old"), "archived hidden: {:?}", order);
    assert_eq!(
        hits.iter()
            .find(|h| h.entity.id == "kb_body_hit")
            .unwrap()
            .matched,
        "body"
    );
    let with_archived = idx
        .search(
            "cache",
            &QueryFilter {
                include_archived: true,
                ..Default::default()
            },
        )
        .unwrap()
        .items;
    assert!(with_archived.iter().any(|h| h.entity.id == "kb_old"));
    let exact = idx
        .search("kb_body_hit", &QueryFilter::default())
        .unwrap()
        .items;
    assert_eq!(exact[0].matched, "id");

    let by_type = idx
        .list_filtered(&QueryFilter {
            types: vec!["decision".into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(by_type.items.len(), 1);
    let by_topic = idx
        .list_filtered(&QueryFilter {
            topic: Some("Caching".into()),
            ..Default::default()
        })
        .unwrap();
    assert!(by_topic.items.iter().any(|e| e.id == "kb_cache_policy"));
    let by_status = idx
        .list_filtered(&QueryFilter {
            statuses: vec!["in_flight".into()],
            ..Default::default()
        })
        .unwrap();
    assert_eq!(by_status.items[0].id, "kb_body_hit");
    let limited = idx
        .list_filtered(&QueryFilter {
            limit: Some(1),
            ..Default::default()
        })
        .unwrap();
    assert!(limited.truncated && limited.items.len() == 1);

    let n = idx
        .neighborhood("kb_cache_policy", Some(2), None, None, false)
        .unwrap()
        .unwrap();
    assert_eq!(n.backlinks.len(), 1);
    assert!(!n.reached.iter().any(|e| e.id == "kb_old"));
    let n = idx
        .neighborhood("kb_cache_policy", Some(2), None, None, true)
        .unwrap()
        .unwrap();
    assert!(n.reached.iter().any(|e| e.id == "kb_old"));
    let tiny = idx
        .neighborhood("kb_cache_policy", Some(2), Some(1), None, true)
        .unwrap()
        .unwrap();
    assert!(tiny.truncated && tiny.reached.is_empty());

    let g = idx
        .graph_slice(&["kb_cache_policy".into()], Some(1), None, false)
        .unwrap();
    assert!(g
        .edges
        .iter()
        .any(|e| e.source == "kb_body_hit" && e.target == "kb_cache_policy"));

    // Incremental: only the touched file is re-read; deletions are dropped.
    f.write("context/c2.md", "---\nid: kb_new\ntitle: New\n---\n# New\n");
    fs::remove_file(f.scaffold.join("topics/cache.md")).unwrap();
    let s = idx.refresh(&f.scaffold).unwrap();
    assert_eq!(
        (s.files_added, s.files_removed, s.files_unchanged),
        (1, 1, 1)
    );
    assert!(idx.show("kb_topic_cache").unwrap().is_none());
}

#[test]
fn for_code_accepts_several_references() {
    let f = Fixture::new();
    f.write(
        "context/a.md",
        "---\nid: kb_a\ntitle: A\ngrounds_to: [function:src/a.rs:one]\n---\n# A\n",
    );
    f.write(
        "context/b.md",
        "---\nid: kb_b\ntitle: B\n---\n# B\n<!-- kb-ground: function:src/b.rs:two -->\n",
    );
    let idx = f.index();
    let page = idx
        .for_code_many(
            &[
                "function:src/a.rs:one".into(),
                "function:src/b.rs:two".into(),
            ],
            Some(10),
        )
        .unwrap();
    let ids: Vec<&str> = page.items.iter().map(|h| h.entity.id.as_str()).collect();
    assert_eq!(ids.len(), 2, "{:?}", ids);
    let one = idx
        .for_code_many(
            &[
                "function:src/a.rs:one".into(),
                "function:src/b.rs:two".into(),
            ],
            Some(1),
        )
        .unwrap();
    assert!(one.truncated);
}

// ---------------------------------------------------------------------------
// Views and export
// ---------------------------------------------------------------------------

#[test]
fn regenerate_views_only_touches_marked_sections() {
    let f = Fixture::new();
    f.write(
        "context/d1.md",
        "---\nid: kb_d1\ntype: decision\ntitle: First decision\n---\n# First decision\n",
    );
    f.write("decisions.md", "# Decisions\n\nHand-written intro.\n\n<!-- kb:generated:begin -->\nstale\n<!-- kb:generated:end -->\n\nOutro.\n");
    f.write(
        "patterns/INDEX.md",
        "# Patterns\n\n```\n<!-- kb:generated:begin -->\n```\n",
    );
    assert!(has(&f.codes(), "GENERATED_VIEW_DRIFT"));
    let scope = WikiScope::load(&f.scaffold);
    let dry = knobyte::wiki::views::regenerate_views(&scope, true);
    assert_eq!(dry.files.len(), 1);
    assert!(f.read("decisions.md").contains("stale"));
    let done = knobyte::wiki::views::regenerate_views(&scope, false);
    assert!(done.files[0].written);
    let text = f.read("decisions.md");
    assert!(text.starts_with("# Decisions\n\nHand-written intro.\n\n<!-- kb:generated:begin -->"));
    assert!(text.contains("| First decision | promoted | `context/d1.md` |"));
    assert!(text.ends_with("<!-- kb:generated:end -->\n\nOutro.\n"));
    assert!(!has(&f.codes(), "GENERATED_VIEW_DRIFT"));
}

#[test]
fn export_bundles_the_scaffold_and_protects_other_files() {
    use knobyte::wiki::export::{export_scaffold, BUNDLE_MARKER};
    let f = Fixture::new();
    f.write("context/a.md", "# A\n");
    f.write("patterns/b.md", "# B\n");
    let r = export_scaffold(&f.root, &f.scaffold, None).unwrap();
    assert!(r.document.starts_with(BUNDLE_MARKER));
    assert!(r.document.contains("## context/a.md\n\n# A\n"));
    assert_eq!(r.files.len(), 2);

    let r = export_scaffold(&f.root, &f.scaffold, Some("out/bundle.md")).unwrap();
    assert!(r.written_to.is_some());
    // A previous bundle may be overwritten; any other existing file may not.
    assert!(export_scaffold(&f.root, &f.scaffold, Some("out/bundle.md")).is_ok());
    fs::write(f.root.join("notes.md"), "mine").unwrap();
    assert!(export_scaffold(&f.root, &f.scaffold, Some("notes.md")).is_err());
    assert!(export_scaffold(&f.root, &f.scaffold, Some(".knobyte/context/a.md")).is_err());
}

// ---------------------------------------------------------------------------
// Synthesis
// ---------------------------------------------------------------------------

fn synthesis_fixture() -> Fixture {
    use knobyte::graph::GraphEngine;
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("src/billing")).unwrap();
    fs::write(
        f.root.join("src/billing/invoice.rs"),
        "/// Builds an invoice.\npub fn build_invoice(total: u32) -> u32 {\n    apply_tax(total)\n}\n\npub fn apply_tax(total: u32) -> u32 {\n    total * 2\n}\n",
    )
    .unwrap();
    fs::write(f.root.join("src/billing/mod.rs"), "pub mod invoice;\n").unwrap();
    fs::write(f.root.join("src/lib.rs"), "pub mod billing;\n").unwrap();
    let mut graph = GraphEngine::open(&f.scaffold.join("graph.db")).unwrap();
    graph.rebuild(&f.root).unwrap();
    f
}

fn write_json(dir: &Path, name: &str, v: &Value) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, serde_json::to_string(v).unwrap()).unwrap();
    p
}

#[test]
fn synthesis_prepare_propose_and_apply() {
    use knobyte::wiki::synthesis::{clusters, prepare, propose, render_playbook, SynthesisEnv};
    let f = synthesis_fixture();
    let scope = WikiScope::load(&f.scaffold);
    let graph_db = f.scaffold.join("graph.db");
    let env = SynthesisEnv {
        scope: &scope,
        project_root: &f.root,
        graph_db: &graph_db,
    };
    let found = clusters(&env);
    assert_eq!(
        found.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        vec!["billing"]
    );
    assert!(
        render_playbook(&f.root, &f.scaffold, &["billing".into()], None)
            .contains("knobyte wiki synthesis prepare")
    );

    let prepared = prepare(&env, "architecture_component", Some("billing")).unwrap();
    let user = prepared["prompt"]["user"].as_str().unwrap();
    assert!(
        user.contains("function:src/billing/invoice.rs:build_invoice"),
        "{}",
        user
    );
    assert!(prepare(&env, "pattern", None).is_err());

    let response = json!({
        "stage": "architecture_component",
        "cluster": "billing",
        "units": [
            { "type": "component", "title": "Invoice builder", "summary": "Builds invoices and applies tax.",
              "body": "build_invoice delegates tax calculation to apply_tax.", "confidence": 0.9,
              "grounding": { "nodeIds": ["function:src/billing/invoice.rs:build_invoice"] } },
            { "type": "component", "title": "Tax helper", "summary": "Doubles the total as tax.",
              "body": "apply_tax multiplies the total by two for tax.", "confidence": 0.5,
              "grounding": { "nodeIds": ["function:src/billing/invoice.rs:apply_tax"] } },
            { "type": "component", "title": "Weak guess", "summary": "Probably something else.",
              "body": "Speculative unit with little evidence here.", "confidence": 0.2,
              "grounding": { "nodeIds": ["function:src/billing/invoice.rs:apply_tax"] } },
            { "type": "pattern", "title": "Wrong stage", "summary": "Patterns belong to another stage.",
              "body": "This unit has a type the stage does not allow.", "confidence": 0.9,
              "grounding": { "nodeIds": ["function:src/billing/invoice.rs:apply_tax"] } },
            { "type": "component", "title": "Invented", "summary": "Grounded in a node that is not there.",
              "body": "This references a symbol outside the cluster context.", "confidence": 0.9,
              "grounding": { "nodeIds": ["function:src/nowhere.rs:ghost"] } }
        ]
    });
    let proposal = propose(&env, &response, None).unwrap();
    assert_eq!(proposal.accepted, 2);
    assert_eq!(proposal.rejected.len(), 3);
    let statuses: Vec<&str> = proposal
        .operations
        .iter()
        .map(|o| o["payload"]["status"].as_str().unwrap())
        .collect();
    assert_eq!(statuses, vec!["promoted", "in_flight"]);

    // Nothing is written until applied.
    assert!(!f.scaffold.join("context/architecture.md").exists());
    let path = write_json(&f.root, "resp.json", &response);
    let _ = path;
    let r = f.apply(Value::Array(proposal.operations.clone()), false);
    assert!(r.ok, "{:?}", r.diagnostics);
    let arch = parse_markdown_file(
        "context/architecture.md",
        &f.read("context/architecture.md"),
    );
    let builder = arch
        .entities
        .iter()
        .find(|e| e.entity.title == "Invoice builder")
        .unwrap();
    assert_eq!(builder.entity.status, "promoted");
    assert_eq!(
        builder.entity.grounds_to,
        vec!["function:src/billing/invoice.rs:build_invoice"]
    );
    // Re-proposing the same response replays as a no-op.
    let again = f.apply(
        Value::Array(propose(&env, &response, None).unwrap().operations),
        false,
    );
    assert!(again.operations.iter().all(|o| o.replayed));

    // Relationships: the builder calls the tax helper, so the pair is a candidate.
    let prepared = prepare(&env, "relationships", None).unwrap();
    let candidates = prepared["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1, "{}", prepared);
    let c = &candidates[0];
    assert!(c["allowedTypes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t == "depends_on"));
    let judgment = json!({ "stage": "relationships", "judgments": [{
        "candidateId": c["candidateId"], "action": "create", "type": "depends_on",
        "sourceId": c["source"]["id"], "targetId": c["target"]["id"], "confidence": 0.85,
        "evidence": "build_invoice calls apply_tax"
    }]});
    let rel = propose(&env, &judgment, None).unwrap();
    assert_eq!(rel.operations.len(), 1);
    assert!(f.apply(Value::Array(rel.operations), false).ok);

    // Global pass groups near-duplicates of one type.
    let r = f.apply(
        op("g1", "create-entry", None, json!({
            "file": "context/architecture.md", "type": "component", "title": "Invoice builder service",
            "summary": "Builds invoices and applies tax.", "body": "Duplicate description of the invoice builder.",
            "headingDepth": 2, "groundsTo": ["function:src/billing/invoice.rs:build_invoice"]
        })),
        false,
    );
    assert!(r.ok, "{:?}", r.diagnostics);
    let prepared = prepare(&env, "global", None).unwrap();
    let groups = prepared["groups"].as_array().unwrap();
    assert!(!groups.is_empty(), "{}", prepared);
    let g = &groups[0];
    let ids: Vec<String> = g["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["id"].as_str().unwrap().to_string())
        .collect();
    let action = json!({ "stage": "global", "actions": [{
        "groupId": g["groupId"], "action": "promote_one", "winnerId": ids[0],
        "reasoning": "The first entity is the clearer description."
    }]});
    let plan = propose(&env, &action, None).unwrap();
    assert!(plan.operations.iter().any(|o| o["type"] == "add-relation"));
    let applied = f.apply(Value::Array(plan.operations), false);
    assert!(applied.ok, "{:?}", applied.diagnostics);
}

#[test]
fn revision_divergence_adoption_and_traceability() {
    let f = Fixture::new();
    f.write(
        "specs/auth.md",
        "---\nid: kb_spec\ntype: spec\ntitle: Auth spec\n---\n# Auth spec\n",
    );
    f.write(
        "context/chain.md",
        "# Chain\n\n<!-- kb:entity id=kb_req type=requirement derived_from=kb_spec -->\n## Requirement\n\n<!-- kb:entity id=kb_dec type=decision implements=kb_req -->\n## Decision\n\n<!-- kb:entity id=kb_comp type=component implements=kb_dec -->\n## Component\n\n<!-- kb:entity id=kb_ac type=acceptance_criterion -->\n## Criterion\n\n## Unmarked section\n\nProse.\n",
    );
    let idx = f.index();
    // Hand edit without a revision bump.
    let text = f
        .read("specs/auth.md")
        .replace("# Auth spec\n", "# Auth spec\n\nEdited by hand.\n");
    f.write("specs/auth.md", &text);
    let report = validate_path(&f.scaffold, None, Some(&idx), None);
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code == "REVISION_DIVERGED"),
        "{:?}",
        report.diagnostics
    );

    // Adopt existing prose as an entity without rewriting it.
    let r = f.apply(
        op("a1", "create-entry", None, json!({
            "file": "context/chain.md", "type": "fact", "title": "Unmarked section", "id": "kb_adopted",
            "adopt": { "at": "heading", "ordinal": 5, "text": "Unmarked section" }
        })),
        false,
    );
    assert!(r.ok, "{:?}", r.diagnostics);
    let chain = f.read("context/chain.md");
    assert!(
        chain.contains("-->\n## Unmarked section\n\nProse.\n"),
        "{}",
        chain
    );
    let wrong = f.apply(
        op(
            "a2",
            "create-entry",
            None,
            json!({
                "file": "context/chain.md", "type": "fact", "title": "X",
                "adopt": { "at": "heading", "ordinal": 1, "text": "Not the heading" }
            }),
        ),
        false,
    );
    assert_eq!(wrong.diagnostics[0].code, "AMBIGUOUS_MIGRATION");

    let idx = f.index();
    let t = knobyte::wiki::trace::trace(&idx, "kb_dec", &f.scaffold.join("graph.db"))
        .unwrap()
        .unwrap();
    for ty in ["spec", "requirement", "decision", "component"] {
        assert!(t.nodes.contains_key(ty), "{:?}", t.nodes.keys());
    }
    assert!(t
        .gaps
        .iter()
        .any(|g| g.entity_id == "kb_comp" && g.hop.contains("implementation")));
}

#[test]
fn cli_apply_query_validate_and_export() {
    use std::process::Command;
    let f = Fixture::new();
    f.write(
        "context/a.md",
        "---\nid: kb_a\ntitle: Alpha\ntype: architecture\n---\n# Alpha\n\nAlpha body.\n",
    );
    let ops = json!([op(
        "c1",
        "create-entry",
        None,
        json!({
            "file": "context/a.md", "type": "decision", "title": "Beta decision", "body": "Beta.",
            "headingDepth": 2, "id": "kb_beta", "status": "promoted"
        })
    )]);
    write_json(&f.root, "ops.json", &ops);
    // CLI commands require a complete scaffold (ROUTER.md marks a finished setup).
    f.write("ROUTER.md", "# Router\n");
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_knobyte"))
            .args(args)
            .current_dir(&f.root)
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    };
    let (ok, out) = run(&["wiki", "apply", "ops.json", "--dry-run", "--json"]);
    assert!(ok, "{}", out);
    assert!(!f.read("context/a.md").contains("Beta"));
    let (ok, out) = run(&["wiki", "apply", "ops.json"]);
    assert!(ok, "{}", out);
    assert!(f.read("context/a.md").contains("## Beta decision"));
    let (ok, out) = run(&["wiki", "list", "--type", "decision", "--jsonl"]);
    assert!(ok);
    assert_eq!(out.lines().count(), 1, "{}", out);
    assert!(out.contains("kb_beta"));
    let (ok, out) = run(&["wiki", "show", "kb_beta", "--no-body", "--json"]);
    assert!(ok);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["schemaVersion"], 1);
    assert_eq!(v["ok"], true);
    assert!(v["data"].get("body").is_none());
    assert_eq!(v["data"]["entityType"], "decision");
    let (ok, out) = run(&["wiki", "validate", "--json"]);
    assert!(ok);
    let v: Value = serde_json::from_str(&out).unwrap();
    // kb_a, kb_beta and the ROUTER.md document.
    assert_eq!(v["data"]["entitiesChecked"], 3);
    assert!(v["diagnostics"].is_array());
    let (ok, _) = run(&["export", "--out", "bundle.md"]);
    assert!(ok);
    assert!(fs::read_to_string(f.root.join("bundle.md"))
        .unwrap()
        .contains("## context/a.md"));
}

/// `wiki apply --dry-run` shows only the frontmatter lines an operation changes (a unified
/// diff with context), never the whole block removed and re-added.
#[test]
fn dry_run_diff_is_minimal_for_frontmatter_edits() {
    let f = Fixture::new();
    f.write("context/b.md", "---\nid: kb_b\ntitle: B\ntype: architecture\n---\n# B\n");
    let before = "---\nid: kb_a\ntype: architecture\nstatus: promoted\nrevision: 3\ntitle: Alpha\nsummary: The alpha component\ntags:\n  - core\n  - storage\nowner: sam\nrelations:\n  - type: implements\n    target_id: kb_b\n---\n# Alpha\n\nBody.\n";
    f.write("context/a.md", before);
    let r = f.apply(
        op(
            "d1",
            "add-relation",
            Some("kb_a"),
            json!({ "relation": { "type": "depends_on", "target": "kb_b" } }),
        ),
        true,
    );
    assert!(r.ok, "{:?}", r.diagnostics);
    assert_eq!(f.read("context/a.md"), before, "dry run must not write");
    let diff = &r.operations[0].changes[0].diff;
    let removed: Vec<&str> = diff.lines().filter(|l| l.starts_with('-')).collect();
    let added: Vec<&str> = diff.lines().filter(|l| l.starts_with('+')).collect();
    assert_eq!(removed, vec!["-revision: 3"], "{}", diff);
    assert!(added.contains(&"+revision: 4"), "{}", diff);
    assert!(added.iter().any(|l| l.contains("depends_on")), "{}", diff);
    assert!(!added.iter().any(|l| l.contains("title") || l.contains("summary") || l.contains("core")), "{}", diff);
    assert!(diff.contains("\n title: Alpha\n"), "context lines are kept: {}", diff);
}
