use std::fs;

use serde_json::{json, Value};

use crate::config::KnobyteConfig;
use crate::cozo::CozoEngine;
use crate::drift::checker::run_drift_check;
use crate::events::{
    append_event_full, bound_timeline_output, clamp_timeline_limit, query_timeline, read_events, EventEntry, TimelineFilter,
    EVENT_KINDS, MAX_TIMELINE_LIMIT, TIMELINE_OMITTED_NOTE,
};
use crate::graph::GraphEngine;
use crate::heartbeat::check_heartbeat;
use crate::mcp::profiles::McpProfile;
use crate::mcp::protocol::{CallToolResult, Tool};
use crate::mcp::security::{check_project_root_arg, resolve_confined_path, validate_id};
use crate::team::identity::resolve_actor;
use crate::team::members::{get_current_member, get_member, list_members};
use crate::team::refs::parse_evidence;
use crate::team::relay::list_relays;
use crate::team::workflow::{run_action, ActorChoice};
use crate::team::TeamError;
use crate::wiki::WikiIndex;

/// Default age (days) after which scaffold files are reported as stale.
pub const DEFAULT_STALE_THRESHOLD_DAYS: u64 = 14;

/// Workstream step statuses accepted by `knobyte_workstream_step_update`.
pub const STEP_STATUSES: &[&str] = crate::team::workstreams::STEP_STATUSES;

fn tool(name: &str, description: &str, input_schema: Value) -> Tool {
    Tool {
        name: name.to_string(),
        description: description.to_string(),
        input_schema,
    }
}

fn string_array_schema(description: &str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": description })
}

/// Agent protocol v3 budget/detail properties shared by the graph tools. Supplying any of them
/// switches the tool to protocol v3 records (meta first, summary last) under a hard token budget.
/// `scope` adds the scope-only bounds (`max_files`, `max_flow_steps`).
fn agent_budget_properties(props: &mut serde_json::Map<String, Value>, scope: bool) {
    props.insert("detail".into(), json!({ "type": "string", "enum": ["minimal", "standard", "source"], "description": "Detail level. Any detail/max_* option returns budgeted v3 records." }));
    props.insert("max_nodes".into(), json!({ "type": "integer", "minimum": 0 }));
    if scope {
        props.insert("max_files".into(), json!({ "type": "integer", "minimum": 0 }));
        props.insert("max_flow_steps".into(), json!({ "type": "integer", "minimum": 0 }));
    }
    props.insert("max_output_tokens".into(), json!({ "type": "integer", "minimum": 1, "description": "Estimated output token ceiling." }));
    props.insert("max_source_lines".into(), json!({ "type": "integer", "minimum": 1, "description": "Per-node source line cap." }));
    props.insert("fingerprint".into(), json!({ "type": "boolean", "description": "Attach body hashes and MinHash fingerprints." }));
}

fn with_budget(mut schema: Value, scope: bool) -> Value {
    if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
        agent_budget_properties(props, scope);
    }
    schema
}

/// Protocol v3 options from MCP arguments; `None` when no budget/detail argument was given.
fn agent_input(args: &Value) -> Result<Option<crate::graph::protocol::AgentOptionsInput>, String> {
    let num = |k: &str| args.get(k).and_then(|v| v.as_u64()).map(|n| n as usize);
    let detail = match args.get("detail").and_then(|v| v.as_str()) {
        Some(d) => Some(
            crate::graph::protocol::DetailLevel::parse(d)
                .ok_or_else(|| format!("Unknown detail '{}'. Use minimal, standard or source.", d))?,
        ),
        None => None,
    };
    let input = crate::graph::protocol::AgentOptionsInput {
        detail,
        max_nodes: num("max_nodes"),
        max_files: num("max_files"),
        max_flow_steps: num("max_flow_steps"),
        max_output_tokens: num("max_output_tokens"),
        max_source_lines: num("max_source_lines"),
        depth: None,
        fingerprint: args.get("fingerprint").and_then(|v| v.as_bool()).unwrap_or(false),
    };
    Ok(input.any_set().then_some(input))
}

/// Read-only Datalog reference served as the `knobyte://reference/datalog-schema` resource.
pub const DATALOG_REFERENCE_URI: &str = "knobyte://reference/datalog-schema";

/// Markdown body of [`DATALOG_REFERENCE_URI`]: the stored relations, indices and examples.
pub const DATALOG_REFERENCE: &str = "\
# Knobyte CozoScript reference

`knobyte_cozo_datalog` runs read-only CozoScript. Mutations (`:put`, `:rm`, `:create`, ...) are refused.

## Stored relations

- `code_nodes{id: String => file_path: String, kind: String, name: String, start_line: Int, end_line: Int, body_hash: String, embedding: <F32; D>, qualified_name: String}`
- `code_edges{source_id: String, target_id: String, kind: String => file_path: String}`
  - kind: calls, calls_trait_method, possible_call, instantiates, imports, implements, impl_of, extends, overrides, returns, type_of, decorates, references, contains, exports (contains/exports are structural, not dependencies)
- `wiki_entities{id: String => title: String, path: String, tags: [String], summary: String, embedding: <F32; D>}`
- `embedding_meta{relation: String => embedder_id: String, dim: Int}` (which embedder built each vector relation)

## HNSW indices (cosine)

`code_nodes:node_vec` and `wiki_entities:wiki_vec` over `embedding`. D depends on the embedding backend (128 for `hashed`, the model dimension for `model2vec`, e.g. 256); read it from `embedding_meta`. For text queries use `knobyte_vector_search`, which embeds the query with the same backend.

## Examples

```
?[name, kind, file_path, start_line] := *code_nodes{name, kind, file_path, start_line, qualified_name}, qualified_name == 'CozoEngine::open'
?[caller, callee] := *code_edges{source_id: s, target_id: t, kind: 'calls'}, *code_nodes{id: s, name: caller}, *code_nodes{id: t, name: callee}
?[relation, embedder_id, dim] := *embedding_meta{relation, embedder_id, dim}
```
";

/// Every tool exposed by the server, in `tools/list` order (see
/// [`crate::mcp::profiles::TOOL_PROFILES`]). Every tool operates on the project the server was
/// started in; there is intentionally no tool that approves or rejects inbox proposals,
/// publishes relays, or publishes/archives playbooks (human-only actions).
pub fn get_tools_list() -> Vec<Tool> {
    let mut defs = tool_definitions();
    let mut out = Vec::with_capacity(defs.len());
    for (name, _) in crate::mcp::profiles::TOOL_PROFILES {
        if let Some(pos) = defs.iter().position(|t| t.name == *name) {
            out.push(defs.remove(pos));
        }
    }
    out.extend(defs);
    out
}

/// The tools listed under `profile`, in `tools/list` order.
pub fn tools_for_profile(profile: McpProfile) -> Vec<Tool> {
    get_tools_list().into_iter().filter(|t| profile.includes(&t.name)).collect()
}

/// Whether `name` (a tool or a retired alias) can be called at all.
pub fn is_known_tool(name: &str) -> bool {
    crate::mcp::profiles::alias(name).is_some() || crate::mcp::profiles::TOOL_PROFILES.iter().any(|(t, _)| *t == name)
}

