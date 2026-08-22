//! Transport-independent JSON-RPC dispatch for the Knobyte MCP server.

use serde_json::{json, Value};

use crate::config::KnobyteConfig;
use crate::mcp::protocol::{CallToolResult, JsonRpcRequest, JsonRpcResponse};
use crate::mcp::security::validate_id;
use crate::mcp::profiles::{canonical_tool, out_of_profile_message, McpProfile};
use crate::mcp::tools::{execute_tool_with_config, is_known_tool, tools_for_profile, DATALOG_REFERENCE, DATALOG_REFERENCE_URI};

/// Protocol revisions this server can speak, oldest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// Revision used when the client does not request one it supports.
pub const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

/// Resolve the server's project configuration from the working directory.
pub fn server_config() -> KnobyteConfig {
    crate::config::find_config(None).unwrap_or_else(|_| {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        KnobyteConfig::new(cwd.clone(), cwd.join(".knobyte"))
    })
}

/// Process a request using the configuration discovered from the working directory.
/// Returns `None` for notifications (messages without an `id`).
pub fn process_jsonrpc_request(req: JsonRpcRequest) -> Option<JsonRpcResponse> {
    process_jsonrpc_request_with_config(req, &server_config())
}

/// Process a raw JSON-RPC text payload (single message or batch).
/// Returns the serialized response, or `None` when nothing must be sent back.
pub fn handle_text(text: &str, config: &KnobyteConfig, profile: McpProfile) -> Option<String> {
    match serde_json::from_str::<Value>(text) {
        Ok(v) => handle_value(v, config, profile).map(|r| r.to_string()),
        Err(e) => Some(
            serde_json::to_string(&JsonRpcResponse::error(
                Some(Value::Null),
                PARSE_ERROR,
                &format!("Parse error: {}", e),
            ))
            .unwrap_or_default(),
        ),
    }
}

/// Process a parsed JSON-RPC payload (single message or batch).
pub fn handle_value(value: Value, config: &KnobyteConfig, profile: McpProfile) -> Option<Value> {
    match value {
        Value::Array(items) => {
            if items.is_empty() {
                return Some(invalid_request(Value::Null, "Empty batch"));
            }
            let responses: Vec<Value> = items
                .into_iter()
                .filter_map(|item| handle_single(item, config, profile))
                .collect();
            if responses.is_empty() {
                None
            } else {
                Some(Value::Array(responses))
            }
        }
        other => handle_single(other, config, profile),
    }
}

/// True when the payload contains only notifications/responses (no requests).
pub fn is_request_free(value: &Value) -> bool {
    fn is_request(v: &Value) -> bool {
        v.get("method").is_some() && v.get("id").is_some_and(|id| !id.is_null())
    }
    match value {
        Value::Array(items) => !items.iter().any(is_request),
        v => !is_request(v),
    }
}

fn invalid_request(id: Value, msg: &str) -> Value {
    serde_json::to_value(JsonRpcResponse::error(Some(id), INVALID_REQUEST, msg)).unwrap_or_default()
}

fn handle_single(value: Value, config: &KnobyteConfig, profile: McpProfile) -> Option<Value> {
    if !value.is_object() {
        return Some(invalid_request(Value::Null, "Invalid Request"));
    }
    let raw_id = value.get("id").cloned();
    // Responses from the client (result/error without method) need no reply.
    if value.get("method").is_none() && (value.get("result").is_some() || value.get("error").is_some()) {
        return None;
    }
    match serde_json::from_value::<JsonRpcRequest>(value) {
        Ok(req) => {
            if req.jsonrpc != "2.0" {
                return req
                    .id
                    .map(|id| invalid_request(id, "Invalid Request: jsonrpc must be \"2.0\""));
            }
            process_jsonrpc_request_with_profile(req, config, profile)
                .map(|r| serde_json::to_value(r).unwrap_or_default())
        }
        // A malformed notification (no id) gets no reply.
        Err(e) => raw_id.map(|id| invalid_request(id, &format!("Invalid Request: {}", e))),
    }
}

/// Negotiate the protocol version from `initialize` params.
pub fn negotiate_protocol_version(params: Option<&Value>) -> &'static str {
    let requested = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_str());
    match requested {
        Some(r) => SUPPORTED_PROTOCOL_VERSIONS
            .iter()
            .find(|v| **v == r)
            .copied()
            .unwrap_or_else(|| SUPPORTED_PROTOCOL_VERSIONS[SUPPORTED_PROTOCOL_VERSIONS.len() - 1]),
        None => DEFAULT_PROTOCOL_VERSION,
    }
}

