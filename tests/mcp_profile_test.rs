//! MCP tool profiles (core/team/wiki/graph/full), their precedence, out-of-profile refusals,
//! the hidden aliases of merged tools, and the Datalog reference resource.

use std::fs;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

use knobyte::config::KnobyteConfig;
use knobyte::mcp::handler::process_jsonrpc_request_with_profile;
use knobyte::mcp::profiles::{
    config_profile, out_of_profile_message, resolve_profile, McpProfile, ProfileSource, ALIASES, TOOL_PROFILES,
};
use knobyte::mcp::protocol::{CallToolResult, JsonRpcRequest, JsonRpcResponse};
use knobyte::mcp::sse::{build_router_with_profile, HttpTransport, ServerSecurity};
use knobyte::mcp::tools::{execute_tool_with_config, get_tools_list, tools_for_profile};

fn temp_project() -> (tempfile::TempDir, KnobyteConfig) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let scaffold = root.join(".knobyte");
    fs::create_dir_all(scaffold.join("context")).unwrap();
    fs::write(scaffold.join("AGENTS.md"), "# Agents\n").unwrap();
    let config = KnobyteConfig::new(root, scaffold);
    (dir, config)
}

fn rpc(config: &KnobyteConfig, profile: McpProfile, method: &str, params: Value) -> JsonRpcResponse {
    let req = JsonRpcRequest { jsonrpc: "2.0".into(), id: Some(json!(1)), method: method.into(), params: Some(params) };
    process_jsonrpc_request_with_profile(req, config, profile).unwrap()
}