fn tool_definitions() -> Vec<Tool> {
    let kinds: Vec<&str> = EVENT_KINDS.to_vec();
    let evidence = "Evidence: file:<path>, commit:<sha>, entity:<id>, external:<uri>, or free text.";
    vec![
        tool(
            "knobyte_vector_search",
            "Similarity search over code symbols or wiki entities with the project's local embeddings (nothing leaves the machine). Returns up to k ranked matches with readable refs, the embedder used and index freshness. Read-only.",
            json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "Text query or code snippet." },
                    "target": { "type": "string", "enum": ["code", "wiki"], "default": "code", "description": "Which index to search." },
                    "k": { "type": "integer", "minimum": 1, "default": 10, "description": "Number of matches to return." },
                    "minScore": { "type": "number", "minimum": 0, "maximum": 1, "default": 0.2, "description": "Relevance floor (1 - cosine distance); belowFloor counts candidates it left out. 0 disables it." }
                }
            }),
        ),
        tool(
            "knobyte_cozo_datalog",
            "Run a read-only CozoScript Datalog query over the code graph, vector indices and wiki relations. Relation schemas and examples: resource knobyte://reference/datalog-schema.",
            json!({
                "type": "object",
                "required": ["script"],
                "properties": {
                    "script": { "type": "string", "description": "Read-only CozoScript (mutations are refused). Relations: code_nodes, code_edges, wiki_entities, embedding_meta." },
                    "params": { "type": "object", "description": "Named query parameters ($name in the script)." }
                }
            }),
        ),
        tool(
            "knobyte_cozo_pagerank",
            "Rank code symbols by PageRank centrality over dependency edges. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "theta": { "type": "number", "default": 0.85, "description": "Damping factor." },
                    "iterations": { "type": "integer", "minimum": 1, "default": 20, "description": "Number of iterations." }
                }
            }),
        ),
        tool(
            "knobyte_cozo_shortest_path",
            "Shortest dependency path between two code symbols (null when none exists). Read-only.",
            json!({
                "type": "object",
                "required": ["start", "target"],
                "properties": {
                    "start": { "type": "string", "description": "Node ID, symbol name or readable ref (kind:path:qualified_name); ambiguous names list candidates." },
                    "target": { "type": "string", "description": "Node ID, symbol name or readable ref." }
                }
            }),
        ),
        tool(
            "knobyte_check",
            "Run a drift check on the scaffold and return the report (score, issues, file count). fix=true first relocates drifted grounding anchors (dryRun previews them).",
            json!({
                "type": "object",
                "properties": {
                    "fix": { "type": "boolean", "default": false, "description": "Heal drifted grounding anchors before checking (writes scaffold files)." },
                    "dryRun": { "type": "boolean", "default": false, "description": "With fix: only preview the relocations; nothing is written." }
                }
            }),
        ),
        tool(
            "knobyte_log",
            "Read recent project events, or append one (decision, discovery, note, risk, todo) as the current actor.",
            json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["read", "write"], "default": "read" },
                    "kind": { "type": "string", "enum": kinds, "default": "note" },
                    "summary": { "type": "string", "description": "Event summary (required for write)." },
                    "details": { "type": "string" },
                    "tags": string_array_schema("Tags."),
                    "files": string_array_schema("Repository files the event refers to."),
                    "actor": { "type": "string", "description": "Optional check: must equal the current actor." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_TIMELINE_LIMIT, "default": 20, "description": "Events to read (output capped at 64 KiB)." }
                }
            }),
        ),
        tool(
            "knobyte_timeline",
            "Search historical project events by text, kind, referenced file or start time.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Text in summary, tags or details." },
                    "kind": { "type": "string", "enum": kinds },
                    "file": { "type": "string", "description": "Referenced file." },
                    "since": { "type": "string", "format": "date-time", "description": "RFC 3339 lower bound." },
                    "includeSuperseded": { "type": "boolean", "default": false },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_TIMELINE_LIMIT, "default": 50, "description": "Output capped at 64 KiB." }
                }
            }),
        ),
        tool(
            "knobyte_heartbeat",
            "Scaffold health: overall status, stale scaffold files with their age in days, and memory cleanup status.",
            json!({
                "type": "object",
                "properties": {
                    "staleDays": { "type": "integer", "minimum": 1, "default": DEFAULT_STALE_THRESHOLD_DAYS, "description": "Age in days after which a file counts as stale." }
                }
            }),
        ),
        tool(
            "knobyte_read_file",
            "Read a text file inside the scaffold directory (.knobyte/), e.g. 'AGENTS.md' or 'context/stack.md'. Paths leaving the scaffold are rejected.",
            json!({
                "type": "object",
                "required": ["file"],
                "properties": {
                    "file": { "type": "string", "description": "Path relative to the scaffold root." }
                }
            }),
        ),
        tool(
            "knobyte_graph_query",
            "Find where a symbol is defined, who calls it, what it calls, or who imports it, with index freshness.",
            with_budget(
                json!({
                    "type": "object",
                    "required": ["relation", "target"],
                    "properties": {
                        "relation": { "type": "string", "enum": ["where-defined", "who-calls", "what-calls", "who-imports"] },
                        "target": { "type": "string", "description": "Symbol, function or module path." }
                    }
                }),
                false,
            ),
        ),
        tool(
            "knobyte_graph_scope",
            "Find the symbols, definitions and code neighbourhood most relevant to a natural-language task, with explanations and freshness.",
            with_budget(
                json!({
                    "type": "object",
                    "required": ["task"],
                    "properties": {
                        "task": { "type": "string", "description": "Natural-language task." },
                        "wiki": { "type": "boolean", "description": "Attach wiki entities grounded to the returned nodes." },
                        "hybrid": { "type": "boolean", "description": "Re-rank with vector similarity." }
                    }
                }),
                true,
            ),
        ),
        tool(
            "knobyte_graph_get",
            "Get source definitions and metadata for code graph nodes by ID.",
            with_budget(
                json!({
                    "type": "object",
                    "required": ["ids"],
                    "properties": {
                        "ids": string_array_schema("Code node IDs or grounding references.")
                    }
                }),
                false,
            ),
        ),
        tool(
            "knobyte_graph_status",
            "Code graph index status (node and edge counts, last indexed) plus working-tree freshness.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "knobyte_wiki_get",
            "Get one wiki entity by id: metadata, relations, backlinks, groundings with derived health, and index state; the body is opt-in. Read-only.",
            json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": { "type": "string", "description": "Entity id (e.g. 'kb_stack')." },
                    "includeBody": { "type": "boolean", "description": "Include the Markdown body (default false)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200, "description": "Page size for relations and backlinks (default 25)." },
                    "relationsOffset": { "type": "integer", "minimum": 0, "description": "relationsPage.nextOffset of the previous page." },
                    "backlinksOffset": { "type": "integer", "minimum": 0, "description": "backlinksPage.nextOffset of the previous page." }
                }
            }),
        ),
        tool(
            "knobyte_wiki_search",
            "Ranked, paged search over wiki entities (architecture notes, decisions, conventions); omit query to list them. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search text (max 256 characters); empty or omitted lists entities." },
                    "type": { "type": "array", "items": { "type": "string" }, "description": "Only these entity types." },
                    "status": { "type": "array", "items": { "type": "string" }, "description": "Only these lifecycle states." },
                    "topic": { "type": "string", "description": "Only members of this topic." },
                    "includeArchived": { "type": "boolean", "description": "Include archived entities (default false)." },
                    "includeBody": { "type": "boolean", "description": "Attach each entity's Markdown body (default false)." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Page size (default 25)." },
                    "maxTokens": { "type": "integer", "minimum": 64, "description": "Token budget for the page (default 4000)." },
                    "cursor": { "type": "string", "description": "nextCursor of the previous page of this exact request (refused once the wiki changed)." }
                }
            }),
        ),
        tool(
            "knobyte_wiki_neighborhood",
            "Breadth-first neighbourhood of a wiki entity over typed relations, bounded by depth, entity count and token budget. Read-only.",
            json!({
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": { "type": "string", "description": "Root entity id." },
                    "direction": { "type": "string", "enum": ["outgoing", "incoming", "both"], "description": "Edge direction (default both)." },
                    "relationTypes": { "type": "array", "items": { "type": "string" }, "description": "Only these relation types (e.g. depends_on)." },
                    "depth": { "type": "integer", "minimum": 1, "maximum": 5, "description": "Hops (default 2)." },
                    "maxEntities": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Entities reached (default 25)." },
                    "maxTokens": { "type": "integer", "minimum": 64, "description": "Token budget (default 4000)." },
                    "includeArchived": { "type": "boolean", "description": "Traverse archived entities (default false)." }
                }
            }),
        ),
        tool(
            "knobyte_wiki_validate",
            "Validate the wiki Markdown (no index needed) and return located diagnostics with remediation, plus the index state. Read-only.",
            json!({
                "type": "object",
                "properties": {
                    "entityIds": { "type": "array", "items": { "type": "string" }, "description": "Only diagnostics about these entities." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Only diagnostics in these scaffold-relative files or directories." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Maximum diagnostics (default 100)." }
                }
            }),
        ),
        tool(
            "knobyte_wiki_plan_operation",
            "Dry-run typed wiki operations and return the planned diffs plus a single-use plan handle (15 minutes) for knobyte_wiki_apply_operation. Nothing is written.",
            json!({
                "type": "object",
                "properties": {
                    "operations": { "type": "array", "items": { "type": "object" }, "description": "Operation envelopes {type, entityId, payload, reason?, opId?}; type is create-entry, update-entry, set-property, add-relation, remove-relation, add-source, remove-source, set-grounding, supersede-entry, move-entry or archive-entry." },
                    "operation": { "type": "object", "description": "A single operation envelope (alternative to operations)." },
                    "sessionId": { "type": "string", "description": "Agent session id recorded in the audit log." }
                }
            }),
        ),
        tool(
            "knobyte_wiki_apply_operation",
            "Apply a plan from knobyte_wiki_plan_operation by handle; nothing is written if an entity changed since planning (plan again). Edits the wiki only, never team inbox items.",
            json!({
                "type": "object",
                "required": ["handle"],
                "properties": {
                    "handle": { "type": "string", "description": "Plan handle from knobyte_wiki_plan_operation." },
                    "sessionId": { "type": "string", "description": "Agent session id recorded in the audit log." }
                }
            }),
        ),
        tool(
            "knobyte_relay_list",
            "List published team handoff relays.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "knobyte_relay_draft",
            "Save a DRAFT handoff relay (progress, blockers, next actions, evidence) as the current actor. A human reviews and publishes it.",
            json!({
                "type": "object",
                "required": ["title", "summary"],
                "properties": {
                    "title": { "type": "string" },
                    "summary": { "type": "string" },
                    "sender": { "type": "string", "description": "Optional check: must equal the current member." },
                    "namedRecipients": string_array_schema("Active member ids (see knobyte_members)."),
                    "openToTeam": { "type": "boolean", "description": "Any teammate may pick it up (default: true without named recipients)." },
                    "progress": string_array_schema("Work completed."),
                    "blockers": string_array_schema("Open blockers."),
                    "nextActions": string_array_schema("Recommended next actions."),
                    "evidence": string_array_schema(evidence)
                }
            }),
        ),
        tool(
            "knobyte_inbox_draft",
            "Save a DRAFT inbox proposal as the current actor: a typed change, or a Markdown edit (title + target + content). A human reviews, publishes and decides it.",
            json!({
                "type": "object",
                "required": ["reason"],
                "properties": {
                    "title": { "type": "string" },
                    "target": { "type": "string", "description": "Markdown edit: scaffold file under .knobyte/ (e.g. context/stack.md)." },
                    "content": { "type": "string", "description": "Markdown edit: proposed content." },
                    "mode": { "type": "string", "enum": ["replace", "append"], "description": "Markdown edit mode." },
                    "change": {
                        "type": "object",
                        "description": "Typed change (see `knobyte inbox contract`). *.create: {kind, entityKind, title, body, summary?, status?, topics?, relation? (spec only: {type, target: {id}})}. *.update: {kind, target: {id}, patch: {title?, summary?, body?}}.",
                        "required": ["kind"],
                        "properties": { "kind": { "type": "string", "enum": ["knowledge.create", "knowledge.update", "spec.create", "spec.update"] } }
                    },
                    "reason": { "type": "string", "description": "Why the change is proposed." },
                    "evidence": string_array_schema(evidence),
                    "author": { "type": "string", "description": "Optional check: must equal the current member." }
                }
            }),
        ),
        tool(
            "knobyte_members",
            "The effective local team member (current, null if none) and the registered team members.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "knobyte_session_start",
            "Entry point for new or resuming agents: project, current member, workstreams and in-progress step, git HEAD and dirty files, latest relay, recent decisions/risks, and scaffold health.",
            json!({ "type": "object", "properties": {} }),
        ),
        tool(
            "knobyte_workstream_step_update",
            "Checkpoint a workstream step's status (with optional evidence and touched files) as the current actor, stamped with git state. Creates a missing step, never a workstream.",
            json!({
                "type": "object",
                "required": ["status"],
                "properties": {
                    "workstreamId": { "type": "string", "description": "Defaults to the only active workstream (or the only one with an in-progress step)." },
                    "stepId": { "type": "string", "description": "Step ID (takes precedence over stepIndex; default: the in-progress step)." },
                    "stepIndex": { "type": "integer", "minimum": 0, "description": "0-based index of an existing step." },
                    "status": { "type": "string", "enum": STEP_STATUSES },
                    "evidence": { "type": "string", "description": "e.g. test results or command output." },
                    "filesTouched": string_array_schema("Files modified in this step.")
                }
            }),
        ),
        tool(
            "knobyte_playbooks",
            "List team playbooks (reusable procedures), or get one playbook by id (steps, checks, expected evidence, recent runs) or one run by runId.",
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Playbook id: return that playbook." },
                    "runId": { "type": "string", "description": "Playbook run id: return that run's step states and evidence." },
                    "state": { "type": "string", "enum": crate::team::playbooks::PLAYBOOK_STATES, "description": "List: only this state." },
                    "topic": { "type": "string", "description": "List: only this topic." },
                    "includeArchived": { "type": "boolean", "default": false, "description": "List: include archived playbooks." },
                    "cursor": { "type": "string", "description": "List: nextCursor of the previous page." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
                }
            }),
        ),
        tool(
            "knobyte_playbook_complete_step",
            "Complete one pending step of an active playbook run as the current actor, recording evidence. Agents cannot create, publish or archive playbooks, nor start or abandon runs.",
            json!({
                "type": "object",
                "required": ["runId", "stepId"],
                "properties": {
                    "runId": { "type": "string" },
                    "stepId": { "type": ["string", "integer"], "description": "Step id, or the step's 1-based number." },
                    "evidence": string_array_schema("Evidence: file:<path>, commit:<sha>, entity:<id>, code:<symbol>, a URL, or free text."),
                    "note": { "type": "string", "description": "How the step was done." }
                }
            }),
        ),
        tool(
            "knobyte_catch_up",
            "What changed in shared memory since the current actor last caught up: handoffs to me, reviews, decisions, knowledge, workstreams, playbooks, activity. mark=true marks it read instead.",
            json!({
                "type": "object",
                "properties": {
                    "since": { "type": "string", "description": "Override the baseline: RFC 3339, YYYY-MM-DD, or Nd/Nh." },
                    "workstream": { "type": "string", "description": "Only items related to this workstream." },
                    "groups": string_array_schema("Only these groups: handoffs, reviews, decisions, knowledge, workstreams, playbooks, activity."),
                    "includeMine": { "type": "boolean", "default": false, "description": "Also list the actor's own changes." },
                    "cursor": { "type": "string", "description": "nextCursor of the previous page." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
                    "mark": { "type": "boolean", "default": false, "description": "Mark the digest read: moves only this checkout's cursor (nothing shared is written)." },
                    "at": { "type": "string", "description": "With mark: RFC 3339 instant to mark up to; pass the digest's observedAt (default: now)." }
                }
            }),
        ),
        tool(
            "knobyte_file_context",
            "Context for one file: recorded events (decisions, risks, discoveries) that reference it and the code symbols defined in it.",
            json!({
                "type": "object",
                "required": ["filePath"],
                "properties": {
                    "filePath": { "type": "string", "description": "Repository-relative path (or suffix)." }
                }
            }),
        ),
        tool(
            "knobyte_harvest",
            "Harvest decisions from git history, ADRs and changelogs into the project event log (deduplicated); returns counts per source.",
            json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "default": 20, "description": "Git commits to scan." }
                }
            }),
        ),
    ]
}

