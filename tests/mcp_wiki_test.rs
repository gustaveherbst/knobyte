//! MCP wiki contract tools: get, search (bounded, paged, revision-bound cursors),
//! neighborhood, validate, grounding status, and the plan/apply pair with signed handles.

use std::fs;
use std::path::PathBuf;

use knobyte::config::KnobyteConfig;
use knobyte::mcp::protocol::CallToolResult;
use knobyte::mcp::tools::{execute_tool_with_config, get_tools_list};
use knobyte::wiki::index::WikiIndex;
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};

struct Fixture {
    _dir: TempDir,
    scaffold: PathBuf,
    config: KnobyteConfig,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let scaffold = root.join(".knobyte");
        fs::create_dir_all(scaffold.join("context")).unwrap();
        let config = KnobyteConfig::new(root, scaffold.clone());
        let f = Self {
            _dir: dir,
            scaffold,
            config,
        };
        f.write(
            "context/a.md",
            "---\nid: kb_a\ntitle: Alpha\ntype: architecture\ndepends_on: [kb_b]\ngrounds_to:\n  - function:src/a.rs:run\n---\n# Alpha\n\nAlpha body.\n",
        );
        f.write(
            "context/b.md",
            "---\nid: kb_b\ntitle: Beta\ntype: component\nimplements: [kb_c]\n---\n# Beta\n\nBeta body.\n",
        );
        f.write(
            "decisions/c.md",
            "---\nid: kb_c\ntitle: Gamma\ntype: decision\n---\n# Gamma\n\nGamma body.\n",
        );
        for i in 0..5 {
            f.write(
                &format!("context/n{}.md", i),
                &format!(
                    "---\nid: kb_n{}\ntitle: Note {}\ntype: fact\n---\n# Note {}\n\nwidget note.\n",
                    i, i, i
                ),
            );
        }
        f.rebuild();
        f
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
        let mut idx = WikiIndex::open_for_rebuild(&self.config.wiki_db_path()).unwrap();
        idx.rebuild(&self.scaffold).unwrap();
    }

    fn call(&self, name: &str, args: Value) -> (CallToolResult, Value) {
        let r = execute_tool_with_config(name, &args, &self.config);
        let v: Value = serde_json::from_str(&r.content[0].text).unwrap();
        assert_eq!(v["schemaVersion"], 1, "{}", v);
        (r, v)
    }

    fn ok(&self, name: &str, args: Value) -> Value {
        let (r, v) = self.call(name, args);
        assert!(r.is_error != Some(true), "{}: {}", name, v);
        v
    }
}

#[test]
fn contract_tools_are_listed() {
    let tools = get_tools_list();
    for name in [
        "knobyte_wiki_get",
        "knobyte_wiki_search",
        "knobyte_wiki_neighborhood",
        "knobyte_wiki_validate",
        "knobyte_wiki_grounding_status",
        "knobyte_wiki_plan_operation",
        "knobyte_wiki_apply_operation",
        "knobyte_wiki_query",
        "knobyte_wiki_show",
        "knobyte_wiki_list",
    ] {
        assert!(tools.iter().any(|t| t.name == name), "missing {}", name);
    }
    let apply = tools
        .iter()
        .find(|t| t.name == "knobyte_wiki_apply_operation")
        .unwrap();
    assert_eq!(apply.input_schema["required"], json!(["handle"]));
}