fn listed(config: &KnobyteConfig, profile: McpProfile) -> Vec<String> {
    let resp = rpc(config, profile, "tools/list", json!({}));
    resp.result.unwrap()["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

fn call(config: &KnobyteConfig, name: &str, args: Value) -> CallToolResult {
    execute_tool_with_config(name, &args, config)
}

fn ok_json(r: &CallToolResult) -> Value {
    assert!(r.is_error != Some(true), "{}", r.content[0].text);
    serde_json::from_str(&r.content[0].text).unwrap()
}

// ---------------------------------------------------------------------------
// profile table and tools/list
// ---------------------------------------------------------------------------

#[test]
fn tools_list_counts_per_profile() {
    let (_dir, config) = temp_project();
    let expected = [
        (McpProfile::Core, 15),
        (McpProfile::Team, 18),
        (McpProfile::Wiki, 19),
        (McpProfile::Graph, 20),
        (McpProfile::Full, 30),
    ];
    for (profile, n) in expected {
        let names = listed(&config, profile);
        assert_eq!(names.len(), n, "{}: {:?}", profile, names);
        assert_eq!(names.len(), tools_for_profile(profile).len());
    }
    // Every profile contains core; full lists every tool.
    let core = listed(&config, McpProfile::Core);
    for profile in McpProfile::ALL {
        let names = listed(&config, profile);
        assert!(core.iter().all(|t| names.contains(t)), "{} lacks a core tool", profile);
    }
    assert_eq!(listed(&config, McpProfile::Full).len(), get_tools_list().len());
    for t in ["knobyte_session_start", "knobyte_graph_scope", "knobyte_graph_query", "knobyte_vector_search", "knobyte_wiki_search",
        "knobyte_wiki_get", "knobyte_check", "knobyte_log", "knobyte_timeline", "knobyte_catch_up", "knobyte_relay_draft",
        "knobyte_inbox_draft", "knobyte_members"] {
        assert!(core.contains(&t.to_string()), "core lacks {}", t);
    }
    assert!(listed(&config, McpProfile::Graph).contains(&"knobyte_cozo_datalog".to_string()));
    assert!(listed(&config, McpProfile::Wiki).contains(&"knobyte_wiki_plan_operation".to_string()));
    assert!(listed(&config, McpProfile::Team).contains(&"knobyte_playbooks".to_string()));
    assert!(!core.contains(&"knobyte_cozo_datalog".to_string()));
}

#[test]
fn profile_table_matches_tool_definitions() {
    let defs: Vec<String> = get_tools_list().into_iter().map(|t| t.name).collect();
    let table: Vec<String> = TOOL_PROFILES.iter().map(|(t, _)| t.to_string()).collect();
    assert_eq!(defs, table, "TOOL_PROFILES and the tool definitions must list the same tools in the same order");
    for a in ALIASES {
        assert!(!table.contains(&a.name.to_string()), "{} is both a tool and an alias", a.name);
    }
}

#[test]
fn no_tool_decides_human_only_actions() {
    for t in get_tools_list() {
        let n = &t.name;
        assert!(!n.contains("approve") && !n.contains("reject") && !n.contains("publish") && !n.contains("archive"), "{}", n);
    }
}

// ---------------------------------------------------------------------------
// precedence
// ---------------------------------------------------------------------------

#[test]
fn profile_precedence_flag_env_config_default() {
    let r = resolve_profile(Some("graph"), Some("team"), Some("wiki")).unwrap();
    assert_eq!((r.profile, r.source), (McpProfile::Graph, ProfileSource::Flag));
    let r = resolve_profile(None, Some("team"), Some("wiki")).unwrap();
    assert_eq!((r.profile, r.source), (McpProfile::Team, ProfileSource::Env));
    let r = resolve_profile(None, Some("  "), Some("wiki")).unwrap();
    assert_eq!((r.profile, r.source), (McpProfile::Wiki, ProfileSource::Config));
    let r = resolve_profile(None, None, None).unwrap();
    assert_eq!((r.profile, r.source), (McpProfile::Core, ProfileSource::Default));
    assert_eq!(resolve_profile(None, Some("FULL"), None).unwrap().profile, McpProfile::Full);
    let e = resolve_profile(None, Some("everything"), None).unwrap_err();
    assert!(e.contains("everything") && e.contains("KNOBYTE_MCP_PROFILE") && e.contains("core, team, wiki, graph, full"), "{}", e);
}

#[test]
fn config_json_mcp_profile_is_read() {
    let (_dir, config) = temp_project();
    assert_eq!(config_profile(&config.scaffold_root), None);
    fs::write(config.scaffold_root.join("config.json"), r#"{"mode":"code-repo","mcp":{"profile":"team"}}"#).unwrap();
    assert_eq!(config_profile(&config.scaffold_root).as_deref(), Some("team"));
    let r = resolve_profile(None, None, config_profile(&config.scaffold_root).as_deref()).unwrap();
    assert_eq!((r.profile, r.source), (McpProfile::Team, ProfileSource::Config));
}

fn run_stdio_bin(dir: &std::path::Path, args: &[&str], env: Option<&str>) -> (Option<i32>, Vec<Value>, String) {
    use std::io::Write;
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_knobyte"));
    cmd.arg("mcp").arg("--stdio").args(args).current_dir(dir).env_remove("KNOBYTE_MCP_PROFILE");
    cmd.env("KNOBYTE_HOME", dir.join("home")).env("KNOBYTE_NO_AGENT_LAUNCH", "1");
    if let Some(v) = env {
        cmd.env("KNOBYTE_MCP_PROFILE", v);
    }
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let lines = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    (out.status.code(), lines, String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn mcp_command_applies_profile_precedence() {
    let (dir, config) = temp_project();
    let root = config.project_root.clone();
    let count = |lines: &[Value]| lines[1]["result"]["tools"].as_array().unwrap().len();

    let (code, lines, err) = run_stdio_bin(&root, &[], None);
    assert_eq!(code, Some(0), "{}", err);
    assert_eq!(count(&lines), 15);
    assert_eq!(lines[0]["result"]["serverInfo"]["profile"], "core");
    assert!(err.contains("tool profile core (15 tools, from default)"), "{}", err);

    fs::write(config.scaffold_root.join("config.json"), r#"{"mcp":{"profile":"team"}}"#).unwrap();
    let (_, lines, err) = run_stdio_bin(&root, &[], None);
    assert_eq!(count(&lines), 18, "{}", err);
    assert!(err.contains("from .knobyte/config.json mcp.profile"), "{}", err);

    let (_, lines, err) = run_stdio_bin(&root, &[], Some("wiki"));
    assert_eq!(count(&lines), 19, "{}", err);
    assert!(err.contains("from KNOBYTE_MCP_PROFILE"), "{}", err);

    let (_, lines, err) = run_stdio_bin(&root, &["--profile", "full"], Some("wiki"));
    assert_eq!(count(&lines), 30, "{}", err);
    assert!(err.contains("from --profile"), "{}", err);

    // Unknown names: clap rejects the flag; an unknown env value stops startup.
    let (code, _, _) = run_stdio_bin(&root, &["--profile", "huge"], None);
    assert_eq!(code, Some(2));
    let (code, lines, err) = run_stdio_bin(&root, &[], Some("huge"));
    assert_eq!(code, Some(2));
    assert!(lines.is_empty());
    assert!(err.contains("Unknown MCP profile 'huge'"), "{}", err);
    drop(dir);
}

// ---------------------------------------------------------------------------
// out-of-profile refusal and initialize
// ---------------------------------------------------------------------------

#[test]
fn out_of_profile_call_names_the_profiles() {
    let (_dir, config) = temp_project();
    let resp = rpc(&config, McpProfile::Core, "tools/call", json!({ "name": "knobyte_cozo_datalog", "arguments": { "script": "?[x] := x = 1" } }));
    let err = resp.error.expect("refused");
    assert!(err.message.contains("'knobyte_cozo_datalog'"), "{}", err.message);
    assert!(err.message.contains("'core'"), "{}", err.message);
    assert!(err.message.contains("graph, full"), "{}", err.message);
    assert!(err.message.contains("--profile graph"), "{}", err.message);

    // Aliases resolve first: a retired team tool names its merged tool and profiles.
    let resp = rpc(&config, McpProfile::Core, "tools/call", json!({ "name": "knobyte_playbook_list", "arguments": {} }));
    let msg = resp.error.expect("refused").message;
    assert!(msg.contains("now knobyte_playbooks") && msg.contains("team, full"), "{}", msg);
    assert_eq!(msg, out_of_profile_message("knobyte_playbook_list", McpProfile::Core));

    // The same tools are accepted where their profile is active.
    let resp = rpc(&config, McpProfile::Team, "tools/call", json!({ "name": "knobyte_playbook_list", "arguments": {} }));
    assert!(resp.error.is_none(), "{:?}", resp.error);
    let resp = rpc(&config, McpProfile::Full, "tools/call", json!({ "name": "knobyte_heartbeat", "arguments": {} }));
    assert!(resp.error.is_none(), "{:?}", resp.error);

    // An alias of a core tool works in core.
    let resp = rpc(&config, McpProfile::Core, "tools/call", json!({ "name": "knobyte_member_list", "arguments": {} }));
    assert!(resp.error.is_none(), "{:?}", resp.error);

    // Unknown names are still unknown.
    let resp = rpc(&config, McpProfile::Full, "tools/call", json!({ "name": "knobyte_nope", "arguments": {} }));
    assert!(resp.error.unwrap().message.contains("Unknown tool"));
}

#[test]
fn initialize_reports_the_active_profile() {
    let (_dir, config) = temp_project();
    let result = rpc(&config, McpProfile::Graph, "initialize", json!({})).result.unwrap();
    assert_eq!(result["serverInfo"]["profile"], "graph");
    let instructions = result["instructions"].as_str().unwrap();
    assert!(instructions.contains("Active tool profile: graph (20 tools"), "{}", instructions);
    assert!(instructions.contains("--profile"));
    // Every tool the base instructions name is in the default profile.
    let core = listed(&config, McpProfile::Core);
    for t in ["knobyte_session_start", "knobyte_graph_query", "knobyte_workstream_step_update", "knobyte_log", "knobyte_relay_draft"] {
        assert!(core.contains(&t.to_string()), "{}", t);
    }
}

#[tokio::test]
async fn health_and_root_report_the_profile() {
    let (_dir, config) = temp_project();
    let app = build_router_with_profile(config, ServerSecurity { token: None, loopback_bind: true }, HttpTransport::Both, McpProfile::Wiki);
    let get = |uri: &str| Request::builder().uri(uri).header("host", "127.0.0.1:3005").body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(get("/health")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(v["profile"], "wiki");
    assert_eq!(v["tools"].as_array().unwrap().len(), 19);
    let resp = app.oneshot(get("/")).await.unwrap();
    let v: Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap()).unwrap();
    assert_eq!(v["profile"], "wiki");
    assert_eq!(v["tools"], 19);
}

// ---------------------------------------------------------------------------
// Datalog reference resource
// ---------------------------------------------------------------------------

#[test]
fn datalog_reference_resource_is_listed_and_readable() {
    let (_dir, config) = temp_project();
    let list = rpc(&config, McpProfile::Core, "resources/list", json!({})).result.unwrap();
    let r = list["resources"].as_array().unwrap().iter().find(|r| r["uri"] == "knobyte://reference/datalog-schema").cloned();
    assert_eq!(r.expect("listed")["mimeType"], "text/markdown");
    let read = rpc(&config, McpProfile::Core, "resources/read", json!({ "uri": "knobyte://reference/datalog-schema" })).result.unwrap();
    let text = read["contents"][0]["text"].as_str().unwrap();
    for needle in ["code_nodes{", "code_edges{", "wiki_entities{", "embedding_meta{", "node_vec", "?[caller, callee]"] {
        assert!(text.contains(needle), "missing {}", needle);
    }
}

// ---------------------------------------------------------------------------
// aliases of merged team/drift tools (wiki aliases: tests/mcp_wiki_test.rs)
// ---------------------------------------------------------------------------

fn team_project() -> (tempfile::TempDir, KnobyteConfig) {
    let (dir, config) = temp_project();
    knobyte::team::members::create_member(&config, "alex", "alex", None, None).unwrap();
    knobyte::team::members::create_member(&config, "bo", "bo", None, None).unwrap();
    knobyte::team::members::select_current_member(&config, "alex").unwrap();
    (dir, config)
}

#[test]
fn alias_member_list_and_member_current_project_knobyte_members() {
    let (_dir, config) = team_project();
    let merged = ok_json(&call(&config, "knobyte_members", json!({})));
    assert_eq!(merged["current"]["id"], "alex");
    assert_eq!(merged["members"].as_array().unwrap().len(), 2);
    assert_eq!(ok_json(&call(&config, "knobyte_member_list", json!({}))), merged["members"]);
    assert_eq!(ok_json(&call(&config, "knobyte_member_current", json!({}))), merged["current"]);

    let (_dir, empty) = temp_project();
    let merged = ok_json(&call(&empty, "knobyte_members", json!({})));
    assert!(merged["current"].is_null());
    assert_eq!(call(&empty, "knobyte_member_current", json!({})).content[0].text, "null");
}

fn with_playbook(config: &KnobyteConfig) -> String {
    use knobyte::team::workflow::{run_action, ActorChoice};
    run_action(config, json!({ "kind": "playbook.create", "playbook": { "id": "rel", "title": "Release", "state": "active",
        "steps": [{ "id": "test", "title": "Test" }] } }), &ActorChoice::resolved()).unwrap();
    let run = run_action(config, json!({ "kind": "playbook.run.start", "playbookId": "rel" }), &ActorChoice::resolved()).unwrap();
    run.result["id"].as_str().unwrap().to_string()
}

#[test]
fn alias_playbook_list_equals_knobyte_playbooks_without_id() {
    let (_dir, config) = team_project();
    with_playbook(&config);
    let legacy = ok_json(&call(&config, "knobyte_playbook_list", json!({ "limit": 10 })));
    let merged = ok_json(&call(&config, "knobyte_playbooks", json!({ "limit": 10 })));
    assert_eq!(legacy["items"][0]["id"], "rel");
    assert_eq!(legacy["items"], merged["items"]);
}

#[test]
fn alias_playbook_get_equals_knobyte_playbooks_with_id_or_run() {
    let (_dir, config) = team_project();
    let run_id = with_playbook(&config);
    let legacy = ok_json(&call(&config, "knobyte_playbook_get", json!({ "id": "rel" })));
    let merged = ok_json(&call(&config, "knobyte_playbooks", json!({ "id": "rel" })));
    assert_eq!(legacy, merged);
    let legacy = ok_json(&call(&config, "knobyte_playbook_get", json!({ "runId": run_id })));
    let merged = ok_json(&call(&config, "knobyte_playbooks", json!({ "runId": run_id })));
    assert_eq!(legacy["id"], merged["id"]);
    // The old tool still requires an id or runId.
    assert_eq!(call(&config, "knobyte_playbook_get", json!({})).is_error, Some(true));
}

#[test]
fn alias_catch_up_mark_equals_catch_up_with_mark() {
    let (_dir, config) = team_project();
    let digest = ok_json(&call(&config, "knobyte_catch_up", json!({})));
    let at = digest["observedAt"].as_str().unwrap().to_string();
    let legacy = ok_json(&call(&config, "knobyte_catch_up_mark", json!({ "at": at })));
    let cursor = config.local_dir().join("catch-up/member-alex.json");
    assert!(cursor.exists());
    fs::remove_file(&cursor).unwrap();
    let merged = ok_json(&call(&config, "knobyte_catch_up", json!({ "mark": true, "at": at })));
    assert!(cursor.exists());
    assert_eq!(legacy.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()), merged.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()));
}

#[test]
fn alias_sync_groundings_equals_check_fix_result() {
    let (_dir, config) = temp_project();
    let legacy = ok_json(&call(&config, "knobyte_sync_groundings", json!({ "dryRun": true })));
    let merged = ok_json(&call(&config, "knobyte_check", json!({ "fix": true, "dryRun": true })));
    assert_eq!(merged["fix"], legacy);
    assert!(merged["report"]["score"].is_number(), "{}", merged);
    // Without fix the check returns the plain report.
    let plain = ok_json(&call(&config, "knobyte_check", json!({})));
    assert!(plain.get("fix").is_none() && plain["score"].is_number());
}

#[test]
fn every_alias_has_a_test_and_resolves_to_a_listed_tool() {
    let names: Vec<String> = get_tools_list().into_iter().map(|t| t.name).collect();
    assert_eq!(ALIASES.len(), 10);
    for a in ALIASES {
        assert!(names.contains(&a.target.to_string()), "{}", a.name);
        assert_eq!(knobyte::mcp::profiles::canonical_tool(a.name), a.target);
    }
}