/// Execute a tool against the project discovered from the server's working directory.
pub fn execute_tool(name: &str, args: &Value) -> CallToolResult {
    let config = crate::mcp::handler::server_config();
    execute_tool_with_config(name, args, &config)
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

fn pretty<T: serde::Serialize>(value: &T) -> CallToolResult {
    CallToolResult::text(&serde_json::to_string_pretty(value).unwrap_or_default())
}

/// Append the "events omitted" note as a second text item when entries were dropped.
fn with_omitted_note(mut result: CallToolResult, omitted: bool) -> CallToolResult {
    if omitted {
        let note = CallToolResult::text(TIMELINE_OMITTED_NOTE);
        result.content.extend(note.content);
    }
    result
}

/// Execute a tool against an explicit project configuration. A `projectRoot`
/// argument is accepted only when it resolves to this configuration's root.
pub fn execute_tool_with_config(name: &str, args: &Value, config: &KnobyteConfig) -> CallToolResult {
    if let Err(e) = check_project_root_arg(args.get("projectRoot"), config) {
        return CallToolResult::error(&e);
    }

    match name {
        "knobyte_check" => {
            let dry_run = args.get("dryRun").and_then(|v| v.as_bool()).unwrap_or(false);
            let fix = dry_run || args.get("fix").and_then(|v| v.as_bool()).unwrap_or(false);
            let sync = if fix {
                match crate::drift::sync_groundings(config, dry_run) {
                    Ok(res) => Some(res),
                    Err(e) => return CallToolResult::error(&format!("Failed to fix groundings: {}", e)),
                }
            } else {
                None
            };
            let report = run_drift_check(config);
            match sync {
                Some(sync) => pretty(&json!({ "fix": sync, "report": report })),
                None => pretty(&report),
            }
        }
        // Alias of knobyte_check {fix: true}: only the relocation result.
        "knobyte_sync_groundings" => {
            let dry_run = args.get("dryRun").and_then(|v| v.as_bool()).unwrap_or(false);
            match crate::drift::sync_groundings(config, dry_run) {
                Ok(res) => pretty(&res),
                Err(e) => CallToolResult::error(&format!("Failed to sync groundings: {}", e)),
            }
        }
        "knobyte_log" => {
            let action = str_arg(args, "action").unwrap_or("read");
            match action {
                "write" => {
                    let summary = match str_arg(args, "summary") {
                        Some(s) if !s.trim().is_empty() => s,
                        _ => return CallToolResult::error("'summary' is required for write action"),
                    };
                    let kind = str_arg(args, "kind").unwrap_or("note");
                    if !EVENT_KINDS.contains(&kind) {
                        return CallToolResult::error(&format!(
                            "Invalid kind '{}'; expected one of: {}",
                            kind,
                            EVENT_KINDS.join(", ")
                        ));
                    }
                    // The actor is always the resolved actor; a supplied `actor`
                    // must name it (no attributing events to someone else).
                    let resolved = resolve_actor(config).actor;
                    if let Some(claimed) = str_arg(args, "actor").map(str::trim).filter(|s| !s.is_empty()) {
                        if claimed != resolved.id() {
                            return CallToolResult::error(&format!(
                                "{}: 'actor' '{}' is not the current actor ({}); omit it to record as the current actor.",
                                crate::team::workflow::ACTOR_MISMATCH,
                                claimed,
                                resolved.id()
                            ));
                        }
                    }
                    let actor = Some(resolved.id()).filter(|a| a != "unknown");
                    let entry = EventEntry {
                        id: uuid::Uuid::new_v4().to_string(),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        kind: kind.to_string(),
                        summary: summary.to_string(),
                        details: str_arg(args, "details").map(|s| s.to_string()),
                        tags: parse_string_array(args.get("tags")),
                        files: parse_string_array(args.get("files")),
                        actor,
                        session_id: None,
                        supersedes: None,
                        superseded_by: None,
                        confidence: Some(1.0),
                        provenance: Some("mcp".to_string()),
                        ..Default::default()
                    };
                    match append_event_full(config, entry) {
                        Ok(entry) => pretty(&entry),
                        Err(e) => CallToolResult::error(&format!("Failed to write event: {}", e)),
                    }
                }
                "read" => {
                    let limit = clamp_timeline_limit(args.get("limit").and_then(|v| v.as_u64()), 20);
                    let events = read_events(config);
                    let total = events.len();
                    let recent: Vec<_> = events.into_iter().rev().take(limit).collect();
                    let (kept, _) = bound_timeline_output(recent);
                    with_omitted_note(pretty(&kept), kept.len() < total)
                }
                other => CallToolResult::error(&format!("Unknown action '{}'; expected 'read' or 'write'", other)),
            }
        }
        "knobyte_timeline" => {
            let since = match str_arg(args, "since") {
                Some(s) => match chrono::DateTime::parse_from_rfc3339(s) {
                    Ok(dt) => Some(dt.with_timezone(&chrono::Utc)),
                    Err(e) => return CallToolResult::error(&format!("Invalid 'since' timestamp: {}", e)),
                },
                None => None,
            };
            let filter = TimelineFilter {
                query: str_arg(args, "query").map(|s| s.to_string()),
                kind: str_arg(args, "kind").map(|s| s.to_string()),
                file: str_arg(args, "file").map(|s| s.to_string()),
                since,
                include_superseded: args.get("includeSuperseded").and_then(|v| v.as_bool()).unwrap_or(false),
                limit: clamp_timeline_limit(args.get("limit").and_then(|v| v.as_u64()), 50),
            };
            let mut res = query_timeline(config, filter);
            let (kept, dropped) = bound_timeline_output(std::mem::take(&mut res.entries));
            res.entries = kept;
            res.truncated = res.truncated || dropped > 0;
            let omitted = res.truncated;
            with_omitted_note(pretty(&res), omitted)
        }
        "knobyte_heartbeat" => {
            let days = args
                .get("staleDays")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_STALE_THRESHOLD_DAYS);
            pretty(&check_heartbeat(config, days))
        }
        "knobyte_read_file" => {
            let rel_file = match str_arg(args, "file") {
                Some(f) => f,
                None => return CallToolResult::error("'file' argument is required"),
            };
            let full_path = match resolve_confined_path(&config.scaffold_root, rel_file) {
                Ok(p) => p,
                Err(e) => return CallToolResult::error(&e),
            };
            if !full_path.is_file() {
                return CallToolResult::error(&format!("Not a regular file: {}", rel_file));
            }
            match fs::read_to_string(&full_path) {
                Ok(content) => CallToolResult::text(&content),
                Err(e) => CallToolResult::error(&format!("Failed to read file: {}", e)),
            }
        }
        "knobyte_graph_query" => {
            let relation = match str_arg(args, "relation") {
                Some(r) => r,
                None => return CallToolResult::error("'relation' argument is required"),
            };
            let target = match str_arg(args, "target") {
                Some(t) => t,
                None => return CallToolResult::error("'target' argument is required"),
            };
            let engine = match GraphEngine::open(&config.graph_db_path()) {
                Ok(e) => e,
                Err(e) => return CallToolResult::error(&format!("Failed to open graph.db: {}", e)),
            };
            match agent_input(args) {
                Err(e) => return CallToolResult::error(&e),
                Ok(Some(input)) => {
                    return pretty(&crate::graph::agent::run_query(
                        &engine,
                        &config.project_root,
                        relation,
                        target,
                        &input,
                    ))
                }
                Ok(None) => {}
            }
            let nodes = match relation {
                "where-defined" => engine.query_where_defined(target),
                "who-calls" => engine.query_who_calls(target),
                "what-calls" => engine.query_what_calls(target),
                "who-imports" => engine.query_who_imports(target),
                _ => return CallToolResult::error(&format!("Unknown relation: {}", relation)),
            };
            let freshness = get_freshness_metadata(config);
            match nodes {
                Ok(n) => pretty(&json!({ "results": n, "freshness": freshness })),
                Err(e) => CallToolResult::error(&format!("Query failed: {}", e)),
            }
        }
        "knobyte_graph_scope" => {
            let task = match str_arg(args, "task") {
                Some(t) => t,
                None => return CallToolResult::error("'task' argument is required"),
            };
            let engine = match GraphEngine::open(&config.graph_db_path()) {
                Ok(e) => e,
                Err(e) => return CallToolResult::error(&format!("Failed to open graph.db: {}", e)),
            };
            let wiki = args.get("wiki").and_then(|v| v.as_bool()).unwrap_or(false);
            let hybrid = args.get("hybrid").and_then(|v| v.as_bool()).unwrap_or(false);
            match agent_input(args) {
                Err(e) => return CallToolResult::error(&e),
                Ok(input) if input.is_some() || wiki || hybrid => {
                    let input = input.unwrap_or_default();
                    return pretty(&crate::graph::cli_agent::scope_records(config, &engine, task, &input, wiki, hybrid));
                }
                Ok(_) => {}
            }
            let freshness = get_freshness_metadata(config);
            match engine.query_scope_explained(task) {
                Ok(nodes) => pretty(&json!({ "results": nodes, "freshness": freshness })),
                Err(e) => CallToolResult::error(&format!("Scope query failed: {}", e)),
            }
        }
        "knobyte_graph_get" => {
            let ids: Vec<String> = args
                .get("ids")
                .and_then(|v| v.as_array())
                .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .or_else(|| str_arg(args, "id").map(|s| vec![s.to_string()]))
                .unwrap_or_default();
            if ids.is_empty() {
                return CallToolResult::error("'ids' argument is required");
            }
            let engine = match GraphEngine::open(&config.graph_db_path()) {
                Ok(e) => e,
                Err(e) => return CallToolResult::error(&format!("Failed to open graph.db: {}", e)),
            };
            match agent_input(args) {
                Err(e) => return CallToolResult::error(&e),
                Ok(Some(input)) => {
                    return pretty(&crate::graph::agent::run_get(&engine, &config.project_root, &ids, &input))
                }
                Ok(None) => {}
            }
            match engine.get_nodes(&ids) {
                Ok(nodes) => pretty(&nodes),
                Err(e) => CallToolResult::error(&format!("Get nodes failed: {}", e)),
            }
        }
        "knobyte_graph_status" => {
            let engine = match GraphEngine::open(&config.graph_db_path()) {
                Ok(e) => e,
                Err(e) => return CallToolResult::error(&format!("Failed to open graph.db: {}", e)),
            };
            let freshness = get_freshness_metadata(config);
            match engine.status() {
                Ok(status) => pretty(&json!({ "status": status, "freshness": freshness })),
                Err(e) => CallToolResult::error(&format!("Status failed: {}", e)),
            }
        }
        // Legacy aliases of knobyte_wiki_search / knobyte_wiki_get (offset paging, raw shapes).
        "knobyte_wiki_query" => {
            let text = match args.get("text").or_else(|| args.get("query")).and_then(|v| v.as_str()) {
                Some(t) => t.to_string(),
                None => return CallToolResult::error("'text' argument is required"),
            };
            let index = match open_wiki_for_tool(config) {
                Ok(i) => i,
                Err(e) => return e,
            };
            let filter = wiki_tool_filter(args);
            if text.trim().is_empty() {
                return match index.list_filtered(&filter) {
                    Ok(page) => wiki_page_result(&index, args, &filter, page.items, page.truncated),
                    Err(e) => CallToolResult::error(&format!("Wiki query failed: {}", e)),
                };
            }
            match index.search(&text, &filter) {
                Ok(page) => {
                    let matched: Vec<String> = page.items.iter().map(|h| h.matched.clone()).collect();
                    let items = page.items.into_iter().map(|h| h.entity).collect();
                    let mut out = wiki_page_value(&index, args, &filter, items, page.truncated);
                    if let Some(arr) = out["items"].as_array_mut() {
                        for (v, m) in arr.iter_mut().zip(matched) {
                            v["matched"] = json!(m);
                        }
                    }
                    pretty(&out)
                }
                Err(e) => CallToolResult::error(&format!("Wiki query failed: {}", e)),
            }
        }
        "knobyte_wiki_show" => {
            let id = match str_arg(args, "id") {
                Some(i) => i,
                None => return CallToolResult::error("'id' argument is required"),
            };
            let index = match open_wiki_for_tool(config) {
                Ok(i) => i,
                Err(e) => return e,
            };
            let opts = crate::wiki::index::DetailOptions {
                include_body: true,
                ..Default::default()
            };
            match index.entity_detail(id, &opts) {
                Ok(Some(detail)) => {
                    let mut v = json!(detail.entity);
                    v["relationsPage"] = json!(detail.relations_page);
                    v["backlinks"] = json!(detail.backlinks);
                    pretty(&v)
                }
                Ok(None) => CallToolResult::error(&format!("Wiki entity '{}' not found", id)),
                Err(e) => CallToolResult::error(&format!("Wiki show failed: {}", e)),
            }
        }
        "knobyte_wiki_list" => {
            let index = match open_wiki_for_tool(config) {
                Ok(i) => i,
                Err(e) => return e,
            };
            let filter = wiki_tool_filter(args);
            match index.list_filtered(&filter) {
                Ok(page) => wiki_page_result(&index, args, &filter, page.items, page.truncated),
                Err(e) => CallToolResult::error(&format!("Wiki list failed: {}", e)),
            }
        }
        "knobyte_wiki_get" => wiki_get_tool(args, config),
        "knobyte_wiki_search" => wiki_search_tool(args, config),
        "knobyte_wiki_neighborhood" => wiki_neighborhood_tool(args, config),
        "knobyte_wiki_validate" => wiki_validate_tool(args, config),
        "knobyte_wiki_grounding_status" => wiki_grounding_status_tool(args, config),
        "knobyte_wiki_plan_operation" => wiki_plan_operation_tool(args, config),
        "knobyte_wiki_apply_operation" => wiki_apply_operation_tool(args, config),
        "knobyte_relay_list" => pretty(&list_relays(config)),
        "knobyte_relay_draft" => relay_draft_tool(args, config),
        "knobyte_inbox_draft" => inbox_draft_tool(args, config),
        "knobyte_members" => pretty(&json!({ "current": get_current_member(config), "members": list_members(config) })),
        // Aliases of knobyte_members: one field each.
        "knobyte_member_list" => pretty(&list_members(config)),
        "knobyte_member_current" => match get_current_member(config) {
            Some(m) => pretty(&m),
            None => CallToolResult::text("null"),
        },
        "knobyte_vector_search" => {
            let query = match str_arg(args, "query") {
                Some(q) => q,
                None => return CallToolResult::error("'query' argument is required"),
            };
            let target = str_arg(args, "target").unwrap_or("code");
            if target != "code" && target != "wiki" {
                return CallToolResult::error("'target' must be 'code' or 'wiki'");
            }
            let k = args.get("k").and_then(|v| v.as_u64()).unwrap_or(10).max(1) as usize;
            let min_score = match args.get("minScore") {
                None | Some(Value::Null) => None,
                Some(v) => match v.as_f64().filter(|s| (0.0..=1.0).contains(s)) {
                    Some(s) => Some(s),
                    None => return CallToolResult::error("'minScore' must be a number from 0 to 1"),
                },
            };
            let freshness = get_freshness_metadata(config);
            match get_cozo_engine(config) {
                Ok(engine) => {
                    // The index was built by another embedder: re-embed before searching.
                    if matches!(engine.space_mismatch(target), Ok(Some(_))) {
                        sync_cozo_engine(&engine, config);
                    }
                    let embedder = json!({
                        "id": engine.embedder().id(),
                        "dim": engine.embedder().dim()
                    });
                    let opts = crate::cozo::VectorSearchOptions { k, min_score };
                    match engine.vector_search_with(query, target, &opts) {
                        Ok(out) => {
                            let message = match (out.matches.is_empty(), out.below_floor) {
                                (true, 0) => Some("No matches: the index has no entries for this target (run knobyte cozo sync)".to_string()),
                                (true, n) => Some(format!("No matches: all {} nearest candidate(s) scored below the relevance floor {:.2}; lower minScore to include them", n, out.min_score)),
                                (false, n) if n > 0 => Some(format!("{} of the {} nearest candidate(s) scored below the relevance floor {:.2} and were left out; lower minScore to include them", n, out.k, out.min_score)),
                                _ => None,
                            };
                            let mut body = json!({
                                "matches": out.matches,
                                "k": out.k,
                                "minScore": out.min_score,
                                "belowFloor": out.below_floor,
                                "embedder": embedder,
                                "freshness": freshness
                            });
                            if let Some(m) = message {
                                body["message"] = json!(m);
                            }
                            pretty(&body)
                        }
                        Err(e) => CallToolResult::error(&format!("Vector search failed: {}", e)),
                    }
                }
                Err(e) => CallToolResult::error(&e),
            }
        }
        "knobyte_cozo_datalog" => {
            let script = match str_arg(args, "script") {
                Some(s) => s,
                None => return CallToolResult::error("'script' argument is required"),
            };
            let params = args.get("params").cloned().unwrap_or_else(|| json!({}));
            if !params.is_object() {
                return CallToolResult::error("'params' must be an object");
            }
            match get_cozo_engine(config) {
                Ok(engine) => match engine.datalog_query(script, params) {
                    Ok(res) => pretty(&res),
                    Err(e) => CallToolResult::error(&format!("Datalog query failed: {}", e)),
                },
                Err(e) => CallToolResult::error(&e),
            }
        }
        "knobyte_cozo_pagerank" => {
            let theta = args.get("theta").and_then(|v| v.as_f64());
            let iterations = args.get("iterations").and_then(|v| v.as_u64()).map(|v| v as usize);
            match get_cozo_engine(config) {
                Ok(engine) => match engine.pagerank(theta, iterations) {
                    Ok(ranks) => pretty(&ranks),
                    Err(e) => CallToolResult::error(&format!("PageRank failed: {}", e)),
                },
                Err(e) => CallToolResult::error(&e),
            }
        }
        "knobyte_cozo_shortest_path" => {
            let start = match str_arg(args, "start") {
                Some(s) => s,
                None => return CallToolResult::error("'start' argument is required"),
            };
            let target = match str_arg(args, "target") {
                Some(t) => t,
                None => return CallToolResult::error("'target' argument is required"),
            };
            match get_cozo_engine(config) {
                Ok(engine) => match engine.shortest_path_detailed(start, target) {
                    Ok(Some(path)) => pretty(&path),
                    Ok(None) => CallToolResult::text("null"),
                    Err(e) => CallToolResult::error(&format!("Shortest path failed: {}", e)),
                },
                Err(e) => CallToolResult::error(&e),
            }
        }
        "knobyte_session_start" => {
            let (head, dirty_files) = crate::team::workstreams::get_git_state(&config.project_root);
            let member = get_current_member(config);
            let workstreams = crate::team::workstreams::list_workstreams(config);
            let current_step = workstreams
                .iter()
                .flat_map(|w| w.steps.iter().map(move |s| (w.id.clone(), s)))
                .find(|(_, s)| s.status == "in_progress");
            let last_relay = list_relays(config).into_iter().last();
            let events = read_events(config);
            let recent_decisions: Vec<_> = events
                .iter()
                .rev()
                .filter(|e| e.kind == "decision" || e.kind == "risk")
                .take(5)
                .cloned()
                .collect();
            let heartbeat = check_heartbeat(config, DEFAULT_STALE_THRESHOLD_DAYS);

            pretty(&json!({
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
            }))
        }
        "knobyte_workstream_step_update" => workstream_step_update(args, config),
        "knobyte_playbooks" => {
            if args.get("id").is_some_and(|v| !v.is_null()) || args.get("runId").is_some_and(|v| !v.is_null()) {
                playbook_get_tool(args, config)
            } else {
                playbook_list_tool(args, config)
            }
        }
        // Aliases of knobyte_playbooks.
        "knobyte_playbook_list" => playbook_list_tool(args, config),
        "knobyte_playbook_get" => playbook_get_tool(args, config),
        "knobyte_playbook_complete_step" => playbook_complete_step_tool(args, config),
        "knobyte_catch_up" if args.get("mark").and_then(|v| v.as_bool()).unwrap_or(false) => catch_up_mark(args, config),
        "knobyte_catch_up" => catch_up_tool(args, config),
        // Alias of knobyte_catch_up {mark: true}.
        "knobyte_catch_up_mark" => catch_up_mark(args, config),
        "knobyte_file_context" => {
            let file_path = match args.get("filePath").or_else(|| args.get("file")).and_then(|v| v.as_str()) {
                Some(f) if !f.trim().is_empty() => f,
                _ => return CallToolResult::error("'filePath' argument is required"),
            };
            let events = read_events(config);
            let matching_events: Vec<_> = events
                .into_iter()
                .filter(|e| {
                    e.files.iter().any(|f| f.contains(file_path) || file_path.contains(f.as_str()))
                        || e.summary.contains(file_path)
                })
                .collect();

            let mut defined_symbols = Vec::new();
            if config.graph_db_path().exists() {
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
                            defined_symbols.extend(rows.flatten());
                        }
                    }
                }
            }

            pretty(&json!({
                "file_path": file_path,
                "recorded_events": matching_events,
                "symbols_defined": defined_symbols,
            }))
        }
        "knobyte_harvest" => {
            let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            pretty(&crate::harvest::harvest_all(config, limit))
        }
        _ => CallToolResult::error(&format!("Unknown tool: {}", name)),
    }
}