/// Dispatch one JSON-RPC message with the tool profile configured for the project
/// (`KNOBYTE_MCP_PROFILE`, then `mcp.profile` in config.json, else `core`). Messages without
/// an `id` are notifications and never produce a response.
pub fn process_jsonrpc_request_with_config(req: JsonRpcRequest, config: &KnobyteConfig) -> Option<JsonRpcResponse> {
    process_jsonrpc_request_with_profile(req, config, McpProfile::configured(&config.scaffold_root))
}

/// Dispatch one JSON-RPC message, listing and accepting only the tools of `profile`.
pub fn process_jsonrpc_request_with_profile(
    req: JsonRpcRequest,
    config: &KnobyteConfig,
    profile: McpProfile,
) -> Option<JsonRpcResponse> {
    let id = match req.id.clone() {
        Some(Value::Null) | None => {
            // Notifications (e.g. notifications/initialized, notifications/cancelled)
            // are accepted silently.
            return None;
        }
        Some(id) => id,
    };
    Some(dispatch(req, Some(id), config, profile))
}

/// The `initialize` instructions for `profile`.
pub fn server_instructions(profile: McpProfile) -> String {
    let mut text = String::from(BASE_INSTRUCTIONS);
    text.push_str(&format!(
        "\nActive tool profile: {} ({} tools; {}). Other profiles: {}. Start the server with `knobyte mcp --profile <name>` for more tools.",
        profile,
        profile.tool_names().len(),
        profile.summary(),
        McpProfile::ALL.iter().filter(|p| **p != profile).map(|p| p.name()).collect::<Vec<_>>().join(", ")
    ));
    text
}

const BASE_INSTRUCTIONS: &str = "\
Knobyte Agent Operating Rules:\n\
1. On session start, call `knobyte_session_start` to review open workstreams, current steps, dirty files, and team handoffs.\n\
2. Always read context (`context/stack.md`, `AGENTS.md`, `ROUTER.md`) before writing code.\n\
3. Ground code changes with `knobyte_graph_query` ('where-defined', 'who-calls', 'who-imports') before modifying traits, ports, or shared structs.\n\
4. Checkpoint your work using `knobyte_workstream_step_update` whenever a step status changes or tests pass, so progress survives interruptions.\n\
5. Log key architectural decisions, discovered invariants, and risks with `knobyte_log`.\n\
6. At session end, use `knobyte_relay_draft` to leave a structured handoff for the next session or collaborator. Drafts are reviewed and published by a human.";

