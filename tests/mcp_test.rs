use std::fs;
use std::path::Path;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use knobyte::config::KnobyteConfig;
use knobyte::mcp::handler::process_jsonrpc_request_with_config;
use knobyte::mcp::protocol::{CallToolResult, JsonRpcRequest};
use knobyte::mcp::sse::{build_router, process_jsonrpc_request, resolve_auth_token, ServerSecurity};
use knobyte::mcp::stdio::run_stdio;
use knobyte::mcp::tools::{execute_tool_with_config, get_tools_list};

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn temp_project() -> (tempfile::TempDir, KnobyteConfig) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let scaffold = root.join(".knobyte");
    fs::create_dir_all(scaffold.join("context")).unwrap();
    fs::write(scaffold.join("AGENTS.md"), "# Agents\n").unwrap();
    fs::write(scaffold.join("context/stack.md"), "# Stack\n").unwrap();
    let config = KnobyteConfig::new(root.clone(), scaffold);
    (dir, config)
}

fn req(id: Option<Value>, method: &str, params: Option<Value>) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id,
        method: method.to_string(),
        params,
    }
}

fn call(config: &KnobyteConfig, name: &str, args: Value) -> CallToolResult {
    execute_tool_with_config(name, &args, config)
}

fn is_err(r: &CallToolResult) -> bool {
    r.is_error == Some(true)
}

fn loopback_router(config: KnobyteConfig) -> Router {
    build_router(config, ServerSecurity { token: None, loopback_bind: true })
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn post_json(uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
}

// ---------------------------------------------------------------------------
// tool list / dispatch basics
// ---------------------------------------------------------------------------

#[test]
fn test_mcp_tools_list() {
    let tools = get_tools_list();
    for name in [
        "knobyte_check",
        "knobyte_log",
        "knobyte_timeline",
        "knobyte_graph_query",
        "knobyte_wiki_query",
        "knobyte_vector_search",
        "knobyte_cozo_datalog",
        "knobyte_cozo_pagerank",
        "knobyte_cozo_shortest_path",
        "knobyte_sync_groundings",
        "knobyte_session_start",
        "knobyte_workstream_step_update",
        "knobyte_file_context",
        "knobyte_harvest",
        "knobyte_playbook_list",
        "knobyte_playbook_get",
        "knobyte_playbook_complete_step",
        "knobyte_catch_up",
        "knobyte_catch_up_mark",
    ] {
        assert!(tools.iter().any(|t| t.name == name), "missing tool {}", name);
    }
    assert_eq!(tools.len(), 38);

    // Unique names, non-empty descriptions, object schemas, no projectRoot override.
    let mut names: Vec<_> = tools.iter().map(|t| t.name.clone()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), tools.len());
    for t in &tools {
        assert!(!t.description.is_empty());
        assert_eq!(t.input_schema["type"], "object");
        assert!(t.input_schema["properties"].get("projectRoot").is_none(), "{}", t.name);
        assert!(!t.name.contains("approve") && !t.name.contains("reject") && !t.name.contains("publish"));
    }

    let check = tools.iter().find(|t| t.name == "knobyte_check").unwrap();
    assert!(check.input_schema["properties"].get("fix").is_some());
    let datalog = tools.iter().find(|t| t.name == "knobyte_cozo_datalog").unwrap();
    assert!(datalog.description.to_lowercase().contains("read-only"));
}