/// Error result carrying the team problem code (e.g. UNAUTHORIZED, NOT_FOUND).
fn team_error(e: &TeamError) -> CallToolResult {
    let mut r = CallToolResult::error(&e.detail);
    r.content[0].text = json!({ "error": e.detail, "code": e.code, "title": e.title }).to_string();
    r
}

/// Run a team action through the preview/apply workflow (lock, journal,
/// activity). The first content item is the action result; diagnostics, if
/// any, follow as a second item.
fn run_team_action(config: &KnobyteConfig, action: Value, actor: &ActorChoice) -> CallToolResult {
    match run_action(config, action, actor) {
        Ok(r) => {
            let mut out = pretty(&r.result);
            if !r.diagnostics.is_empty() {
                out.content.extend(CallToolResult::text(&json!({ "diagnostics": r.diagnostics }).to_string()).content);
            }
            out
        }
        Err(e) => team_error(&e),
    }
}

/// A caller-named actor (`sender`, `author`) is only an assertion: it must be
/// the resolved actor of this checkout.
fn requested_actor(args: &Value, key: &str) -> ActorChoice {
    match str_arg(args, key).map(str::trim).filter(|s| !s.is_empty()) {
        Some(m) => ActorChoice::member(m),
        None => ActorChoice::resolved(),
    }
}