fn dispatch(req: JsonRpcRequest, id: Option<Value>, config: &KnobyteConfig, profile: McpProfile) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => {
            let instructions = server_instructions(profile);

            let result = json!({
                "protocolVersion": negotiate_protocol_version(req.params.as_ref()),
                "capabilities": {
                    "tools": { "listChanged": false },
                    "resources": { "subscribe": false, "listChanged": false },
                    "prompts": { "listChanged": false }
                },
                "serverInfo": {
                    "name": crate::version::APP_NAME,
                    "version": crate::version::VERSION,
                    "profile": profile.name()
                },
                "instructions": instructions
            });
            JsonRpcResponse::success(id, result)
        }
        "ping" => JsonRpcResponse::success(id, json!({})),
        "tools/list" => JsonRpcResponse::success(id, json!({ "tools": tools_for_profile(profile) })),
        "tools/call" => {
            let params = req.params.unwrap_or_default();
            let name = match params.get("name").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => return JsonRpcResponse::error(id, INVALID_PARAMS, "'name' parameter is required"),
            };
            if !is_known_tool(name) {
                return JsonRpcResponse::error(id, INVALID_PARAMS, &format!("Unknown tool: {}", name));
            }
            // Retired names resolve to their merged tool before the profile check.
            if !profile.includes(canonical_tool(name)) {
                return JsonRpcResponse::error(id, INVALID_PARAMS, &out_of_profile_message(name, profile));
            }
            let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let result: CallToolResult = execute_tool_with_config(name, &arguments, config);
            JsonRpcResponse::success(id, serde_json::to_value(result).unwrap_or_default())
        }
        "resources/list" => {
            let mut resources = vec![
                json!({
                    "uri": "knobyte://context/stack",
                    "name": "Tech Stack Context",
                    "description": "Project tech stack, architecture, and dependencies",
                    "mimeType": "text/markdown"
                }),
                json!({
                    "uri": "knobyte://scaffold/AGENTS",
                    "name": "Agent Operating Guidelines",
                    "description": "Rules and conventions for AI agents operating in this repository",
                    "mimeType": "text/markdown"
                }),
                json!({
                    "uri": "knobyte://scaffold/ROUTER",
                    "name": "Context Routing Guide",
                    "description": "Repository context routing guidelines",
                    "mimeType": "text/markdown"
                }),
                json!({
                    "uri": "knobyte://workstreams/current",
                    "name": "Active Workstreams",
                    "description": "Workstreams, plans, steps, and progress checkpoints",
                    "mimeType": "application/json"
                }),
                json!({
                    "uri": "knobyte://log/decisions",
                    "name": "Decisions & Discoveries Log",
                    "description": "Recorded architectural decisions, risks, and discoveries",
                    "mimeType": "application/jsonl"
                }),
                json!({
                    "uri": DATALOG_REFERENCE_URI,
                    "name": "Datalog Schema Reference",
                    "description": "Relations, vector indices and example queries for knobyte_cozo_datalog",
                    "mimeType": "text/markdown"
                }),
            ];

            if config.wiki_db_path().exists() {
                if let Ok(wiki) = crate::wiki::WikiIndex::open(&config.wiki_db_path()) {
                    if let Ok(entities) = wiki.list() {
                        for entity in entities {
                            resources.push(json!({
                                "uri": format!("knobyte://wiki/{}", entity.id),
                                "name": entity.title,
                                "description": format!("Wiki entity: {}", entity.entity_type),
                                "mimeType": "application/json"
                            }));
                        }
                    }
                }
            }

            for r in crate::team::relay::list_relays(config) {
                resources.push(json!({
                    "uri": format!("knobyte://relay/{}", r.id),
                    "name": r.title,
                    "description": format!("Handoff relay from {}", r.sender),
                    "mimeType": "application/json"
                }));
            }

            JsonRpcResponse::success(id, json!({ "resources": resources }))
        }
        "resources/templates/list" => {
            let templates = vec![
                json!({
                    "uriTemplate": "knobyte://wiki/{id}",
                    "name": "Wiki Entity",
                    "description": "Project documentation, architecture guides, and conventions",
                    "mimeType": "application/json"
                }),
                json!({
                    "uriTemplate": "knobyte://relay/{id}",
                    "name": "Team Relay",
                    "description": "Handoff relay documents for team collaboration",
                    "mimeType": "application/json"
                }),
            ];
            JsonRpcResponse::success(id, json!({ "resourceTemplates": templates }))
        }
        "resources/read" => {
            let params = req.params.unwrap_or_default();
            let uri = match params.get("uri").and_then(|v| v.as_str()) {
                Some(u) => u,
                None => return JsonRpcResponse::error(id, INVALID_PARAMS, "'uri' parameter is required"),
            };

            let (text, mime) = if uri == DATALOG_REFERENCE_URI {
                (DATALOG_REFERENCE.to_string(), "text/markdown")
            } else if uri == "knobyte://context/stack" {
                let path = config.context_dir().join("stack.md");
                (std::fs::read_to_string(&path).unwrap_or_default(), "text/markdown")
            } else if uri == "knobyte://scaffold/AGENTS" {
                let path = config.scaffold_root.join("AGENTS.md");
                (std::fs::read_to_string(&path).unwrap_or_default(), "text/markdown")
            } else if uri == "knobyte://scaffold/ROUTER" {
                let path = config.scaffold_root.join("ROUTER.md");
                (std::fs::read_to_string(&path).unwrap_or_default(), "text/markdown")
            } else if uri == "knobyte://log/decisions" {
                let path = config.decisions_log_path();
                (std::fs::read_to_string(&path).unwrap_or_default(), "application/jsonl")
            } else if uri == "knobyte://workstreams/current" {
                let ws = crate::team::workstreams::list_workstreams(config);
                (serde_json::to_string_pretty(&ws).unwrap_or_default(), "application/json")
            } else if let Some(wiki_id) = uri.strip_prefix("knobyte://wiki/") {
                if let Err(e) = validate_id(wiki_id) {
                    return JsonRpcResponse::error(id, INVALID_PARAMS, &format!("Invalid wiki id: {}", e));
                }
                if !config.wiki_db_path().exists() {
                    return JsonRpcResponse::error(id, INVALID_PARAMS, "Wiki database not found");
                }
                match crate::wiki::WikiIndex::open(&config.wiki_db_path()) {
                    Ok(wiki) => match wiki.show(wiki_id) {
                        Ok(Some(entity)) => (serde_json::to_string_pretty(&entity).unwrap_or_default(), "application/json"),
                        _ => {
                            return JsonRpcResponse::error(id, INVALID_PARAMS, &format!("Wiki entity '{}' not found", wiki_id))
                        }
                    },
                    Err(_) => return JsonRpcResponse::error(id, INVALID_PARAMS, "Wiki database not found"),
                }
            } else if let Some(relay_id) = uri.strip_prefix("knobyte://relay/") {
                if let Err(e) = validate_id(relay_id) {
                    return JsonRpcResponse::error(id, INVALID_PARAMS, &format!("Invalid relay id: {}", e));
                }
                let path = config.relays_dir().join(format!("{}.json", relay_id));
                match std::fs::read_to_string(&path) {
                    Ok(t) => (t, "application/json"),
                    Err(_) => {
                        return JsonRpcResponse::error(id, INVALID_PARAMS, &format!("Relay '{}' not found", relay_id))
                    }
                }
            } else {
                return JsonRpcResponse::error(id, INVALID_PARAMS, &format!("Unknown resource URI: {}", uri));
            };

            JsonRpcResponse::success(
                id,
                json!({ "contents": [ { "uri": uri, "mimeType": mime, "text": text } ] }),
            )
        }
        "prompts/list" => {
            let prompts = vec![
                json!({
                    "name": "start-session",
                    "description": "Orient a newly started agent: review workstreams, step in progress, dirty files, and open relays",
                    "arguments": []
                }),
                json!({
                    "name": "end-session",
                    "description": "Summarize work completed in this session, run tests, and prepare a handoff relay",
                    "arguments": [
                        { "name": "summary", "description": "High-level summary of work performed", "required": false }
                    ]
                }),
                json!({
                    "name": "impact-analysis",
                    "description": "Analyze the blast radius of modifying a trait, method, port, or schema",
                    "arguments": [
                        { "name": "symbol", "description": "Symbol or trait name to inspect", "required": true }
                    ]
                }),
            ];
            JsonRpcResponse::success(id, json!({ "prompts": prompts }))
        }
        "prompts/get" => {
            let params = req.params.unwrap_or_default();
            let name = match params.get("name").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => return JsonRpcResponse::error(id, INVALID_PARAMS, "'name' parameter is required"),
            };
            let arg = |key: &str| {
                params
                    .get("arguments")
                    .and_then(|a| a.get(key))
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
            };

            let (desc, text): (&str, String) = match name {
                "start-session" => (
                    "Orient a newly started agent",
                    "Please run `knobyte_session_start` to check active workstreams, uncommitted changes, and open relays. Then read `context/stack.md` and `AGENTS.md` before planning next steps.".to_string(),
                ),
                "end-session" => {
                    let mut text = "Please review git status and test results. Update any in-progress steps with `knobyte_workstream_step_update`, and draft a handoff relay using `knobyte_relay_draft` with progress, blockers, and next actions.".to_string();
                    if let Some(summary) = arg("summary") {
                        text.push_str(&format!("\n\nSession summary: {}", summary));
                    }
                    ("Session end wrap-up and relay draft", text)
                }
                "impact-analysis" => {
                    let symbol = match arg("symbol") {
                        Some(s) => s,
                        None => {
                            return JsonRpcResponse::error(id, INVALID_PARAMS, "'symbol' argument is required")
                        }
                    };
                    (
                        "Impact analysis for symbol changes",
                        format!("Please query `knobyte_graph_query` for 'where-defined', 'who-calls', and 'who-imports' on '{}'. Trace implementors, trait callers, and integration points to identify potential breakage.", symbol),
                    )
                }
                _ => return JsonRpcResponse::error(id, INVALID_PARAMS, &format!("Prompt '{}' not found", name)),
            };

            JsonRpcResponse::success(
                id,
                json!({
                    "description": desc,
                    "messages": [ { "role": "user", "content": { "type": "text", "text": text } } ]
                }),
            )
        }
        _ => JsonRpcResponse::error(id, METHOD_NOT_FOUND, &format!("Method not found: {}", req.method)),
    }
}
