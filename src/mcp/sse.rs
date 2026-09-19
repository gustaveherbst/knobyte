use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use futures_util::stream::Stream;
use serde::Deserialize;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

use colored::Colorize;
use crate::mcp::protocol::{CallToolResult, JsonRpcRequest, JsonRpcResponse};
use crate::mcp::tools::{execute_tool, get_tools_list};

type SessionMap = Arc<RwLock<HashMap<String, mpsc::Sender<Event>>>>;

#[derive(Clone)]
pub struct AppState {
    pub sessions: SessionMap,
    pub config: crate::config::KnobyteConfig,
}

#[derive(Debug, Deserialize)]
pub struct MessageQuery {
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
}

pub async fn start_sse_server(host: &str, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let config = crate::config::find_config(None).unwrap_or_else(|_| {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        crate::config::KnobyteConfig::new(cwd.clone(), cwd.join(".knobyte"))
    });

    let state = AppState {
        sessions: Arc::new(RwLock::new(HashMap::new())),
        config,
    };

    let app = Router::new()
        .route("/sse", get(sse_handler))
        .route("/messages", post(messages_handler))
        .route("/mcp", post(messages_handler).get(sse_handler))
        .route("/health", get(health_handler))
        .route("/dashboard", get(dashboard_handler))
        .route("/", get(root_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("{} Knobyte Remote MCP SSE Server listening on http://{}", "[mcp]".cyan().bold(), addr);
    println!("   - Dashboard:        http://{}/dashboard", addr);
    println!("   - SSE Endpoint:     http://{}/sse", addr);
    println!("   - Messages:         http://{}/messages", addr);
    println!("   - Streamable MCP:   http://{}/mcp", addr);
    println!("   - Health check:     http://{}/health", addr);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn dashboard_handler(State(state): State<AppState>) -> impl IntoResponse {
    Html(crate::hub::render_dashboard_html(&state.config))
}

async fn root_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let accept = headers.get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if accept.contains("text/html") {
        return Html(crate::hub::render_dashboard_html(&state.config)).into_response();
    }
    Json(serde_json::json!({
        "product": "knobyte",
        "version": crate::version::VERSION,
        "protocol": "model-context-protocol",
        "transports": ["sse", "streamable-http", "stdio"],
        "endpoints": {
            "dashboard": "/dashboard",
            "sse": "/sse",
            "messages": "/messages",
            "mcp": "/mcp",
            "health": "/health"
        }
    })).into_response()
}

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok", "service": "knobyte-mcp" })))
}

async fn sse_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let session_id = Uuid::new_v4().to_string();
    let (tx, rx) = mpsc::channel::<Event>(100);

    // Initial endpoint event as per MCP specification
    let endpoint_uri = format!("/messages?sessionId={}", session_id);
    let _ = tx.send(Event::default().event("endpoint").data(endpoint_uri)).await;

    {
        let mut sessions = state.sessions.write().await;
        sessions.insert(session_id.clone(), tx);
    }

    let sessions_clone = state.sessions.clone();
    let sid_clone = session_id.clone();

    tokio::spawn(async move {
        // Heartbeat or cleanup
        tokio::time::sleep(Duration::from_secs(3600)).await;
        let mut sessions = sessions_clone.write().await;
        sessions.remove(&sid_clone);
    });

    let stream = ReceiverStream::new(rx).map(Ok);
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

async fn messages_handler(
    State(state): State<AppState>,
    Query(query): Query<MessageQuery>,
    Json(request): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let response = process_jsonrpc_request_with_config(request, &state.config);
    let response_json = serde_json::to_string(&response).unwrap_or_default();

    // If session ID provided and active, send event over SSE as well
    if let Some(sid) = query.session_id {
        let sessions = state.sessions.read().await;
        if let Some(tx) = sessions.get(&sid) {
            let _ = tx.send(Event::default().event("message").data(&response_json)).await;
        }
    }

    (StatusCode::OK, Json(response))
}

pub fn process_jsonrpc_request(req: JsonRpcRequest) -> JsonRpcResponse {
    let config = crate::config::find_config(None).unwrap_or_else(|_| {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        crate::config::KnobyteConfig::new(cwd.clone(), cwd.join(".knobyte"))
    });
    process_jsonrpc_request_with_config(req, &config)
}

pub fn process_jsonrpc_request_with_config(req: JsonRpcRequest, config: &crate::config::KnobyteConfig) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => {
            let instructions = "\
Knobyte Agent Operating Rules:\n\
1. On session start, call `knobyte_session_start` to review open workstreams, current steps, dirty files, and team handoffs.\n\
2. Always read context (`context/stack.md`, `AGENTS.md`, `ROUTER.md`) before writing code.\n\
3. Ground code changes with `knobyte_graph_query` ('where-defined', 'who-calls', 'who-imports') before modifying traits, ports, or shared structs.\n\
4. Checkpoint your work using `knobyte_workstream_step_update` whenever a step status changes or tests pass, so progress survives interruptions.\n\
5. Log key architectural decisions, discovered invariants, and risks with `knobyte_log`.\n\
6. At session end, use `knobyte_relay_draft` to leave a structured handoff for the next session or collaborator.";

            let result = serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {
                        "listChanged": false
                    },
                    "resources": {
                        "subscribe": false,
                        "listChanged": false
                    },
                    "prompts": {
                        "listChanged": false
                    }
                },
                "serverInfo": {
                    "name": "knobyte",
                    "version": crate::version::VERSION
                },
                "instructions": instructions
            });
            JsonRpcResponse::success(req.id, result)
        }
        "notifications/initialized" => {
            JsonRpcResponse::success(req.id, serde_json::json!({}))
        }
        "ping" => {
            JsonRpcResponse::success(req.id, serde_json::json!({}))
        }
        "tools/list" => {
            let tools = get_tools_list();
            let result = serde_json::json!({
                "tools": tools
            });
            JsonRpcResponse::success(req.id, result)
        }
        "tools/call" => {
            let params = req.params.unwrap_or_default();
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let arguments = params.get("arguments").cloned().unwrap_or(serde_json::json!({}));

            let result: CallToolResult = execute_tool(name, &arguments);
            JsonRpcResponse::success(req.id, serde_json::to_value(result).unwrap_or_default())
        }
        "resources/list" => {
            let mut resources = vec![
                serde_json::json!({
                    "uri": "knobyte://context/stack",
                    "name": "Tech Stack Context",
                    "description": "Project tech stack, architecture, and dependencies",
                    "mimeType": "text/markdown"
                }),
                serde_json::json!({
                    "uri": "knobyte://scaffold/AGENTS",
                    "name": "Agent Operating Guidelines",
                    "description": "Rules and conventions for AI agents operating in this repository",
                    "mimeType": "text/markdown"
                }),
                serde_json::json!({
                    "uri": "knobyte://scaffold/ROUTER",
                    "name": "Context Routing Guide",
                    "description": "Repository context routing guidelines",
                    "mimeType": "text/markdown"
                }),
                serde_json::json!({
                    "uri": "knobyte://workstreams/current",
                    "name": "Active Workstreams",
                    "description": "Workstreams, plans, steps, and progress checkpoints",
                    "mimeType": "application/json"
                }),
                serde_json::json!({
                    "uri": "knobyte://log/decisions",
                    "name": "Decisions & Discoveries Log",
                    "description": "Recorded architectural decisions, risks, and discoveries",
                    "mimeType": "application/jsonl"
                }),
            ];

            // List wiki entities
            if let Ok(wiki) = crate::wiki::WikiIndex::open(&config.wiki_db_path()) {
                if let Ok(entities) = wiki.list() {
                    for entity in entities {
                        resources.push(serde_json::json!({
                            "uri": format!("knobyte://wiki/{}", entity.id),
                            "name": entity.title,
                            "description": format!("Wiki entity: {}", entity.entity_type),
                            "mimeType": "text/markdown"
                        }));
                    }
                }
            }

            // List relays
            let relays = crate::team::relay::list_relays(config);
            for r in relays {
                resources.push(serde_json::json!({
                    "uri": format!("knobyte://relay/{}", r.id),
                    "name": r.title,
                    "description": format!("Handoff relay from {}", r.sender),
                    "mimeType": "application/json"
                }));
            }

            JsonRpcResponse::success(req.id, serde_json::json!({ "resources": resources }))
        }
        "resources/templates/list" => {
            let templates = vec![
                serde_json::json!({
                    "uriTemplate": "knobyte://wiki/{id}",
                    "name": "Wiki Entity",
                    "description": "Project documentation, architecture guides, and conventions"
                }),
                serde_json::json!({
                    "uriTemplate": "knobyte://relay/{id}",
                    "name": "Team Relay",
                    "description": "Handoff relay documents for team collaboration"
                }),
            ];
            JsonRpcResponse::success(req.id, serde_json::json!({ "resourceTemplates": templates }))
        }
        "resources/read" => {
            let params = req.params.unwrap_or_default();
            let uri = match params.get("uri").and_then(|v| v.as_str()) {
                Some(u) => u,
                None => return JsonRpcResponse::error(req.id, -32602, "'uri' parameter is required"),
            };

            let (text, mime) = if uri == "knobyte://context/stack" {
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
                if let Ok(wiki) = crate::wiki::WikiIndex::open(&config.wiki_db_path()) {
                    if let Ok(Some(entity)) = wiki.show(wiki_id) {
                        (serde_json::to_string_pretty(&entity).unwrap_or_default(), "application/json")
                    } else {
                        return JsonRpcResponse::error(req.id, -32602, &format!("Wiki entity '{}' not found", wiki_id));
                    }
                } else {
                    return JsonRpcResponse::error(req.id, -32602, "Wiki database not found");
                }
            } else if let Some(relay_id) = uri.strip_prefix("knobyte://relay/") {
                let path = config.relays_dir().join(format!("{}.json", relay_id));
                if path.exists() {
                    (std::fs::read_to_string(&path).unwrap_or_default(), "application/json")
                } else {
                    return JsonRpcResponse::error(req.id, -32602, &format!("Relay '{}' not found", relay_id));
                }
            } else {
                return JsonRpcResponse::error(req.id, -32602, &format!("Unknown resource URI: {}", uri));
            };

            let result = serde_json::json!({
                "contents": [
                    {
                        "uri": uri,
                        "mimeType": mime,
                        "text": text
                    }
                ]
            });
            JsonRpcResponse::success(req.id, result)
        }
        "prompts/list" => {
            let prompts = vec![
                serde_json::json!({
                    "name": "start-session",
                    "description": "Orient a newly started agent: review workstreams, step in progress, dirty files, and open relays",
                    "arguments": []
                }),
                serde_json::json!({
                    "name": "end-session",
                    "description": "Summarize work completed in this session, run tests, and prepare a handoff relay",
                    "arguments": [
                        {
                            "name": "summary",
                            "description": "High-level summary of work performed",
                            "required": false
                        }
                    ]
                }),
                serde_json::json!({
                    "name": "impact-analysis",
                    "description": "Analyze the blast radius of modifying a trait, method, port, or schema",
                    "arguments": [
                        {
                            "name": "symbol",
                            "description": "Symbol or trait name to inspect",
                            "required": true
                        }
                    ]
                }),
            ];
            JsonRpcResponse::success(req.id, serde_json::json!({ "prompts": prompts }))
        }
        "prompts/get" => {
            let params = req.params.unwrap_or_default();
            let name = match params.get("name").and_then(|v| v.as_str()) {
                Some(n) => n,
                None => return JsonRpcResponse::error(req.id, -32602, "'name' parameter is required"),
            };

            let (desc, text): (&str, String) = match name {
                "start-session" => (
                    "Orient a newly started agent",
                    "Please run `knobyte_session_start` to check active workstreams, uncommitted changes, and open relays. Then read `context/stack.md` and `AGENTS.md` before planning next steps.".to_string()
                ),
                "end-session" => (
                    "Session end wrap-up and relay draft",
                    "Please review git status and test results. Update any in-progress steps with `knobyte_workstream_step_update`, and draft a handoff relay using `knobyte_relay_draft` with progress, blockers, and next actions.".to_string()
                ),
                "impact-analysis" => {
                    let symbol = params.get("arguments")
                        .and_then(|a| a.get("symbol"))
                        .and_then(|s| s.as_str())
                        .unwrap_or("target_symbol");
                    (
                        "Impact analysis for symbol changes",
                        format!("Please query `knobyte_graph_query` for 'where-defined', 'who-calls', and 'who-imports' on '{}'. Trace implementors, trait callers, and integration points to identify potential breakage.", symbol)
                    )
                }
                _ => return JsonRpcResponse::error(req.id, -32602, &format!("Prompt '{}' not found", name)),
            };

            let result = serde_json::json!({
                "description": desc,
                "messages": [
                    {
                        "role": "user",
                        "content": {
                            "type": "text",
                            "text": text
                        }
                    }
                ]
            });
            JsonRpcResponse::success(req.id, result)
        }
        _ => JsonRpcResponse::error(req.id, -32601, &format!("Method not found: {}", req.method)),
    }
}