/// Refuse member ids that are not registered, active members.
fn require_active_members(config: &KnobyteConfig, label: &str, ids: &[String]) -> Result<(), CallToolResult> {
    let unknown: Vec<&str> = ids
        .iter()
        .filter(|id| !get_member(config, id).map(|m| m.is_active()).unwrap_or(false))
        .map(|s| s.as_str())
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(team_error(&TeamError::validation(format!(
            "Unknown or inactive {}: {} (see knobyte_members)",
            label,
            unknown.join(", ")
        ))))
    }
}

fn relay_draft_tool(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    let title = match str_arg(args, "title") {
        Some(t) if !t.trim().is_empty() => t,
        _ => return CallToolResult::error("'title' argument is required"),
    };
    let summary = match str_arg(args, "summary") {
        Some(s) => s,
        None => return CallToolResult::error("'summary' argument is required"),
    };
    let recipients = parse_string_array(args.get("namedRecipients"));
    if let Err(r) = require_active_members(config, "recipient(s)", &recipients) {
        return r;
    }
    let evidence = match parse_evidence(&parse_string_array(args.get("evidence"))) {
        Ok(e) => e,
        Err(e) => return team_error(&TeamError::usage(e)),
    };
    let mut draft = json!({
        "title": title,
        "summary": summary,
        "recipients": recipients,
        "progress": parse_string_array(args.get("progress")),
        "blockers": parse_string_array(args.get("blockers")),
        "nextActions": parse_string_array(args.get("nextActions")),
        "evidence": evidence,
    });
    if let Some(team) = args.get("openToTeam").and_then(|v| v.as_bool()) {
        draft["audience"] = json!(if team { "team" } else { "members" });
    }
    run_team_action(config, json!({ "kind": "relay.draft.save", "draft": draft }), &requested_actor(args, "sender"))
}

