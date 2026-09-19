use std::fs;
use std::path::Path;
use serde_json::json;

use crate::config::{find_config, KnobyteConfig};
use crate::cozo::CozoEngine;
use crate::drift::checker::run_drift_check;
use crate::events::{append_event, query_timeline, read_events, TimelineFilter};
use crate::graph::GraphEngine;
use crate::heartbeat::check_heartbeat;
use crate::mcp::protocol::{CallToolResult, Tool};
use crate::team::inbox::{save_inbox_draft, InboxDraft};
use crate::team::members::{get_current_member, list_members};
use crate::team::relay::{list_relays, save_relay_draft, RelayDraft};
use crate::wiki::WikiIndex;

pub fn get_tools_list() -> Vec<Tool> {
    vec![
        Tool {
            name: "knobyte_vector_search".to_string(),
            description: "Perform neuro-symbolic HNSW vector similarity search on code nodes or wiki entities using 128-dim dense embeddings.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "projectRoot": { "type": "string" },
                    "query": { "type": "string", "description": "Text query or code snippet to search for." },
                    "target": { "type": "string", "enum": ["code", "wiki"], "default": "code" },
                    "k": { "type": "integer", "default": 10, "description": "Number of nearest neighbors to return." }
                }
            }),
        },
        Tool {
            name: "knobyte_cozo_datalog".to_string(),
            description: "Execute arbitrary CozoScript Datalog query over code graph relations, HNSW vector indices, and wiki entities.\n\
                Stored Relations:\n\
                - code_nodes{id: String => name: String, kind: String, file_path: String, start_line: Int, end_line: Int, is_exported: Bool}\n\
                - code_edges{source_id: String, target_id: String, kind: String => confidence: Float}\n\
                - wiki_entities{id: String => title: String, entity_type: String, file_path: String, summary: String}\n\
                - code_vec{id: String, vector: <F32; 128>}\n\
                - wiki_vec{id: String, vector: <F32; 128>}\n\
                Examples:\n\
                  ?[name, file_path] := *code_nodes{id, name, file_path}, id = '...'\n\
                  ?[source_id, target_id, kind] := *code_edges{source_id, target_id, kind}".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["script"],
                "properties": {
                    "projectRoot": { "type": "string", "description": "Optional path to project root. Defaults to server working directory." },
                    "script": { "type": "string", "description": "CozoScript Datalog query." },
                    "params": { "type": "object", "description": "JSON map of query parameters." }
                }
            }),
        },
        Tool {
            name: "knobyte_cozo_pagerank".to_string(),
            description: "Compute PageRank graph centrality scores across code dependencies in CozoDB.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "projectRoot": { "type": "string" },
                    "theta": { "type": "number", "default": 0.85 },
                    "iterations": { "type": "integer", "default": 20 }
                }
            }),
        },
        Tool {
            name: "knobyte_cozo_shortest_path".to_string(),
            description: "Find shortest path between two code nodes using BFS pathfinding in CozoDB.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["start", "target"],
                "properties": {
                    "projectRoot": { "type": "string" },
                    "start": { "type": "string", "description": "Starting node ID." },
                    "target": { "type": "string", "description": "Target node ID." }
                }
            }),
        },
        Tool {
            name: "knobyte_check".to_string(),
            description: "Run a drift check on the knobyte scaffold. Returns a DriftReport with a numeric score, issues list, and file count.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "projectRoot": { "type": "string", "description": "Path to project root. Defaults to cwd." }
                }
            }),
        },
        Tool {
            name: "knobyte_log".to_string(),
            description: "Append an agent event to the knobyte log, or read recent events. Valid kinds: decision, discovery, note, risk, todo.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "projectRoot": { "type": "string", "description": "Path to project root." },
                    "action": { "type": "string", "enum": ["read", "write"], "default": "read" },
                    "kind": { "type": "string", "enum": ["decision", "discovery", "note", "risk", "todo"] },
                    "summary": { "type": "string", "description": "Human-readable event summary for write." },
                    "limit": { "type": "integer", "default": 20 }
                }
            }),
        },
        Tool {
            name: "knobyte_timeline".to_string(),
            description: "Read historical project notes, optionally filtered by kind, query, or since.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "projectRoot": { "type": "string" },
                    "query": { "type": "string", "description": "Search text in summary, tags, or details." },
                    "kind": { "type": "string", "enum": ["decision", "discovery", "note", "risk", "todo"] },
                    "file": { "type": "string", "description": "Filter by referenced file." },
                    "limit": { "type": "integer", "default": 50 }
                }
            }),
        },
        Tool {
            name: "knobyte_heartbeat".to_string(),
            description: "Check the knobyte scaffold heartbeat. Returns ok status, stale files with age in days, and memory cleanup status.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "projectRoot": { "type": "string" }
                }
            }),
        },
        Tool {
            name: "knobyte_read_file".to_string(),
            description: "Read a file from the knobyte scaffold directory (.knobyte/). Path is relative to scaffold root (e.g. 'AGENTS.md', 'context/stack.md').".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["file"],
                "properties": {
                    "projectRoot": { "type": "string" },
                    "file": { "type": "string", "description": "Path to scaffold file relative to scaffold root." }
                }
            }),
        },
        Tool {
            name: "knobyte_graph_query".to_string(),
            description: "Query structural relationships in the code graph (where-defined, who-calls, who-imports).".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["relation", "target"],
                "properties": {
                    "projectRoot": { "type": "string" },
                    "relation": { "type": "string", "enum": ["where-defined", "who-calls", "who-imports"] },
                    "target": { "type": "string", "description": "Symbol name, function name, or module path." }
                }
            }),
        },
        Tool {
            name: "knobyte_graph_scope".to_string(),
            description: "Retrieve relevant symbols, definitions, and code neighborhood for a given task description.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["task"],
                "properties": {
                    "projectRoot": { "type": "string" },
                    "task": { "type": "string", "description": "Natural language task or query description." }
                }
            }),
        },
        Tool {
            name: "knobyte_graph_get".to_string(),
            description: "Get node source definitions and metadata by node IDs.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["ids"],
                "properties": {
                    "projectRoot": { "type": "string" },
                    "ids": { "type": "array", "items": { "type": "string" } }
                }
            }),
        },
        Tool {
            name: "knobyte_graph_status".to_string(),
            description: "Get current status, node count, and edge count of the code graph index.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "projectRoot": { "type": "string" }
                }
            }),
        },
        Tool {
            name: "knobyte_wiki_query".to_string(),
            description: "Full-text search on Wiki entities, architecture, conventions, and documentation.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["text"],
                "properties": {
                    "projectRoot": { "type": "string" },
                    "text": { "type": "string", "description": "Search query keywords." }
                }
            }),
        },
        Tool {
            name: "knobyte_wiki_show".to_string(),
            description: "Get full details of a specific Wiki entity by ID (e.g. 'kb_stack').".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "projectRoot": { "type": "string" },
                    "id": { "type": "string", "description": "Entity ID." }
                }
            }),
        },
        Tool {
            name: "knobyte_wiki_list".to_string(),
            description: "List all indexed Wiki entities in the project.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "projectRoot": { "type": "string" }
                }
            }),
        },
        Tool {
            name: "knobyte_relay_list".to_string(),
            description: "List active team handoffs and relays.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "projectRoot": { "type": "string" }
                }
            }),
        },
        Tool {
            name: "knobyte_relay_draft".to_string(),
            description: "Save a local draft of a handoff relay.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["title", "summary", "sender"],
                "properties": {
                    "projectRoot": { "type": "string" },
                    "title": { "type": "string" },
                    "summary": { "type": "string" },
                    "sender": { "type": "string" },
                    "progress": { "type": "array", "items": { "type": "string" } },
                    "blockers": { "type": "array", "items": { "type": "string" } },
                    "nextActions": { "type": "array", "items": { "type": "string" } }
                }
            }),
        },
        Tool {
            name: "knobyte_inbox_draft".to_string(),
            description: "Save an inbox draft proposing a knowledge addition or correction.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["title", "target", "content", "reason", "author"],
                "properties": {
                    "projectRoot": { "type": "string" },
                    "title": { "type": "string" },
                    "target": { "type": "string" },
                    "content": { "type": "string" },
                    "reason": { "type": "string" },
                    "author": { "type": "string" }
                }
            }),
        },
        Tool {
            name: "knobyte_member_list".to_string(),
            description: "List registered team members for attribution.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "projectRoot": { "type": "string" }
                }
            }),
        },
        Tool {
            name: "knobyte_member_current".to_string(),
            description: "Show the effective local team member identity.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "projectRoot": { "type": "string" }
                }
            }),
        },
        Tool {
            name: "knobyte_sync_groundings".to_string(),
            description: "Automatically relocate and heal drifted grounding anchors in scaffold files when code symbols move across files.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "projectRoot": { "type": "string", "description": "Optional path to project root. Defaults to server working directory." },
                    "dryRun": { "type": "boolean", "default": false, "description": "Preview relocations without modifying files." }
                }
            }),
        },
        Tool {
            name: "knobyte_session_start".to_string(),
            description: "Unified entry point for newly started or resuming agents. Returns project stack, current member identity, open workstreams and steps, uncommitted dirty files, latest relay handoffs, and recent decisions/risks.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "projectRoot": { "type": "string", "description": "Optional path to project root. Defaults to server working directory." }
                }
            }),
        },
        Tool {
            name: "knobyte_workstream_step_update".to_string(),
            description: "Update the status, evidence, and checkpoints of a workstream step, automatically stamped with git HEAD and dirty files.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["status"],
                "properties": {
                    "projectRoot": { "type": "string", "description": "Optional path to project root. Defaults to server working directory." },
                    "workstreamId": { "type": "string", "description": "ID of the workstream. Defaults to the first active workstream if omitted." },
                    "stepIndex": { "type": "integer", "description": "0-based index of the step within the workstream." },
                    "stepId": { "type": "string", "description": "Optional step ID or name." },
                    "status": { "type": "string", "enum": ["pending", "in_progress", "done", "blocked", "committed"], "description": "New step status." },
                    "evidence": { "type": "string", "description": "Evidence of completion (e.g. test results, command output)." },
                    "filesTouched": { "type": "array", "items": { "type": "string" }, "description": "Files modified as part of this step." }
                }
            }),
        },
        Tool {
            name: "knobyte_file_context".to_string(),
            description: "Get contextual memory for a specific file: related architectural decisions, risks, discoveries, and graph symbols defined in or calling this file.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["filePath"],
                "properties": {
                    "projectRoot": { "type": "string", "description": "Optional path to project root. Defaults to server working directory." },
                    "filePath": { "type": "string", "description": "Relative or absolute path to the file." }
                }
            }),
        },
        Tool {
            name: "knobyte_harvest".to_string(),
            description: "Harvest architectural decisions, commits, ADRs, and discoveries from git history and documentation to seed agent memory.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "projectRoot": { "type": "string", "description": "Optional path to project root. Defaults to server working directory." },
                    "limit": { "type": "integer", "default": 20, "description": "Max number of git commits to harvest." }
                }
            }),
        },
    ]
}