#[test]
fn test_mcp_jsonrpc_initialize() {
    let resp = process_jsonrpc_request(req(Some(json!(1)), "initialize", None)).unwrap();
    assert_eq!(resp.id, Some(json!(1)));
    assert!(resp.error.is_none());
    let result = resp.result.unwrap();
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert_eq!(result["serverInfo"]["name"], "knobyte");
    assert_eq!(result["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    let instructions = result["instructions"].as_str().unwrap();
    assert!(instructions.contains("Knobyte Agent Operating Rules"));
    assert!(instructions.contains("knobyte_session_start"));
    let caps = &result["capabilities"];
    assert!(caps.get("tools").is_some());
    assert!(caps.get("resources").is_some());
    assert!(caps.get("prompts").is_some());

    // Version negotiation echoes a supported requested version.
    let resp = process_jsonrpc_request(req(
        Some(json!(2)),
        "initialize",
        Some(json!({ "protocolVersion": "2025-03-26" })),
    ))
    .unwrap();
    assert_eq!(resp.result.unwrap()["protocolVersion"], "2025-03-26");
}

#[test]
fn test_mcp_jsonrpc_tools_list() {
    let resp = process_jsonrpc_request(req(Some(json!(2)), "tools/list", None)).unwrap();
    assert!(resp.error.is_none());
    let result = resp.result.unwrap();
    let tools = result["tools"].as_array().unwrap();
    assert_eq!(tools.len(), get_tools_list().len());
    assert!(tools.iter().any(|t| t["name"] == "knobyte_session_start"));
}

#[test]
fn test_mcp_resources_and_prompts() {
    let (_dir, config) = temp_project();
    let resp = process_jsonrpc_request_with_config(req(Some(json!(3)), "resources/list", None), &config).unwrap();
    assert!(resp.error.is_none());
    let list = resp.result.unwrap()["resources"].as_array().cloned().unwrap();
    assert!(list.iter().any(|r| r["uri"] == "knobyte://context/stack"));

    let resp = process_jsonrpc_request_with_config(
        req(Some(json!(4)), "resources/read", Some(json!({ "uri": "knobyte://context/stack" }))),
        &config,
    )
    .unwrap();
    assert_eq!(resp.result.unwrap()["contents"][0]["text"], "# Stack\n");

    let resp = process_jsonrpc_request_with_config(req(Some(json!(5)), "prompts/list", None), &config).unwrap();
    let prompts = resp.result.unwrap()["prompts"].as_array().cloned().unwrap();
    assert!(prompts.iter().any(|p| p["name"] == "impact-analysis"));

    let resp = process_jsonrpc_request_with_config(
        req(
            Some(json!(6)),
            "prompts/get",
            Some(json!({ "name": "impact-analysis", "arguments": { "symbol": "authenticate" } })),
        ),
        &config,
    )
    .unwrap();
    let text = resp.result.unwrap()["messages"][0]["content"]["text"].as_str().unwrap().to_string();
    assert!(text.contains("authenticate"));
}

// ---------------------------------------------------------------------------
// security: paths and project root
// ---------------------------------------------------------------------------

#[test]
fn test_read_file_rejects_traversal() {
    let (dir, config) = temp_project();
    let root = dir.path().canonicalize().unwrap();
    fs::write(root.join("secret.txt"), "TOP SECRET").unwrap();

    let ok = call(&config, "knobyte_read_file", json!({ "file": "AGENTS.md" }));
    assert!(!is_err(&ok));
    assert_eq!(ok.content[0].text, "# Agents\n");
    let ok = call(&config, "knobyte_read_file", json!({ "file": "context/stack.md" }));
    assert!(!is_err(&ok));

    for bad in [
        "../secret.txt",
        "context/../../secret.txt",
        "..\\secret.txt",
        "/etc/passwd",
        root.join("secret.txt").to_str().unwrap(),
        "../../../../../../etc/passwd",
    ] {
        let r = call(&config, "knobyte_read_file", json!({ "file": bad }));
        assert!(is_err(&r), "expected rejection for {}", bad);
        assert!(!r.content[0].text.contains("TOP SECRET"));
    }
}

#[cfg(unix)]
#[test]
fn test_read_file_rejects_symlink_escape() {
    let (dir, config) = temp_project();
    let root = dir.path().canonicalize().unwrap();
    fs::write(root.join("secret.txt"), "TOP SECRET").unwrap();
    std::os::unix::fs::symlink(root.join("secret.txt"), config.scaffold_root.join("link.md")).unwrap();
    std::os::unix::fs::symlink(&root, config.scaffold_root.join("rootdir")).unwrap();
    std::os::unix::fs::symlink(Path::new("/nonexistent/x"), config.scaffold_root.join("dangling")).unwrap();
    // A symlink that stays inside the scaffold is fine.
    std::os::unix::fs::symlink(config.scaffold_root.join("AGENTS.md"), config.scaffold_root.join("inner.md")).unwrap();

    for bad in ["link.md", "rootdir/secret.txt", "dangling"] {
        let r = call(&config, "knobyte_read_file", json!({ "file": bad }));
        assert!(is_err(&r), "expected rejection for {}", bad);
        assert!(!r.content[0].text.contains("TOP SECRET"));
    }
    let ok = call(&config, "knobyte_read_file", json!({ "file": "inner.md" }));
    assert!(!is_err(&ok));
}

#[test]
fn test_project_root_override_rejected() {
    let (dir, config) = temp_project();
    let (other, _other_cfg) = temp_project();

    let r = call(
        &config,
        "knobyte_relay_list",
        json!({ "projectRoot": other.path().to_str().unwrap() }),
    );
    assert!(is_err(&r));
    assert!(r.content[0].text.contains("projectRoot"));

    let r = call(&config, "knobyte_sync_groundings", json!({ "projectRoot": "/" }));
    assert!(is_err(&r));

    // The server's own root (in any spelling) is accepted.
    let r = call(
        &config,
        "knobyte_relay_list",
        json!({ "projectRoot": dir.path().to_str().unwrap() }),
    );
    assert!(!is_err(&r), "{}", r.content[0].text);
}

#[test]
fn test_resource_and_id_traversal_rejected() {
    let (dir, config) = temp_project();
    let root = dir.path().canonicalize().unwrap();
    fs::write(root.join("x.json"), "{\"secret\":true}").unwrap();

    for uri in ["knobyte://relay/../../x", "knobyte://relay/..%2F..%2Fx", "knobyte://relay/a/b", "knobyte://wiki/../x"] {
        let resp = process_jsonrpc_request_with_config(
            req(Some(json!(1)), "resources/read", Some(json!({ "uri": uri }))),
            &config,
        )
        .unwrap();
        assert!(resp.error.is_some(), "expected error for {}", uri);
        assert!(resp.result.is_none());
    }

    let r = call(
        &config,
        "knobyte_workstream_step_update",
        json!({ "workstreamId": "../../x", "stepId": "s1", "status": "done" }),
    );
    assert!(is_err(&r));
}

// ---------------------------------------------------------------------------
// tools behaviour
// ---------------------------------------------------------------------------

#[test]
fn test_log_write_passes_tags_files_actor() {
    let (_dir, config) = temp_project();
    let r = call(
        &config,
        "knobyte_log",
        json!({
            "action": "write",
            "kind": "decision",
            "summary": "Use sled storage",
            "tags": ["storage"],
            "files": ["src/cozo/engine.rs"]
        }),
    );
    assert!(!is_err(&r), "{}", r.content[0].text);
    let log = fs::read_to_string(config.decisions_log_path()).unwrap();
    let entry: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    assert_eq!(entry["tags"], json!(["storage"]));
    assert_eq!(entry["files"], json!(["src/cozo/engine.rs"]));
    let actor = knobyte::team::identity::current_actor_id(&config);
    if actor != "unknown" {
        assert_eq!(entry["actor"], json!(actor));
    }
    assert_eq!(entry["kind"], "decision");

    // Events cannot be attributed to somebody else.
    let r = call(&config, "knobyte_log", json!({ "action": "write", "summary": "x", "actor": "alice" }));
    assert!(is_err(&r));
    assert!(r.content[0].text.contains("ACTOR_MISMATCH"), "{}", r.content[0].text);
    assert_eq!(fs::read_to_string(config.decisions_log_path()).unwrap().lines().count(), 1);

    let r = call(&config, "knobyte_log", json!({ "action": "write", "kind": "bogus", "summary": "x" }));
    assert!(is_err(&r));
}

fn add_member(config: &KnobyteConfig, id: &str) {
    knobyte::team::members::create_member(config, id, id, None, None).unwrap();
}

#[test]
fn test_relay_draft_fields() {
    let (_dir, config) = temp_project();
    add_member(&config, "bob");
    let r = call(
        &config,
        "knobyte_relay_draft",
        json!({
            "title": "Handoff",
            "summary": "Done with parser",
            "namedRecipients": ["bob"],
            "progress": ["parser"],
            "blockers": ["none"],
            "nextActions": ["review"],
            "evidence": ["cargo test green"]
        }),
    );
    assert!(!is_err(&r), "{}", r.content[0].text);
    let draft: Value = serde_json::from_str(&r.content[0].text).unwrap();
    assert_eq!(draft["namedRecipients"], json!(["bob"]));
    assert_eq!(draft["openToTeam"], json!(false));
    assert_eq!(draft["evidence"], json!(["cargo test green"]));
    assert_eq!(draft["nextActions"], json!(["review"]));
    let expected_sender = knobyte::team::members::get_current_member(&config)
        .map(|m| m.id)
        .unwrap_or_else(|| knobyte::team::identity::current_actor_id(&config));
    assert_eq!(draft["sender"], json!(expected_sender));
}

#[test]
fn test_draft_tools_refuse_unknown_members_and_go_through_workflow() {
    let (_dir, config) = temp_project();
    add_member(&config, "alex");
    add_member(&config, "bob");
    knobyte::team::members::select_current_member(&config, "alex").unwrap();

    // Forged sender and unknown recipients are refused; nothing is written.
    let r = call(&config, "knobyte_relay_draft", json!({ "title": "t", "summary": "s", "sender": "ghost" }));
    assert!(is_err(&r));
    assert!(r.content[0].text.contains("ACTOR_MISMATCH"), "{}", r.content[0].text);
    let r = call(&config, "knobyte_relay_draft", json!({ "title": "t", "summary": "s", "sender": "bob" }));
    assert!(is_err(&r));
    let r = call(&config, "knobyte_relay_draft", json!({ "title": "t", "summary": "s", "namedRecipients": ["nobody"] }));
    assert!(is_err(&r));
    assert!(r.content[0].text.contains("nobody"), "{}", r.content[0].text);
    assert!(knobyte::team::relay::list_relay_drafts(&config).is_empty());

    // The current member may name itself; the draft is saved as that member.
    let r = call(&config, "knobyte_relay_draft", json!({ "title": "t", "summary": "s", "sender": "alex", "namedRecipients": ["bob"] }));
    assert!(!is_err(&r), "{}", r.content[0].text);
    let draft: Value = serde_json::from_str(&r.content[0].text).unwrap();
    assert_eq!(draft["sender"], "alex");

    // Inbox drafts: forged author refused; typed change accepted like the CLI.
    let r = call(&config, "knobyte_inbox_draft", json!({ "title": "t", "target": "context/stack.md", "content": "x", "reason": "r", "author": "ghost" }));
    assert!(is_err(&r));
    assert!(knobyte::team::inbox::list_inbox_drafts(&config).is_empty());
    let r = call(
        &config,
        "knobyte_inbox_draft",
        json!({
            "reason": "consistency",
            "evidence": ["file:src/lib.rs"],
            "change": { "kind": "knowledge.create", "entityKind": "convention", "title": "Errors", "body": "Use thiserror" }
        }),
    );
    assert!(!is_err(&r), "{}", r.content[0].text);
    let d: Value = serde_json::from_str(&r.content[0].text).unwrap();
    assert_eq!(d["author"], "alex");
    assert_eq!(d["change"]["kind"], "knowledge.create");
    let r = call(&config, "knobyte_inbox_draft", json!({ "title": "t", "target": "context/stack.md", "content": "# Stack\nRust\n", "reason": "r" }));
    assert!(!is_err(&r), "{}", r.content[0].text);
    assert_eq!(knobyte::team::inbox::list_inbox_drafts(&config).len(), 2);
    let r = call(&config, "knobyte_inbox_draft", json!({ "reason": "r", "change": { "kind": "raw.write" } }));
    assert!(is_err(&r));
}

#[test]
fn test_workstream_step_update_uses_workflow_and_active_workstreams_only() {
    use knobyte::team::workstreams::{archive_workstream, create_workstream, get_workstream, list_workstreams, update_workstream, WorkstreamInput, WorkstreamPatch};
    let (_dir, config) = temp_project();
    add_member(&config, "alex");
    knobyte::team::members::select_current_member(&config, "alex").unwrap();

    // No workstream: refused, and nothing is created silently.
    let r = call(&config, "knobyte_workstream_step_update", json!({ "stepId": "s1", "status": "done" }));
    assert!(is_err(&r));
    assert!(list_workstreams(&config).is_empty());
    assert!(get_workstream(&config, "ws_default").is_none());

    // Only archived / done workstreams: never picked implicitly.
    create_workstream(&config, &WorkstreamInput { id: Some("old".into()), title: "Old".into(), ..Default::default() }).unwrap();
    archive_workstream(&config, "old").unwrap();
    create_workstream(&config, &WorkstreamInput { id: Some("shipped".into()), title: "Shipped".into(), ..Default::default() }).unwrap();
    update_workstream(&config, "shipped", &WorkstreamPatch { state: Some("done".into()), ..Default::default() }).unwrap();
    let r = call(&config, "knobyte_workstream_step_update", json!({ "stepId": "s1", "status": "done" }));
    assert!(is_err(&r), "{}", r.content[0].text);
    assert!(get_workstream(&config, "old").unwrap().steps.is_empty());
    assert!(get_workstream(&config, "shipped").unwrap().steps.is_empty());
    // Explicitly naming an archived workstream is refused too.
    let r = call(&config, "knobyte_workstream_step_update", json!({ "workstreamId": "old", "stepId": "s1", "status": "done" }));
    assert!(is_err(&r));

    // An active workstream is targeted and the update is recorded as activity.
    create_workstream(&config, &WorkstreamInput { id: Some("live".into()), title: "Live".into(), ..Default::default() }).unwrap();
    let before = knobyte::team::activity::list_activity(&config, 100).len();
    let r = call(&config, "knobyte_workstream_step_update", json!({ "stepId": "s1", "status": "in_progress", "evidence": "tests" }));
    assert!(!is_err(&r), "{}", r.content[0].text);
    let ws = get_workstream(&config, "live").unwrap();
    assert_eq!(ws.steps[0].status, "in_progress");
    assert_eq!(ws.checkpoints.len(), 1);
    assert_eq!(ws.updated_by.as_deref(), Some("alex"));
    let acts = knobyte::team::activity::list_activity(&config, 100);
    assert_eq!(acts.len(), before + 1);
    assert!(acts.iter().any(|a| a.action == "workstream.step.update" && a.actor == "alex"));
    let r = call(&config, "knobyte_workstream_step_update", json!({ "status": "bogus" }));
    assert!(is_err(&r));
}

#[test]
fn test_playbook_and_catch_up_tools() {
    use knobyte::team::workflow::{run_action, ActorChoice};
    let (_dir, config) = temp_project();
    add_member(&config, "alex");
    knobyte::team::members::select_current_member(&config, "alex").unwrap();
    let pb = run_action(&config, json!({ "kind": "playbook.create", "playbook": { "id": "rel", "title": "Release", "state": "active",
        "steps": [{ "id": "test", "title": "Test", "expectedEvidence": ["test output"] }, { "id": "tag", "title": "Tag" }] } }), &ActorChoice::resolved()).unwrap();
    assert_eq!(pb.result["state"], "active");
    let run = run_action(&config, json!({ "kind": "playbook.run.start", "playbookId": "rel" }), &ActorChoice::resolved()).unwrap();
    let run_id = run.result["id"].as_str().unwrap().to_string();

    let r = call(&config, "knobyte_playbook_list", json!({}));
    assert!(!is_err(&r), "{}", r.content[0].text);
    let v: Value = serde_json::from_str(&r.content[0].text).unwrap();
    assert_eq!(v["items"][0]["id"], "rel");
    assert!(is_err(&call(&config, "knobyte_playbook_list", json!({ "state": "bogus" }))));
    let r = call(&config, "knobyte_playbook_get", json!({ "id": "rel" }));
    let v: Value = serde_json::from_str(&r.content[0].text).unwrap();
    assert_eq!(v["runs"][0]["id"], run_id.as_str());
    assert!(is_err(&call(&config, "knobyte_playbook_get", json!({}))));
    assert!(is_err(&call(&config, "knobyte_playbook_get", json!({ "id": "../x" }))));

    // Agents record step evidence as the resolved actor.
    let r = call(&config, "knobyte_playbook_complete_step", json!({ "runId": run_id, "stepId": "test", "evidence": ["file:target/report.txt", "412 passed"], "note": "green" }));
    assert!(!is_err(&r), "{}", r.content[0].text);
    let v: Value = serde_json::from_str(&r.content[0].text).unwrap();
    assert_eq!(v["steps"][0]["completedBy"], "alex");
    assert_eq!(v["steps"][0]["evidence"][0]["path"], "target/report.txt");
    // The step's 1-based number names the same (now completed) step.
    let r = call(&config, "knobyte_playbook_complete_step", json!({ "runId": run_id, "stepId": 1 }));
    assert!(is_err(&r) && r.content[0].text.contains("already complete"), "{}", r.content[0].text);
    let r = call(&config, "knobyte_playbook_get", json!({ "runId": run_id }));
    assert!(r.content[0].text.contains("412 passed"));

    // No MCP tool creates, publishes or archives playbooks, or starts/abandons runs.
    let names: Vec<String> = get_tools_list().into_iter().map(|t| t.name).collect();
    assert!(!names.iter().any(|n| n.contains("playbook") && (n.contains("create") || n.contains("archive") || n.contains("start") || n.contains("abandon"))));

    // Catch-up digest and marking the caller's local cursor.
    let r = call(&config, "knobyte_catch_up", json!({ "includeMine": true }));
    assert!(!is_err(&r), "{}", r.content[0].text);
    let v: Value = serde_json::from_str(&r.content[0].text).unwrap();
    assert!(v["items"].as_array().unwrap().iter().any(|i| i["group"] == "playbooks"));
    let observed = v["observedAt"].as_str().unwrap().to_string();
    assert!(is_err(&call(&config, "knobyte_catch_up", json!({ "since": "garbage" }))));
    let r = call(&config, "knobyte_catch_up_mark", json!({ "at": observed }));
    assert!(!is_err(&r), "{}", r.content[0].text);
    assert!(config.local_dir().join("catch-up/member-alex.json").exists());
    let r = call(&config, "knobyte_catch_up", json!({}));
    let v: Value = serde_json::from_str(&r.content[0].text).unwrap();
    assert_eq!(v["baselineSource"], "cursor");
    assert!(v["items"].as_array().unwrap().is_empty(), "{}", v);
}

#[test]
fn test_workstream_step_update_refuses_out_of_range_step_index() {
    use knobyte::team::workstreams::{create_workstream, get_workstream, WorkstreamInput};
    let (_dir, config) = temp_project();
    add_member(&config, "alex");
    knobyte::team::members::select_current_member(&config, "alex").unwrap();
    create_workstream(&config, &WorkstreamInput { id: Some("live".into()), title: "Live".into(), ..Default::default() }).unwrap();
    let r = call(&config, "knobyte_workstream_step_update", json!({ "workstreamId": "live", "stepId": "s1", "status": "in_progress" }));
    assert!(!is_err(&r), "{}", r.content[0].text);

    // Index 5 does not exist: refused, and no `step_5` is invented.
    let r = call(&config, "knobyte_workstream_step_update", json!({ "workstreamId": "live", "stepIndex": 5, "status": "done" }));
    assert!(is_err(&r));
    assert!(r.content[0].text.contains("out of range"), "{}", r.content[0].text);
    let r = call(&config, "knobyte_workstream_step_update", json!({ "workstreamId": "live", "stepIndex": -1, "status": "done" }));
    assert!(is_err(&r));
    let ws = get_workstream(&config, "live").unwrap();
    assert_eq!(ws.steps.len(), 1);
    assert_eq!(ws.steps[0].status, "in_progress");

    // An existing index still works.
    let r = call(&config, "knobyte_workstream_step_update", json!({ "workstreamId": "live", "stepIndex": 0, "status": "done" }));
    assert!(!is_err(&r), "{}", r.content[0].text);
    assert_eq!(get_workstream(&config, "live").unwrap().steps[0].status, "done");
}

#[test]
fn test_timeline_and_log_reads_are_bounded() {
    let (_dir, config) = temp_project();
    let big = "x".repeat(4000);
    for i in 0..250 {
        knobyte::events::append_event(&config, &format!("event {} {}", i, big), "note", &[], &[], None).unwrap();
    }
    for (tool, args) in [
        ("knobyte_timeline", json!({ "limit": 100000 })),
        ("knobyte_log", json!({ "action": "read", "limit": 100000 })),
    ] {
        let r = call(&config, tool, args);
        assert!(!is_err(&r), "{}", r.content[0].text);
        assert!(r.content[0].text.len() <= 64 * 1024, "{} returned {} bytes", tool, r.content[0].text.len());
        assert_eq!(r.content.len(), 2, "{} must append an omitted note", tool);
        assert!(r.content[1].text.contains("omitted"));
    }
    // Small result: no note, limit honoured.
    let r = call(&config, "knobyte_timeline", json!({ "limit": 3 }));
    let v: Value = serde_json::from_str(&r.content[0].text).unwrap();
    assert_eq!(v["entries"].as_array().unwrap().len(), 3);
}

// ---------------------------------------------------------------------------
// protocol: notifications and stdio
// ---------------------------------------------------------------------------

#[test]
fn test_notifications_get_no_response() {
    let (_dir, config) = temp_project();
    assert!(process_jsonrpc_request_with_config(req(None, "notifications/initialized", None), &config).is_none());
    assert!(process_jsonrpc_request_with_config(req(None, "notifications/cancelled", None), &config).is_none());
    assert!(process_jsonrpc_request_with_config(req(None, "some/unknown", None), &config).is_none());
    // Requests to unknown methods still get -32601.
    let resp = process_jsonrpc_request_with_config(req(Some(json!(9)), "some/unknown", None), &config).unwrap();
    assert_eq!(resp.error.unwrap().code, -32601);
}

#[test]
fn test_stdio_parse_error_and_notifications() {
    let (_dir, config) = temp_project();
    let input = concat!(
        "this is not json\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        "\n",
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n",
    );
    let mut out = Vec::new();
    run_stdio(input.as_bytes(), &mut out, &config).unwrap();
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "output: {}", text);
    let first: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["error"]["code"], -32700);
    assert!(first.get("id").is_some() && first["id"].is_null());
    let second: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(second["id"], 1);
    assert_eq!(second["result"], json!({}));
}