fn inbox_draft_tool(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    let reason = match str_arg(args, "reason") {
        Some(r) if !r.trim().is_empty() => r,
        _ => return CallToolResult::error("'reason' argument is required"),
    };
    let evidence = match parse_evidence(&parse_string_array(args.get("evidence"))) {
        Ok(e) => e,
        Err(e) => return team_error(&TeamError::usage(e)),
    };
    let mut input = json!({ "rationale": reason, "evidence": evidence });
    if let Some(t) = str_arg(args, "title") {
        input["title"] = json!(t);
    }
    match args.get("change").filter(|c| !c.is_null()) {
        Some(change) => {
            if args.get("target").is_some() || args.get("content").is_some() {
                return CallToolResult::error("'change' cannot be combined with 'target'/'content'");
            }
            input["change"] = change.clone();
        }
        None => {
            for key in ["title", "target", "content"] {
                if str_arg(args, key).is_none() {
                    return CallToolResult::error(&format!("'{}' argument is required (or pass a typed 'change')", key));
                }
            }
            input["target"] = json!(str_arg(args, "target"));
            input["content"] = json!(str_arg(args, "content"));
            if let Some(m) = str_arg(args, "mode") {
                input["mode"] = json!(m);
            }
        }
    }
    run_team_action(config, json!({ "kind": "inbox.draft.save", "draft": input }), &requested_actor(args, "author"))
}

fn workstream_step_update(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    use crate::team::workstreams::{active_workstreams, get_workstream};

    let status = match str_arg(args, "status") {
        Some(s) => s,
        None => return CallToolResult::error("'status' argument is required"),
    };
    if !STEP_STATUSES.contains(&status) {
        return CallToolResult::error(&format!(
            "Invalid status '{}'; expected one of: {}",
            status,
            STEP_STATUSES.join(", ")
        ));
    }

    let ws = match str_arg(args, "workstreamId") {
        Some(id) => match validate_id(id) {
            Ok(id) => match get_workstream(config, id) {
                Some(ws) => ws,
                None => return team_error(&TeamError::not_found("Workstream", id)),
            },
            Err(e) => return CallToolResult::error(&format!("Invalid workstreamId: {}", e)),
        },
        None => {
            // Only an active workstream is ever targeted implicitly; nothing is created.
            let active = active_workstreams(config);
            let with_step: Vec<_> = active.iter().filter(|w| w.steps.iter().any(|s| s.status == "in_progress")).collect();
            match (active.len(), with_step.len()) {
                (0, _) => {
                    return CallToolResult::error(
                        "No active workstream. Create one (`knobyte workstream create`) or pass 'workstreamId'.",
                    )
                }
                (1, _) => active[0].clone(),
                (_, 1) => with_step[0].clone(),
                _ => {
                    return CallToolResult::error(&format!(
                        "Several active workstreams ({}); pass 'workstreamId'.",
                        active.iter().map(|w| w.id.as_str()).collect::<Vec<_>>().join(", ")
                    ))
                }
            }
        }
    };

    let step_id = if let Some(sid) = str_arg(args, "stepId") {
        sid.to_string()
    } else if args.get("stepIndex").is_some_and(|v| !v.is_null() && v.as_u64().is_none()) {
        return CallToolResult::error("'stepIndex' must be a non-negative integer");
    } else if let Some(idx) = args.get("stepIndex").and_then(|v| v.as_u64()) {
        match ws.steps.get(idx as usize) {
            Some(step) => step.id.clone(),
            None => {
                return CallToolResult::error(&format!(
                    "stepIndex {} is out of range: workstream '{}' has {} step{}. Use an existing step index or pass 'stepId'.",
                    idx,
                    ws.id,
                    ws.steps.len(),
                    if ws.steps.len() == 1 { "" } else { "s" }
                ))
            }
        }
    } else {
        match ws.steps.iter().find(|s| s.status == "in_progress") {
            Some(step) => step.id.clone(),
            None => return CallToolResult::error("'stepId' or 'stepIndex' is required (no step is in progress)"),
        }
    };

    let mut action = json!({ "kind": "workstream.step.update", "workstreamId": ws.id, "stepId": step_id, "status": status });
    if let Some(ev) = str_arg(args, "evidence") {
        action["evidence"] = json!(ev);
    }
    let files = parse_string_array(args.get("filesTouched"));
    if !files.is_empty() {
        action["filesTouched"] = json!(files);
    }
    run_team_action(config, action, &ActorChoice::resolved())
}