pub fn execute_tool(name: &str, args: &serde_json::Value) -> CallToolResult {
    let project_root_arg = args.get("projectRoot").and_then(|v| v.as_str()).map(Path::new);
    let config = match find_config(project_root_arg) {
        Ok(c) => c,
        Err(e) => return CallToolResult::error(&format!("Config error: {}", e)),
    };

    match name {
        "knobyte_check" => {
            let fix = args.get("fix").and_then(|v| v.as_bool()).unwrap_or(false);
            if fix {
                let _ = crate::drift::sync_groundings(&config, false);
            }
            let report = run_drift_check(&config);
            CallToolResult::text(&serde_json::to_string_pretty(&report).unwrap_or_default())
        }
        "knobyte_sync_groundings" => {
            let dry_run = args.get("dryRun").and_then(|v| v.as_bool()).unwrap_or(false);
            match crate::drift::sync_groundings(&config, dry_run) {
                Ok(res) => CallToolResult::text(&serde_json::to_string_pretty(&res).unwrap_or_default()),
                Err(e) => CallToolResult::error(&format!("Failed to sync groundings: {}", e)),
            }
        }
        "knobyte_log" => {
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("read");
            if action == "write" {
                let summary = match args.get("summary").and_then(|v| v.as_str()) {
                    Some(s) => s,
                    None => return CallToolResult::error("summary is required for write action"),
                };
                let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("note");
                match append_event(&config, summary, kind, &[], &[], None) {
                    Ok(entry) => CallToolResult::text(&serde_json::to_string_pretty(&entry).unwrap_or_default()),
                    Err(e) => CallToolResult::error(&format!("Failed to write event: {}", e)),
                }
            } else {
                let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let events = read_events(&config);
                let recent: Vec<_> = events.into_iter().rev().take(limit).collect();
                CallToolResult::text(&serde_json::to_string_pretty(&recent).unwrap_or_default())
            }
        }
        "knobyte_timeline" => {
            let query = args.get("query").and_then(|v| v.as_str()).map(|s| s.to_string());
            let kind = args.get("kind").and_then(|v| v.as_str()).map(|s| s.to_string());
            let file = args.get("file").and_then(|v| v.as_str()).map(|s| s.to_string());
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as usize;

            let filter = TimelineFilter {
                query,
                kind,
                file,
                since: None,
                include_superseded: false,
                limit,
            };
            let resp = query_timeline(&config, filter);
            CallToolResult::text(&serde_json::to_string_pretty(&resp).unwrap_or_default())
        }
        "knobyte_heartbeat" => {
            let report = check_heartbeat(&config, 14);
            CallToolResult::text(&serde_json::to_string_pretty(&report).unwrap_or_default())
        }
        "knobyte_read_file" => {
            let rel_file = match args.get("file").and_then(|v| v.as_str()) {
                Some(f) => f,
                None => return CallToolResult::error("'file' argument is required"),
            };

            let base = &config.scaffold_root;
            let full_path = base.join(rel_file);

            if !full_path.starts_with(base) {
                return CallToolResult::error("Path escapes scaffold root");
            }

            if !full_path.exists() {
                return CallToolResult::error(&format!("File not found: {}", rel_file));
            }

            match fs::read_to_string(&full_path) {
                Ok(content) => CallToolResult::text(&content),
                Err(e) => CallToolResult::error(&format!("Failed to read file: {}", e)),
            }
        }
        "knobyte_graph_query" => {
            let relation = match args.get("relation").and_then(|v| v.as_str()) {
                Some(r) => r,
                None => return CallToolResult::error("'relation' argument is required"),
            };
            let target = match args.get("target").and_then(|v| v.as_str()) {
                Some(t) => t,
                None => return CallToolResult::error("'target' argument is required"),
            };

            let engine = match GraphEngine::open(&config.graph_db_path()) {
                Ok(e) => e,
                Err(e) => return CallToolResult::error(&format!("Failed to open graph.db: {}", e)),
            };

            let nodes = match relation {
                "where-defined" => engine.query_where_defined(target),
                "who-calls" => engine.query_who_calls(target),
                "who-imports" => engine.query_who_imports(target),
                _ => return CallToolResult::error(&format!("Unknown relation: {}", relation)),
            };

            let freshness = get_freshness_metadata(&config);
            match nodes {
                Ok(n) => CallToolResult::text(&serde_json::to_string_pretty(&json!({
                    "results": n,
                    "freshness": freshness
                })).unwrap_or_default()),
                Err(e) => CallToolResult::error(&format!("Query failed: {}", e)),
            }
        }
        "knobyte_graph_scope" => {
            let task = match args.get("task").and_then(|v| v.as_str()) {
                Some(t) => t,
                None => return CallToolResult::error("'task' argument is required"),
            };

            let engine = match GraphEngine::open(&config.graph_db_path()) {
                Ok(e) => e,
                Err(e) => return CallToolResult::error(&format!("Failed to open graph.db: {}", e)),
            };

            let freshness = get_freshness_metadata(&config);
            match engine.query_scope_explained(task) {
                Ok(nodes) => CallToolResult::text(&serde_json::to_string_pretty(&json!({
                    "results": nodes,
                    "freshness": freshness
                })).unwrap_or_default()),
                Err(e) => CallToolResult::error(&format!("Scope query failed: {}", e)),
            }
        }
        "knobyte_graph_get" => {
            let ids: Vec<String> = args.get("ids")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .or_else(|| args.get("id").and_then(|v| v.as_str()).map(|s| vec![s.to_string()]))
                .unwrap_or_default();

            let engine = match GraphEngine::open(&config.graph_db_path()) {
                Ok(e) => e,
                Err(e) => return CallToolResult::error(&format!("Failed to open graph.db: {}", e)),
            };

            match engine.get_nodes(&ids) {
                Ok(nodes) => CallToolResult::text(&serde_json::to_string_pretty(&nodes).unwrap_or_default()),
                Err(e) => CallToolResult::error(&format!("Get nodes failed: {}", e)),
            }
        }
        "knobyte_graph_status" => {
            let engine = match GraphEngine::open(&config.graph_db_path()) {
                Ok(e) => e,
                Err(e) => return CallToolResult::error(&format!("Failed to open graph.db: {}", e)),
            };

            let freshness = get_freshness_metadata(&config);
            match engine.status() {
                Ok(status) => CallToolResult::text(&serde_json::to_string_pretty(&json!({
                    "status": status,
                    "freshness": freshness
                })).unwrap_or_default()),
                Err(e) => CallToolResult::error(&format!("Status failed: {}", e)),
            }
        }
        "knobyte_wiki_query" => {
            let text = match args.get("text").or_else(|| args.get("query")).and_then(|v| v.as_str()) {
                Some(t) => t,
                None => return CallToolResult::error("'text' argument is required"),
            };

            let index = match WikiIndex::open(&config.wiki_db_path()) {
                Ok(i) => i,
                Err(e) => return CallToolResult::error(&format!("Failed to open wiki.db: {}", e)),
            };

            match index.query(text) {
                Ok(entities) => CallToolResult::text(&serde_json::to_string_pretty(&entities).unwrap_or_default()),
                Err(e) => CallToolResult::error(&format!("Wiki query failed: {}", e)),
            }
        }
        "knobyte_wiki_show" => {
            let id = match args.get("id").and_then(|v| v.as_str()) {
                Some(i) => i,
                None => return CallToolResult::error("'id' argument is required"),
            };

            let index = match WikiIndex::open(&config.wiki_db_path()) {
                Ok(i) => i,
                Err(e) => return CallToolResult::error(&format!("Failed to open wiki.db: {}", e)),
            };

            match index.show(id) {
                Ok(Some(entity)) => CallToolResult::text(&serde_json::to_string_pretty(&entity).unwrap_or_default()),
                Ok(None) => CallToolResult::error(&format!("Wiki entity '{}' not found", id)),
                Err(e) => CallToolResult::error(&format!("Wiki show failed: {}", e)),
            }
        }
        "knobyte_wiki_list" => {
            let index = match WikiIndex::open(&config.wiki_db_path()) {
                Ok(i) => i,
                Err(e) => return CallToolResult::error(&format!("Failed to open wiki.db: {}", e)),
            };

            match index.list() {
                Ok(entities) => CallToolResult::text(&serde_json::to_string_pretty(&entities).unwrap_or_default()),
                Err(e) => CallToolResult::error(&format!("Wiki list failed: {}", e)),
            }
        }
        "knobyte_relay_list" => {
            let relays = list_relays(&config);
            CallToolResult::text(&serde_json::to_string_pretty(&relays).unwrap_or_default())
        }
        "knobyte_relay_draft" => {
            let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled Relay");
            let summary = args.get("summary").and_then(|v| v.as_str()).unwrap_or("");
            let sender = args.get("sender").and_then(|v| v.as_str()).unwrap_or("agent");

            let progress = parse_string_array(args.get("progress"));
            let blockers = parse_string_array(args.get("blockers"));
            let next_actions = parse_string_array(args.get("nextActions"));

            let draft = RelayDraft {
                id: format!("draft_{}", uuid::Uuid::new_v4()),
                title: title.to_string(),
                summary: summary.to_string(),
                sender: sender.to_string(),
                open_to_team: true,
                named_recipients: Vec::new(),
                progress,
                blockers,
                next_actions,
                evidence: Vec::new(),
                created_at: chrono::Utc::now().to_rfc3339(),
            };

            match save_relay_draft(&config, &draft) {
                Ok(_) => CallToolResult::text(&serde_json::to_string_pretty(&draft).unwrap_or_default()),
                Err(e) => CallToolResult::error(&format!("Failed to save draft: {}", e)),
            }
        }
        "knobyte_inbox_draft" => {
            let title = args.get("title").and_then(|v| v.as_str()).unwrap_or("Proposal");
            let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("general");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let reason = args.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            let author = args.get("author").and_then(|v| v.as_str()).unwrap_or("agent");

            let draft = InboxDraft {
                id: format!("draft_{}", uuid::Uuid::new_v4()),
                target: target.to_string(),
                title: title.to_string(),
                proposed_content: content.to_string(),
                reason: reason.to_string(),
                author: author.to_string(),
                created_at: chrono::Utc::now().to_rfc3339(),
            };

            match save_inbox_draft(&config, &draft) {
                Ok(_) => CallToolResult::text(&serde_json::to_string_pretty(&draft).unwrap_or_default()),
                Err(e) => CallToolResult::error(&format!("Failed to save inbox draft: {}", e)),
            }
        }
        "knobyte_member_list" => {
            let members = list_members(&config);
            CallToolResult::text(&serde_json::to_string_pretty(&members).unwrap_or_default())
        }
        "knobyte_member_current" => {
            match get_current_member(&config) {
                Some(m) => CallToolResult::text(&serde_json::to_string_pretty(&m).unwrap_or_default()),
                None => CallToolResult::text("null"),
            }
        }
        "knobyte_vector_search" => {
            let query = match args.get("query").and_then(|v| v.as_str()) {
                Some(q) => q,
                None => return CallToolResult::error("query is required"),
            };
            let target = args.get("target").and_then(|v| v.as_str()).unwrap_or("code");
            let k = args.get("k").and_then(|v| v.as_u64()).unwrap_or(10) as usize;

            let freshness = get_freshness_metadata(&config);
            match get_cozo_engine(&config) {
                Ok(engine) => match engine.vector_search(query, target, k) {
                    Ok(matches) => {
                        if matches.is_empty() {
                            CallToolResult::text(&serde_json::to_string_pretty(&json!({
                                "matches": [],
                                "message": "No matches exceeded relevance threshold (score >= 0.20)",
                                "freshness": freshness
                            })).unwrap_or_default())
                        } else {
                            CallToolResult::text(&serde_json::to_string_pretty(&json!({
                                "matches": matches,
                                "freshness": freshness
                            })).unwrap_or_default())
                        }
                    }
                    Err(e) => CallToolResult::error(&format!("Vector search failed: {}", e)),
                },
                Err(e) => CallToolResult::error(&e),
            }
        }
        "knobyte_cozo_datalog" => {
            let script = match args.get("script").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => return CallToolResult::error("script is required"),
            };
            let params = args.get("params").cloned().unwrap_or_else(|| serde_json::json!({}));

            match get_cozo_engine(&config) {
                Ok(engine) => match engine.datalog_query(script, params) {
                    Ok(res) => CallToolResult::text(&serde_json::to_string_pretty(&res).unwrap_or_default()),
                    Err(e) => CallToolResult::error(&format!("Datalog query failed: {}", e)),
                },
                Err(e) => CallToolResult::error(&e),
            }
        }
        "knobyte_cozo_pagerank" => {
            let theta = args.get("theta").and_then(|v| v.as_f64());
            let iterations = args.get("iterations").and_then(|v| v.as_u64()).map(|v| v as usize);

            match get_cozo_engine(&config) {
                Ok(engine) => match engine.pagerank(theta, iterations) {
                    Ok(ranks) => CallToolResult::text(&serde_json::to_string_pretty(&ranks).unwrap_or_default()),
                    Err(e) => CallToolResult::error(&format!("PageRank failed: {}", e)),
                },
                Err(e) => CallToolResult::error(&e),
            }
        }
        "knobyte_cozo_shortest_path" => {
            let start = match args.get("start").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => return CallToolResult::error("start is required"),
            };
            let target = match args.get("target").and_then(|v| v.as_str()) {
                Some(t) => t,
                None => return CallToolResult::error("target is required"),
            };

            match get_cozo_engine(&config) {
                Ok(engine) => match engine.shortest_path_detailed(start, target) {
                    Ok(Some(path)) => CallToolResult::text(&serde_json::to_string_pretty(&path).unwrap_or_default()),
                    Ok(None) => CallToolResult::text("null"),
                    Err(e) => CallToolResult::error(&format!("Shortest path failed: {}", e)),
                },
                Err(e) => CallToolResult::error(&e),
            }
        }
        "knobyte_session_start" => {
            let (head, dirty_files) = crate::team::workstreams::get_git_state(&config.project_root);
            let member = get_current_member(&config);
            let workstreams = crate::team::workstreams::list_workstreams(&config);
            let current_step = workstreams.iter()
                .flat_map(|w| w.steps.iter().map(move |s| (w.id.clone(), s)))
                .find(|(_, s)| s.status == "in_progress");
            let last_relay = list_relays(&config).into_iter().last();
            let events = read_events(&config);
            let recent_decisions: Vec<_> = events.iter()
                .rev()
                .filter(|e| e.kind == "decision" || e.kind == "risk")
                .take(5)
                .cloned()
                .collect();
            let heartbeat = check_heartbeat(&config, 14);

            let result = json!({
                "project": {
                    "name": config.project_name(),
                    "mode": config.mode,
                    "root": config.project_root.display().to_string(),
                },
                "member": member,
                "git_state": {
                    "head": head,
                    "dirty_files_count": dirty_files.len(),
                    "dirty_files": dirty_files,
                },
                "workstreams": workstreams,
                "current_step": current_step.map(|(ws_id, step)| json!({ "workstream_id": ws_id, "step": step })),
                "last_relay": last_relay,
                "recent_decisions_and_risks": recent_decisions,
                "heartbeat": heartbeat,
            });
            CallToolResult::text(&serde_json::to_string_pretty(&result).unwrap_or_default())
        }
        "knobyte_workstream_step_update" => {
            let status = match args.get("status").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => return CallToolResult::error("'status' argument is required"),
            };
            let evidence = args.get("evidence").and_then(|v| v.as_str());
            let files_touched = parse_string_array(args.get("filesTouched"));
            let files_slice = if files_touched.is_empty() { None } else { Some(files_touched.as_slice()) };

            let ws_list = crate::team::workstreams::list_workstreams(&config);
            let ws_id = match args.get("workstreamId").and_then(|v| v.as_str()) {
                Some(id) => id.to_string(),
                None => {
                    if let Some(first) = ws_list.first() {
                        first.id.clone()
                    } else {
                        let default_ws = crate::team::workstreams::Workstream {
                            id: "ws_default".to_string(),
                            title: "Active Workstream".to_string(),
                            description: Some("Current engineering workstream".to_string()),
                            status: "active".to_string(),
                            owner: get_current_member(&config).map(|m| m.display_name).or_else(|| Some("agent".to_string())),
                            created_at: chrono::Utc::now().to_rfc3339(),
                            updated_at: chrono::Utc::now().to_rfc3339(),
                            steps: Vec::new(),
                            checkpoints: Vec::new(),
                        };
                        let _ = crate::team::workstreams::save_workstream(&config, &default_ws);
                        "ws_default".to_string()
                    }
                }
            };

            let step_id = if let Some(sid) = args.get("stepId").and_then(|v| v.as_str()) {
                sid.to_string()
            } else if let Some(idx) = args.get("stepIndex").and_then(|v| v.as_u64()) {
                format!("step_{}", idx)
            } else {
                "step_1".to_string()
            };

            match crate::team::workstreams::update_workstream_step(
                &config,
                &ws_id,
                &step_id,
                status,
                evidence,
                files_slice,
            ) {
                Ok(ws) => CallToolResult::text(&serde_json::to_string_pretty(&ws).unwrap_or_default()),
                Err(e) => CallToolResult::error(&format!("Failed to update step: {}", e)),
            }
        }
        "knobyte_file_context" => {
            let file_path = match args.get("filePath").or_else(|| args.get("file")).and_then(|v| v.as_str()) {
                Some(f) => f,
                None => return CallToolResult::error("'filePath' argument is required"),
            };

            let events = read_events(&config);
            let matching_events: Vec<_> = events.into_iter().filter(|e| {
                e.files.iter().any(|f| f.contains(file_path) || file_path.contains(f))
                    || e.summary.contains(file_path)
            }).collect();

            let mut defined_symbols = Vec::new();
            if let Ok(conn) = rusqlite::Connection::open(config.graph_db_path()) {
                let pattern = format!("%{}", file_path);
                if let Ok(mut stmt) = conn.prepare("SELECT id, name, kind, start_line, end_line, is_exported FROM nodes WHERE file_path LIKE ? ORDER BY start_line") {
                    if let Ok(rows) = stmt.query_map([pattern], |r| {
                        Ok(json!({
                            "id": r.get::<_, String>(0)?,
                            "name": r.get::<_, String>(1)?,
                            "kind": r.get::<_, String>(2)?,
                            "start_line": r.get::<_, i64>(3)?,
                            "end_line": r.get::<_, i64>(4)?,
                            "is_exported": r.get::<_, bool>(5)?,
                        }))
                    }) {
                        for row in rows.flatten() {
                            defined_symbols.push(row);
                        }
                    }
                }
            }

            let result = json!({
                "file_path": file_path,
                "recorded_events": matching_events,
                "symbols_defined": defined_symbols,
            });
            CallToolResult::text(&serde_json::to_string_pretty(&result).unwrap_or_default())
        }
        "knobyte_harvest" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            let report = crate::harvest::harvest_all(&config, limit);
            CallToolResult::text(&serde_json::to_string_pretty(&report).unwrap_or_default())
        }
        _ => CallToolResult::error(&format!("Unknown tool: {}", name)),
    }
}