// ---------------------------------------------------------------------------
// HTTP transports
// ---------------------------------------------------------------------------

async fn next_sse_chunk(stream: &mut (impl futures_util::Stream<Item = Result<axum::body::Bytes, axum::Error>> + Unpin)) -> String {
    let chunk = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
        .await
        .expect("timed out waiting for SSE data")
        .expect("stream ended")
        .unwrap();
    String::from_utf8(chunk.to_vec()).unwrap()
}

#[tokio::test]
async fn test_sse_messages_202_and_session_lifecycle() {
    let (_dir, config) = temp_project();
    let app = loopback_router(config);

    let resp = app
        .clone()
        .oneshot(Request::builder().uri("/sse").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut stream = resp.into_body().into_data_stream();
    let first = next_sse_chunk(&mut stream).await;
    assert!(first.contains("event: endpoint"), "{}", first);
    let endpoint = first
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap()
        .trim()
        .to_string();
    assert!(endpoint.starts_with("/messages?sessionId="));

    // Request: 202 with empty body, response arrives over SSE.
    let body = json!({ "jsonrpc": "2.0", "id": 7, "method": "ping" });
    let resp = app
        .clone()
        .oneshot(post_json(&endpoint).body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    assert!(bytes.is_empty());
    let msg = next_sse_chunk(&mut stream).await;
    assert!(msg.contains("event: message"), "{}", msg);
    assert!(msg.contains("\"id\":7"), "{}", msg);

    // Notification: 202, nothing sent.
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    let resp = app
        .clone()
        .oneshot(post_json(&endpoint).body(Body::from(note.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    // Unknown session -> 404.
    let resp = app
        .clone()
        .oneshot(
            post_json("/messages?sessionId=does-not-exist")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Disconnecting the SSE stream removes the session.
    drop(stream);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let resp = app
        .clone()
        .oneshot(post_json(&endpoint).body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_origin_and_host_validation() {
    let (_dir, config) = temp_project();
    let app = loopback_router(config);

    let get = |origin: Option<&str>, host: Option<&str>| {
        let mut b = Request::builder().uri("/health");
        if let Some(o) = origin {
            b = b.header(header::ORIGIN, o);
        }
        if let Some(h) = host {
            b = b.header(header::HOST, h);
        }
        b.body(Body::empty()).unwrap()
    };

    let status = |r: axum::response::Response| r.status();
    assert_eq!(status(app.clone().oneshot(get(None, None)).await.unwrap()), StatusCode::OK);
    assert_eq!(
        status(app.clone().oneshot(get(Some("http://localhost:5173"), Some("localhost:3001"))).await.unwrap()),
        StatusCode::OK
    );
    assert_eq!(
        status(app.clone().oneshot(get(Some("https://evil.example"), Some("127.0.0.1:3001"))).await.unwrap()),
        StatusCode::FORBIDDEN
    );
    // DNS rebinding: Host is attacker-controlled name resolving to 127.0.0.1.
    assert_eq!(
        status(app.clone().oneshot(get(Some("http://evil.example:3001"), Some("evil.example:3001"))).await.unwrap()),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        status(app.clone().oneshot(get(None, Some("evil.example:3001"))).await.unwrap()),
        StatusCode::FORBIDDEN
    );

    // Cross-origin POST to /mcp is rejected before reaching the handler.
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" });
    let resp = app
        .clone()
        .oneshot(
            post_json("/mcp")
                .header(header::ORIGIN, "https://evil.example")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(resp.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
}

#[tokio::test]
async fn test_token_required_on_non_loopback() {
    let (tok, generated) = resolve_auth_token("0.0.0.0", None);
    assert!(generated);
    assert!(tok.as_deref().is_some_and(|t| t.len() >= 32));
    assert_eq!(resolve_auth_token("127.0.0.1", None), (None, false));
    assert_eq!(
        resolve_auth_token("127.0.0.1", Some("abc".to_string())),
        (Some("abc".to_string()), false)
    );

    let (_dir, config) = temp_project();
    let app = build_router(
        config,
        ServerSecurity { token: Some("s3cret-token".to_string()), loopback_bind: false },
    );

    let get = |uri: &str, auth: Option<&str>| {
        let mut b = Request::builder().uri(uri).header(header::HOST, "10.0.0.5:3005");
        if let Some(a) = auth {
            b = b.header(header::AUTHORIZATION, a);
        }
        b.body(Body::empty()).unwrap()
    };

    let resp = app.clone().oneshot(get("/health", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get(header::WWW_AUTHENTICATE).is_some());
    let resp = app.clone().oneshot(get("/health", Some("Bearer wrong"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let resp = app.clone().oneshot(get("/health", Some("Bearer s3cret-token"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // ?token= is accepted on /sse (EventSource cannot set headers) but not on /mcp.
    let resp = app.clone().oneshot(get("/sse?token=s3cret-token", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let mut stream = resp.into_body().into_data_stream();
    let first = next_sse_chunk(&mut stream).await;
    assert!(first.contains("token=s3cret-token"), "{}", first);

    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize" });
    let resp = app
        .clone()
        .oneshot(
            post_json("/mcp?token=s3cret-token")
                .header(header::HOST, "10.0.0.5:3005")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = app
        .clone()
        .oneshot(
            post_json("/mcp")
                .header(header::HOST, "10.0.0.5:3005")
                .header(header::AUTHORIZATION, "Bearer s3cret-token")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_streamable_http_sessions() {
    let (_dir, config) = temp_project();
    let app = loopback_router(config);

    let post = |sid: Option<&str>, body: Value| {
        let mut b = post_json("/mcp");
        if let Some(s) = sid {
            b = b.header("Mcp-Session-Id", s);
        }
        b.body(Body::from(body.to_string())).unwrap()
    };

    // initialize issues a session id
    let resp = app
        .clone()
        .oneshot(post(None, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26" } })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let sid = resp.headers().get("mcp-session-id").unwrap().to_str().unwrap().to_string();
    let v = body_json(resp).await;
    assert_eq!(v["result"]["protocolVersion"], "2025-03-26");

    // session id required afterwards
    let list = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
    assert_eq!(app.clone().oneshot(post(None, list.clone())).await.unwrap().status(), StatusCode::BAD_REQUEST);
    assert_eq!(app.clone().oneshot(post(Some("bogus"), list.clone())).await.unwrap().status(), StatusCode::NOT_FOUND);
    let resp = app.clone().oneshot(post(Some(&sid), list.clone())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["result"]["tools"].as_array().unwrap().len(), get_tools_list().len());

    // notifications -> 202 with no body
    let resp = app
        .clone()
        .oneshot(post(Some(&sid), json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    assert!(bytes.is_empty());

    // malformed JSON -> 400 parse error
    let resp = app
        .clone()
        .oneshot(post_json("/mcp").header("Mcp-Session-Id", &sid).body(Body::from("{oops")).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"]["code"], -32700);

    // DELETE ends the session
    let del = |s: &str| Request::builder().method(Method::DELETE).uri("/mcp").header("Mcp-Session-Id", s).body(Body::empty()).unwrap();
    assert_eq!(app.clone().oneshot(del(&sid)).await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(app.clone().oneshot(post(Some(&sid), list)).await.unwrap().status(), StatusCode::NOT_FOUND);
    assert_eq!(app.clone().oneshot(del(&sid)).await.unwrap().status(), StatusCode::NOT_FOUND);
}

/// `knobyte mcp --http` / `--sse` serve one transport; the other's endpoints answer 404.
/// The default serves both, and `GET /` lists only the live transports and endpoints.
#[tokio::test]
async fn test_transport_selection() {
    use knobyte::mcp::{build_router_with, HttpTransport};
    let router = |t: HttpTransport| {
        let (dir, config) = temp_project();
        (dir, build_router_with(config, ServerSecurity { token: None, loopback_bind: true }, t))
    };
    let init = || {
        post_json("/mcp")
            .body(Body::from(
                json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26" } })
                    .to_string(),
            ))
            .unwrap()
    };
    let sse = || Request::builder().uri("/sse").body(Body::empty()).unwrap();
    let messages = || post_json("/messages?sessionId=x").body(Body::from("{}")).unwrap();
    let root = || Request::builder().uri("/").body(Body::empty()).unwrap();

    // --http: streamable HTTP only.
    let (_d1, app) = router(HttpTransport::StreamableHttp);
    assert_eq!(app.clone().oneshot(init()).await.unwrap().status(), StatusCode::OK);
    let resp = app.clone().oneshot(sse()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(body_json(resp).await["error"].as_str().unwrap().contains("--http"));
    assert_eq!(app.clone().oneshot(messages()).await.unwrap().status(), StatusCode::NOT_FOUND);
    let v = body_json(app.clone().oneshot(root()).await.unwrap()).await;
    assert_eq!(v["transports"], json!(["streamable-http"]));
    assert!(v["endpoints"].get("sse").is_none() && v["endpoints"]["mcp"] == "/mcp");

    // --sse: legacy SSE only.
    let (_d2, app) = router(HttpTransport::Sse);
    let resp = app.clone().oneshot(init()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(body_json(resp).await["error"].as_str().unwrap().contains("--sse"));
    let resp = app.clone().oneshot(sse()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers()[header::CONTENT_TYPE].to_str().unwrap().starts_with("text/event-stream"));
    let v = body_json(app.clone().oneshot(root()).await.unwrap()).await;
    assert_eq!(v["transports"], json!(["sse"]));
    assert!(v["endpoints"].get("mcp").is_none() && v["endpoints"]["sse"] == "/sse");

    // Default: both.
    let (_d3, app) = router(HttpTransport::Both);
    assert_eq!(app.clone().oneshot(init()).await.unwrap().status(), StatusCode::OK);
    assert_eq!(app.clone().oneshot(sse()).await.unwrap().status(), StatusCode::OK);
    let v = body_json(app.clone().oneshot(root()).await.unwrap()).await;
    assert_eq!(v["transports"], json!(["sse", "streamable-http"]));
}

/// The transport flags are mutually exclusive.
#[test]
fn test_mcp_transport_flags_conflict() {
    for args in [["--http", "--sse"], ["--stdio", "--http"], ["--stdio", "--sse"]] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_knobyte"))
            .arg("mcp")
            .args(args)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{:?}", args);
        assert!(String::from_utf8_lossy(&out.stderr).contains("cannot be used with"), "{:?}", args);
    }
}