#[test]
fn get_search_neighborhood_and_grounding_status() {
    let f = Fixture::new();
    let v = f.ok("knobyte_wiki_get", json!({ "id": "kb_a" }));
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["id"], "kb_a");
    assert_eq!(v["data"]["entityType"], "architecture");
    assert_eq!(v["data"]["index"]["state"], "fresh");
    assert!(v["data"]["index"]["indexedRevision"].is_string());
    assert_eq!(v["data"]["groundings"][0]["ref"], "function:src/a.rs:run");
    assert_eq!(v["data"]["location"]["file"], "context/a.md");

    let (r, v) = f.call("knobyte_wiki_get", json!({ "id": "kb_missing" }));
    assert_eq!(r.is_error, Some(true));
    assert_eq!(v["ok"], false);
    assert_eq!(v["diagnostics"][0]["code"], "ENTITY_NOT_FOUND");

    // Bounded, paged search.
    let p1 = f.ok(
        "knobyte_wiki_search",
        json!({ "query": "widget", "limit": 3 }),
    );
    assert_eq!(p1["data"]["items"].as_array().unwrap().len(), 3);
    assert_eq!(p1["data"]["truncated"], true);
    let cursor = p1["data"]["nextCursor"].as_str().unwrap().to_string();
    let p2 = f.ok(
        "knobyte_wiki_search",
        json!({ "query": "widget", "limit": 3, "cursor": cursor }),
    );
    assert_eq!(p2["data"]["items"].as_array().unwrap().len(), 2);
    assert!(p2["data"]["nextCursor"].is_null());
    let (r, v) = f.call(
        "knobyte_wiki_search",
        json!({ "query": "widget", "limit": 500 }),
    );
    assert_eq!(r.is_error, Some(true));
    assert_eq!(v["diagnostics"][0]["code"], "INVALID_REQUEST");

    // The corpus changes: an old cursor is refused.
    f.write(
        "context/n9.md",
        "---\nid: kb_n9\ntitle: Note 9\ntype: fact\n---\n# Note 9\n\nwidget.\n",
    );
    let (r, v) = f.call(
        "knobyte_wiki_search",
        json!({ "query": "widget", "limit": 3, "cursor": cursor }),
    );
    assert_eq!(r.is_error, Some(true));
    assert_eq!(v["diagnostics"][0]["code"], "REVISION_CONFLICT");
    // ... and the state says why.
    let v = f.ok("knobyte_wiki_search", json!({ "query": "widget" }));
    assert_eq!(v["data"]["index"]["state"], "stale");
    assert!(v["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["code"] == "INDEX_REFRESH_REQUIRED"));

    let v = f.ok(
        "knobyte_wiki_neighborhood",
        json!({ "id": "kb_c", "direction": "incoming", "depth": 2, "relationTypes": ["implements", "depends_on"] }),
    );
    let ids: Vec<&str> = v["data"]["entities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["kb_b", "kb_a"]);
    let v = f.ok(
        "knobyte_wiki_neighborhood",
        json!({ "id": "kb_c", "direction": "incoming", "relationTypes": "depends_on" }),
    );
    assert!(v["data"]["entities"].as_array().unwrap().is_empty());
    let v = f.ok(
        "knobyte_wiki_neighborhood",
        json!({ "id": "kb_a", "maxTokens": 64 }),
    );
    assert_eq!(v["data"]["truncated"], true);

    let v = f.ok("knobyte_wiki_grounding_status", json!({ "id": "kb_a" }));
    let g = &v["data"]["groundings"][0];
    assert_eq!(g["origin"], "frontmatter");
    assert!(g["health"].is_string());
}

#[test]
fn validate_tool_filters_and_locates() {
    let f = Fixture::new();
    f.write(
        "context/bad.md",
        "---\nid: kb_bad\ntitle: Bad\ntype: fact\ndepends_on: [kb_nowhere]\n---\n# Bad\n",
    );
    let v = f.ok("knobyte_wiki_validate", json!({ "entityIds": ["kb_bad"] }));
    assert_eq!(v["data"]["valid"], false);
    assert_eq!(
        v["ok"], false,
        "error findings make ok false; the call itself succeeded"
    );
    let diags = v["diagnostics"].as_array().unwrap();
    assert!(diags.iter().all(|d| d["entityId"] == "kb_bad"));
    let target = diags
        .iter()
        .find(|d| d["code"] == "INVALID_RELATION_TARGET")
        .unwrap();
    assert_eq!(target["location"]["startLine"], 5);
    assert!(target["remediation"].is_string());
    let v = f.ok("knobyte_wiki_validate", json!({ "paths": ["decisions"] }));
    assert_eq!(v["data"]["valid"], true);
    assert_eq!(v["data"]["index"]["state"], "stale");
}

#[test]
fn plan_then_apply_by_handle_with_preconditions() {
    let f = Fixture::new();
    let op = json!({ "type": "set-property", "entityId": "kb_c", "payload": { "property": "summary", "value": "Why gamma." } });

    let v = f.ok(
        "knobyte_wiki_plan_operation",
        json!({ "operation": op, "sessionId": "s-1" }),
    );
    let handle = v["data"]["handle"].as_str().unwrap().to_string();
    assert!(handle.starts_with("wph1_"));
    assert!(!handle.contains("kb_c") && !handle.contains("decisions"));
    assert!(v["data"]["report"]["dryRun"].as_bool().unwrap());
    assert!(v["data"]["report"]["operations"][0]["changes"][0]["diff"]
        .as_str()
        .unwrap()
        .contains("summary: Why gamma."));
    assert!(
        !f.read("decisions/c.md").contains("Why gamma"),
        "planning writes nothing"
    );

    // A forged handle is refused.
    let (r, v) = f.call(
        "knobyte_wiki_apply_operation",
        json!({ "handle": format!("wph1_{}", "0".repeat(64)) }),
    );
    assert_eq!(r.is_error, Some(true));
    assert_eq!(v["diagnostics"][0]["code"], "PLAN_HANDLE_INVALID");

    let v = f.ok(
        "knobyte_wiki_apply_operation",
        json!({ "handle": handle, "sessionId": "s-1" }),
    );
    assert_eq!(v["ok"], true, "{}", v);
    assert_eq!(v["data"]["indexRefreshed"], true);
    assert!(f.read("decisions/c.md").contains("summary: Why gamma."));
    let log = f.read("events/operations.jsonl");
    let last: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!(last["actor"]["kind"], "agent");
    assert_eq!(last["actor"]["sessionId"], "s-1");
    let v = f.ok("knobyte_wiki_get", json!({ "id": "kb_c" }));
    assert_eq!(v["data"]["summary"], "Why gamma.");

    // Single use.
    let (r, v) = f.call("knobyte_wiki_apply_operation", json!({ "handle": handle }));
    assert_eq!(r.is_error, Some(true));
    assert_eq!(v["diagnostics"][0]["code"], "PLAN_HANDLE_INVALID");

    // The entity moves on between plan and apply: the precondition fails, nothing is written.
    let op = json!({ "type": "set-property", "entityId": "kb_b", "payload": { "property": "summary", "value": "Planned." } });
    let v = f.ok("knobyte_wiki_plan_operation", json!({ "operations": [op] }));
    let handle = v["data"]["handle"].as_str().unwrap().to_string();
    let edited = f
        .read("context/b.md")
        .replace("Beta body.", "Beta body, edited by hand.");
    f.write("context/b.md", &edited);
    let (r, v) = f.call("knobyte_wiki_apply_operation", json!({ "handle": handle }));
    assert_eq!(r.is_error, Some(true), "{}", v);
    assert!(
        v["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "CONTENT_HASH_CONFLICT"),
        "{}",
        v
    );
    assert!(!f.read("context/b.md").contains("Planned."));

    // A plan that does not apply cleanly yields no handle.
    let bad = json!({ "type": "set-property", "entityId": "kb_zz", "payload": { "property": "summary", "value": "x" } });
    let v = f.ok("knobyte_wiki_plan_operation", json!({ "operation": bad }));
    assert!(v["data"]["handle"].is_null());
    assert_eq!(v["ok"], false);
    assert!(v["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["code"] == "ENTITY_NOT_FOUND"));
}

#[test]
fn contract_tools_report_an_unusable_index() {
    let dir = tempdir().unwrap();
    let scaffold = dir.path().join(".knobyte");
    fs::create_dir_all(&scaffold).unwrap();
    let config = KnobyteConfig::new(dir.path().to_path_buf(), scaffold);
    let r = execute_tool_with_config("knobyte_wiki_search", &json!({ "query": "x" }), &config);
    assert_eq!(r.is_error, Some(true));
    let v: Value = serde_json::from_str(&r.content[0].text).unwrap();
    assert_eq!(v["diagnostics"][0]["code"], "WIKI_INDEX_MISSING");
    assert!(
        !config.wiki_db_path().exists(),
        "reads never create the index"
    );
}