fn get_cozo_engine(config: &KnobyteConfig) -> Result<CozoEngine, String> {
    let db_path = config.cozo_db_path();
    let is_new = !db_path.exists();
    let engine = CozoEngine::open(&db_path).map_err(|e| format!("Failed to open CozoDB: {}", e))?;
    if is_new {
        if let Ok(graph_conn) = rusqlite::Connection::open(config.graph_db_path()) {
            let _ = engine.sync_from_graph(&graph_conn);
        }
        if let Ok(wiki_conn) = rusqlite::Connection::open(config.wiki_db_path()) {
            let _ = engine.sync_from_wiki(&wiki_conn);
        }
    }
    Ok(engine)
}

fn parse_string_array(val: Option<&serde_json::Value>) -> Vec<String> {
    val.and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default()
}

fn get_freshness_metadata(config: &KnobyteConfig) -> serde_json::Value {
    let (head, dirty_files) = crate::team::workstreams::get_git_state(&config.project_root);
    let last_indexed = if let Ok(engine) = GraphEngine::open(&config.graph_db_path()) {
        engine.status().ok().and_then(|s| s.last_indexed)
    } else {
        None
    };
    let dirty_count = dirty_files.len();
    json!({
        "is_current": dirty_count == 0,
        "last_indexed": last_indexed,
        "head_commit": head,
        "dirty_files_count": dirty_count,
        "stale_warning": if dirty_count > 0 {
            Some(format!("{} modified or uncommitted file(s) in working tree may not be reflected in indexed results", dirty_count))
        } else {
            None
        }
    })
}