fn limit_arg(args: &Value) -> Option<usize> {
    args.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize)
}

fn playbook_list_tool(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    let states: Vec<String> = str_arg(args, "state").map(|s| vec![s.to_string()]).unwrap_or_default();
    match crate::team::playbooks::list_playbooks_page(
        config,
        &states,
        str_arg(args, "topic"),
        args.get("includeArchived").and_then(|v| v.as_bool()).unwrap_or(false),
        str_arg(args, "cursor"),
        limit_arg(args),
    ) {
        Ok(page) => pretty(&page),
        Err(e) => team_error(&e),
    }
}

fn playbook_get_tool(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    match (str_arg(args, "id"), str_arg(args, "runId")) {
        (Some(_), Some(_)) => CallToolResult::error("Pass either 'id' or 'runId', not both"),
        (Some(id), None) => match crate::team::playbooks::playbook_detail(config, id) {
            Ok(v) => pretty(&v),
            Err(e) => team_error(&e),
        },
        (None, Some(run_id)) => match crate::team::playbooks::get_run(config, run_id) {
            Some(r) => pretty(&r),
            None => team_error(&TeamError::not_found("Playbook run", run_id)),
        },
        (None, None) => CallToolResult::error("'id' or 'runId' argument is required"),
    }
}

fn playbook_complete_step_tool(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    let step_ref = args.get("stepId").and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) if n.is_u64() => Some(n.to_string()),
        _ => None,
    });
    let (run_id, step_id) = match (str_arg(args, "runId"), step_ref) {
        (Some(r), Some(s)) => (r, s),
        _ => return CallToolResult::error("'runId' and 'stepId' arguments are required"),
    };
    let evidence = match parse_evidence(&parse_string_array(args.get("evidence"))) {
        Ok(e) => e,
        Err(e) => return team_error(&TeamError::usage(e)),
    };
    let mut action = json!({ "kind": "playbook.run.complete-step", "runId": run_id, "stepId": step_id, "evidence": evidence });
    if let Some(note) = str_arg(args, "note") {
        action["note"] = json!(note);
    }
    run_team_action(config, action, &ActorChoice::resolved())
}

fn catch_up_mark(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    let mut action = json!({ "kind": "catchup.mark" });
    if let Some(at) = str_arg(args, "at").filter(|a| !a.trim().is_empty()) {
        action["at"] = json!(at);
    }
    run_team_action(config, action, &ActorChoice::resolved())
}

fn catch_up_tool(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    let req = crate::team::catchup::CatchUpRequest {
        since: str_arg(args, "since").map(str::to_string),
        workstream: str_arg(args, "workstream").map(str::to_string),
        include_mine: args.get("includeMine").and_then(|v| v.as_bool()).unwrap_or(false),
        groups: parse_string_array(args.get("groups")),
        cursor: str_arg(args, "cursor").map(str::to_string),
        limit: limit_arg(args),
    };
    match crate::team::catchup::catch_up_digest(config, &req) {
        Ok((mut data, diagnostics)) => {
            if !diagnostics.is_empty() {
                data["diagnostics"] = json!(diagnostics);
            }
            pretty(&data)
        }
        Err(e) => team_error(&e),
    }
}

// ---------------------------------------------------------------------------
// knobyte_wiki_* helpers: read-only index access and bounded, paged results
// ---------------------------------------------------------------------------

/// Open the wiki index read-only for a tool call; typed failures (`WIKI_INDEX_MISSING`,
/// `WIKI_INDEX_REBUILD_REQUIRED`) come back as tool errors and never touch the index.
fn open_wiki_for_tool(config: &KnobyteConfig) -> Result<WikiIndex, CallToolResult> {
    WikiIndex::open_read_only(&config.wiki_db_path()).map_err(|e| {
        let hint = match crate::wiki::index::error_code(&e) {
            Some("WIKI_INDEX_MISSING") => " Run `knobyte wiki rebuild-index`.",
            _ => "",
        };
        CallToolResult::error(&format!("{}{}", e, hint))
    })
}

fn wiki_tool_strings(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        Some(Value::String(s)) => s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        Some(v) => parse_string_array(Some(v)),
        None => Vec::new(),
    }
}

fn wiki_tool_filter(args: &Value) -> crate::wiki::index::QueryFilter {
    crate::wiki::index::QueryFilter {
        types: wiki_tool_strings(args, "type"),
        topic: str_arg(args, "topic").map(|s| s.to_string()),
        statuses: wiki_tool_strings(args, "status"),
        include_archived: args
            .get("includeArchived")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        limit: args.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize),
        offset: args
            .get("offset")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(0),
        ..Default::default()
    }
}

fn wiki_page_value(
    index: &WikiIndex,
    args: &Value,
    filter: &crate::wiki::index::QueryFilter,
    items: Vec<crate::wiki::index::EntitySummary>,
    truncated: bool,
) -> Value {
    let include_body = args
        .get("includeBody")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let next_offset = truncated.then(|| filter.offset + items.len());
    let items: Vec<Value> = items
        .into_iter()
        .map(|s| {
            let mut v = json!(s);
            if include_body {
                if let Ok(Some(e)) = index.show(&s.id) {
                    v["body"] = json!(e.body);
                }
            }
            v
        })
        .collect();
    json!({ "items": items, "truncated": truncated, "nextOffset": next_offset })
}

fn wiki_page_result(
    index: &WikiIndex,
    args: &Value,
    filter: &crate::wiki::index::QueryFilter,
    items: Vec<crate::wiki::index::EntitySummary>,
    truncated: bool,
) -> CallToolResult {
    pretty(&wiki_page_value(index, args, filter, items, truncated))
}

// ---------------------------------------------------------------------------
// Contract wiki tools: snapshot-bound sessions, envelopes, planned operations
// ---------------------------------------------------------------------------

/// A wiki envelope as a tool result; a failed request (no data) is a tool error.
fn wiki_envelope_result(env: crate::wiki::envelope::WikiEnvelope) -> CallToolResult {
    let text = serde_json::to_string_pretty(&env).unwrap_or_default();
    let mut result = CallToolResult::text(&text);
    if !env.ok && env.data.is_null() {
        result.is_error = Some(true);
    }
    result
}

fn wiki_fail(d: impl Into<Box<crate::wiki::models::WikiDiagnostic>>) -> CallToolResult {
    wiki_envelope_result(crate::wiki::envelope::failure(&[*d.into()]))
}

fn wiki_scope(config: &KnobyteConfig) -> crate::wiki::scope::WikiScope {
    crate::wiki::scope::WikiScope::load(&config.scaffold_root)
}

fn wiki_session(config: &KnobyteConfig) -> Result<crate::wiki::session::WikiReadSession, CallToolResult> {
    crate::wiki::session::open_read_session(&config.wiki_db_path(), &wiki_scope(config)).map_err(wiki_fail)
}

fn wiki_index_state(session: &crate::wiki::session::WikiReadSession) -> Value {
    let mut v = session.status().state_value();
    v["indexedRevision"] = json!(session.indexed_revision());
    v["snapshotRevision"] = json!(session.snapshot_revision());
    v
}

fn wiki_request<T: serde::de::DeserializeOwned>(args: &Value) -> Result<T, CallToolResult> {
    let mut v = args.clone();
    if let Some(o) = v.as_object_mut() {
        o.remove("projectRoot");
        // Accept a single string where a list is expected.
        for key in ["type", "status", "relationTypes", "entityIds", "paths"] {
            if let Some(Value::String(s)) = o.get(key).cloned() {
                o.insert(key.to_string(), json!(s.split(',').map(|x| x.trim()).filter(|x| !x.is_empty()).collect::<Vec<_>>()));
            }
        }
    }
    serde_json::from_value(v).map_err(|e| {
        wiki_fail(crate::wiki::diagnostics::diag("INVALID_REQUEST", format!("Invalid arguments: {}", e), ""))
    })
}

fn wiki_get_tool(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    let req: crate::wiki::session::GetRequest = match wiki_request(args) {
        Ok(r) => r,
        Err(e) => return e,
    };
    if req.id.is_empty() {
        return wiki_fail(crate::wiki::diagnostics::diag("INVALID_REQUEST", "'id' argument is required", ""));
    }
    let id = req.id.as_str();
    let session = match wiki_session(config) {
        Ok(s) => s,
        Err(e) => return e,
    };
    match session.get_with(id, &req) {
        Ok(mut entity) => {
            entity["index"] = wiki_index_state(&session);
            wiki_envelope_result(crate::wiki::envelope::envelope_for(&entity, &session.status().diagnostics))
        }
        Err(d) => wiki_fail(d),
    }
}

fn wiki_search_tool(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    let req: crate::wiki::session::SearchRequest = match wiki_request(args) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let session = match wiki_session(config) {
        Ok(s) => s,
        Err(e) => return e,
    };
    let include_body = args.get("includeBody").and_then(|v| v.as_bool()).unwrap_or(false);
    match session.search(&req) {
        Ok(page) => {
            let mut data = json!(page);
            if include_body {
                if let Some(items) = data["items"].as_array_mut() {
                    for item in items {
                        let id = item["entity"]["id"].as_str().map(str::to_string);
                        if let Some(Ok(Some(e))) = id.map(|id| session.index().show(&id)) {
                            item["entity"]["body"] = json!(e.body);
                        }
                    }
                }
            }
            data["index"] = wiki_index_state(&session);
            wiki_envelope_result(crate::wiki::envelope::envelope_for(&data, &session.status().diagnostics))
        }
        Err(d) => wiki_fail(d),
    }
}

fn wiki_neighborhood_tool(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    let mut a = args.clone();
    if let Some(o) = a.as_object_mut() {
        if let Some(id) = o.remove("id") {
            o.insert("entityId".into(), id);
        }
    }
    let req: crate::wiki::session::NeighborhoodRequest = match wiki_request(&a) {
        Ok(r) => r,
        Err(e) => return e,
    };
    if req.entity_id.is_empty() {
        return wiki_fail(crate::wiki::diagnostics::diag("INVALID_REQUEST", "'id' argument is required", ""));
    }
    let session = match wiki_session(config) {
        Ok(s) => s,
        Err(e) => return e,
    };
    match session.neighborhood(&req) {
        Ok(n) => {
            let mut data = json!(n);
            data["index"] = wiki_index_state(&session);
            wiki_envelope_result(crate::wiki::envelope::envelope_for(&data, &session.status().diagnostics))
        }
        Err(d) => wiki_fail(d),
    }
}

fn wiki_validate_tool(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    let req: crate::wiki::session::DiagnosticRequest = match wiki_request(args) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let limit = req.limit.unwrap_or(100).clamp(1, 500);
    let scope = wiki_scope(config);
    let index = WikiIndex::open_read_only(&config.wiki_db_path()).ok();
    let graph_db = config.graph_db_path();
    let report = crate::wiki::validate::validate_scaffold(&crate::wiki::validate::ValidateOptions {
        scope: &scope,
        project_root: &config.project_root,
        graph_db: Some(graph_db.as_path()),
        index: index.as_ref().filter(|i| i.is_built()),
        limit: None,
    });
    let matching: Vec<_> = report
        .diagnostics
        .iter()
        .filter(|d| {
            (req.entity_ids.is_empty() || d.entity_id.as_ref().is_some_and(|e| req.entity_ids.contains(e)))
                && (req.paths.is_empty()
                    || req.paths.iter().any(|p| d.file == *p || d.file.starts_with(&format!("{}/", p.trim_end_matches('/')))))
        })
        .cloned()
        .collect();
    let count = |sev: &str| matching.iter().filter(|d| d.severity == sev).count();
    let counts = json!({ "error": count("error"), "warning": count("warning"), "info": count("info") });
    let valid = count("error") == 0;
    let truncated = matching.len() > limit;
    let shown: Vec<_> = matching.into_iter().take(limit).collect();
    let status = crate::wiki::maintenance::inspect_index(&config.wiki_db_path(), &scope, true);
    let data = json!({
        "valid": valid,
        "filesScanned": report.files_scanned,
        "entitiesChecked": report.entities_checked,
        "counts": counts,
        "truncated": truncated,
        "codeGraphAvailable": report.code_graph_available,
        "index": status.state_value(),
    });
    wiki_envelope_result(crate::wiki::envelope::envelope_for(&data, &shown))
}

fn wiki_grounding_status_tool(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    let Some(id) = str_arg(args, "id") else {
        return wiki_fail(crate::wiki::diagnostics::diag("INVALID_REQUEST", "'id' argument is required", ""));
    };
    let session = match wiki_session(config) {
        Ok(s) => s,
        Err(e) => return e,
    };
    match session.grounding_status(id) {
        Ok(groundings) => {
            let data = json!({ "id": id, "groundings": groundings, "index": wiki_index_state(&session) });
            wiki_envelope_result(crate::wiki::envelope::envelope_for(&data, &session.status().diagnostics))
        }
        Err(d) => wiki_fail(d),
    }
}

fn wiki_agent_actor(args: &Value) -> crate::wiki::ops::OpActor {
    crate::wiki::ops::OpActor {
        kind: "agent".into(),
        id: "mcp".into(),
        session_id: str_arg(args, "sessionId").map(|s| s.to_string()),
    }
}

fn wiki_plan_operation_tool(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    let ops: Vec<Value> = match (args.get("operations"), args.get("operation")) {
        (Some(Value::Array(a)), _) => a.clone(),
        (_, Some(o @ Value::Object(_))) => vec![o.clone()],
        _ => {
            return wiki_fail(crate::wiki::diagnostics::diag(
                "INVALID_REQUEST",
                "Pass `operations` (an array of operation envelopes) or `operation` (one envelope)",
                "",
            ))
        }
    };
    let scope = wiki_scope(config);
    let graph_db = config.graph_db_path();
    match crate::wiki::plans::plan(&scope, Some(graph_db.as_path()), &ops, wiki_agent_actor(args)) {
        Ok(planned) => {
            let mut data = json!(planned);
            if let Some(report) = data.get_mut("report").and_then(|r| r.as_object_mut()) {
                report.remove("diagnostics");
            }
            wiki_envelope_result(crate::wiki::envelope::envelope_for(&data, &planned.report.diagnostics))
        }
        Err(d) => wiki_fail(d),
    }
}

fn wiki_apply_operation_tool(args: &Value, config: &KnobyteConfig) -> CallToolResult {
    let Some(handle) = str_arg(args, "handle") else {
        return wiki_fail(crate::wiki::diagnostics::diag("INVALID_REQUEST", "'handle' argument is required", ""));
    };
    let scope = wiki_scope(config);
    let graph_db = config.graph_db_path();
    match crate::wiki::plans::apply_planned(&scope, Some(graph_db.as_path()), handle, wiki_agent_actor(args)) {
        Ok(report) => {
            let mut diags = report.diagnostics.clone();
            let mut refreshed = false;
            if report.ok && !report.changed_files.is_empty() {
                match WikiIndex::open(&config.wiki_db_path()).and_then(|mut i| i.refresh(&config.scaffold_root)) {
                    Ok(_) => refreshed = true,
                    Err(e) => diags.push(crate::wiki::diagnostics::diag(
                        "INDEX_REFRESH_REQUIRED",
                        format!("The Markdown was written but the index did not refresh: {}", e),
                        "wiki.db",
                    )),
                }
            }
            let mut data = json!(report);
            if let Some(o) = data.as_object_mut() {
                o.remove("diagnostics");
            }
            data["indexRefreshed"] = json!(refreshed);
            let mut result = wiki_envelope_result(crate::wiki::envelope::envelope_for(&data, &diags));
            if !report.ok {
                result.is_error = Some(true);
            }
            result
        }
        Err(d) => wiki_fail(d),
    }
}

fn get_cozo_engine(config: &KnobyteConfig) -> Result<CozoEngine, String> {
    let db_path = config.cozo_db_path();
    let is_new = !db_path.exists();
    // Uses the embedding backend from .knobyte/config.json; a configured but missing model is
    // reported as an error (never downloaded, never silently replaced by another backend).
    // Long-running servers re-read config.json so `knobyte cozo model use` takes effect without
    // a restart (otherwise the server and CLI would keep re-embedding with different backends).
    let fresh = KnobyteConfig::new(config.project_root.clone(), config.scaffold_root.clone());
    let engine =
        CozoEngine::open_configured(&fresh).map_err(|e| format!("Failed to open CozoDB: {}", e))?;
    if is_new {
        sync_cozo_engine(&engine, config);
    }
    Ok(engine)
}

fn sync_cozo_engine(engine: &CozoEngine, config: &KnobyteConfig) {
    if let Ok(graph_conn) = rusqlite::Connection::open(config.graph_db_path()) {
        let _ = engine.sync_from_graph(&graph_conn);
    }
    if let Ok(wiki_conn) = rusqlite::Connection::open(config.wiki_db_path()) {
        let _ = engine.sync_from_wiki(&wiki_conn);
    }
}

fn parse_string_array(val: Option<&Value>) -> Vec<String> {
    val.and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default()
}

fn get_freshness_metadata(config: &KnobyteConfig) -> Value {
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
