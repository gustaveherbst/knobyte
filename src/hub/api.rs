//! JSON API consumed by the Hub front-end (`assets/app.js`).

use std::collections::{HashMap, HashSet};

use axum::{
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use super::problem::Query;
use serde::Deserialize;
use serde_json::{json, Value};

use super::diff::line_diff;
use super::projects::{aggregate_contributors, discover_projects_at};
use super::HubState;
use crate::config::KnobyteConfig;
use crate::drift::checker::run_drift_check;
use crate::events::read_events;
use crate::graph::{GraphEngine, Node};
use crate::heartbeat::check_heartbeat;
use crate::mcp::security::resolve_confined_path;
use crate::team::activity::list_activity;
use crate::team::inbox::{
    get_proposal, list_inbox_drafts, list_inbox_proposals, normalize_proposal_target,
    publish_inbox_draft, reject_proposal, PROPOSAL_MODE_APPEND, PROPOSAL_STATUS_PENDING,
};
use crate::team::members::{get_current_member, list_members, Member};
use crate::team::relay::{acknowledge_relay, close_relay, get_relay, list_relay_drafts, list_relays, publish_relay_draft};
use crate::team::specs::{get_spec, list_specs};
use crate::wiki::{WikiEntity, WikiIndex};

const MAX_GRAPH_ENTITIES: usize = 500;
const MAX_SNIPPET_LINES: i64 = 160;
const MAX_NOTE_LEN: usize = 4000;
/// Related / backlink entities returned by `/api/wiki/entity` (the rest is counted).
const MAX_ENTITY_RELATIONS: usize = 50;

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

pub(crate) fn json_ok(v: Value) -> Response {
    (StatusCode::OK, Json(v)).into_response()
}

pub(crate) fn json_err(status: StatusCode, msg: impl Into<String>) -> Response {
    super::problem::Problem::from_status(status, msg.into()).into_response()
}

/// Map an error string from the team/drift layer to an HTTP status.
fn domain_err(msg: String) -> Response {
    let lower = msg.to_lowercase();
    let status = if lower.contains("not found") {
        StatusCode::NOT_FOUND
    } else if lower.contains("not pending")
        || lower.contains("already")
        || lower.contains("not a named recipient")
        || lower.contains("not the sender")
    {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    };
    json_err(status, msg)
}

/// Run blocking file/SQLite work off the async executor.
async fn blocking<F>(f: F) -> Response
where
    F: FnOnce() -> Response + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(r) => r,
        Err(_) => json_err(StatusCode::INTERNAL_SERVER_ERROR, "Hub worker failed"),
    }
}

fn open_graph(config: &KnobyteConfig) -> Option<GraphEngine> {
    let path = config.graph_db_path();
    if !path.exists() {
        return None;
    }
    // One immutable snapshot for the whole request: every query answers from one publication.
    let engine = GraphEngine::open(&path).ok()?;
    let _ = engine.pin_snapshot();
    Some(engine)
}

/// The wiki index, read-only: Hub reads never create, migrate or rewrite it.
fn open_wiki(config: &KnobyteConfig) -> Option<WikiIndex> {
    let path = config.wiki_db_path();
    if !path.exists() {
        return None;
    }
    WikiIndex::open_read_only(&path).ok()
}

/// Resolve grounding references (graph ids or readable `kind:path:name` refs) to code nodes,
/// keyed by the reference as written. Ambiguous references stay unresolved.
fn resolve_groundings(config: &KnobyteConfig, refs: &[String]) -> HashMap<String, Node> {
    let path = config.graph_db_path();
    let mut out = HashMap::new();
    if !path.exists() {
        return out;
    }
    let Ok(conn) = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return out;
    };
    for r in refs {
        if out.contains_key(r) {
            continue;
        }
        if let Ok(crate::graph::grounding::RefResolution::Resolved(n)) = crate::graph::grounding::resolve_grounding_ref(&conn, r) {
            out.insert(r.clone(), *n);
        }
    }
    out
}

/// Lightweight error carried through helpers and turned into a JSON response.
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        json_err(self.0, self.1)
    }
}

fn require_current_member(config: &KnobyteConfig) -> Result<Member, ApiError> {
    get_current_member(config).ok_or_else(|| {
        ApiError(
            StatusCode::CONFLICT,
            "No current member is selected for this checkout. Run `knobyte member select <id>` \
             (or `knobyte member add`) and retry."
                .to_string(),
        )
    })
}

#[derive(Debug, Default, Deserialize)]
struct NoteBody {
    #[serde(default)]
    note: Option<String>,
}

fn parse_note(body: &Bytes) -> Result<Option<String>, ApiError> {
    if body.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(None);
    }
    let parsed: NoteBody = serde_json::from_slice(body)
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, format!("Invalid JSON body: {}", e)))?;
    let note = parsed.note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty());
    if note.as_ref().is_some_and(|n| n.len() > MAX_NOTE_LEN) {
        return Err(ApiError(StatusCode::BAD_REQUEST, "Note is too long".to_string()));
    }
    Ok(note)
}

fn member_json(m: &Option<Member>) -> Value {
    match m {
        Some(m) => json!({ "id": m.id, "displayName": m.display_name, "role": m.role }),
        None => Value::Null,
    }
}

/// Read the source lines of a code node from the project root (confined).
fn node_snippet(config: &KnobyteConfig, node: &Node) -> Option<Value> {
    let path = resolve_confined_path(&config.project_root, &node.file_path).ok()?;
    let content = std::fs::read_to_string(path).ok()?;
    let start = node.start_line.max(1);
    let end = node.end_line.max(start);
    let truncated = end - start + 1 > MAX_SNIPPET_LINES;
    let last = if truncated { start + MAX_SNIPPET_LINES - 1 } else { end };
    let lines: Vec<&str> = content
        .lines()
        .skip((start - 1) as usize)
        .take((last - start + 1) as usize)
        .collect();
    Some(json!({
        "startLine": start,
        "endLine": last,
        "truncated": truncated,
        "code": lines.join("\n"),
    }))
}

fn node_summary(n: &Node) -> Value {
    json!({
        "id": n.id,
        "name": n.name,
        "qualifiedName": n.qualified_name,
        "kind": n.kind,
        "file": n.file_path,
        "language": n.language,
        "startLine": n.start_line,
        "endLine": n.end_line,
        "signature": n.signature,
    })
}

fn entity_summary(e: &WikiEntity) -> Value {
    json!({
        "id": e.id,
        "title": e.title,
        "type": e.entity_type,
        "status": e.status,
        "summary": e.summary,
        "file": e.file,
    })
}

// ---------------------------------------------------------------------------
// overview / fleet / contributors / feed
// ---------------------------------------------------------------------------

fn graph_journal_mode(config: &KnobyteConfig) -> Option<String> {
    let path = config.graph_db_path();
    if !path.exists() {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.query_row("PRAGMA journal_mode", [], |r| r.get::<_, String>(0)).ok()
}

pub async fn overview(State(state): State<HubState>) -> Response {
    blocking(move || {
        let config = &state.config;
        let drift = run_drift_check(config);
        let graph_status = open_graph(config).and_then(|g| g.status().ok());
        let wiki_count = open_wiki(config).and_then(|w| w.entity_count().ok()).unwrap_or(0);
        let relays = list_relays(config);
        let open_relays = relays.iter().filter(|r| r.status != "closed").count();
        let pending = list_inbox_proposals(config)
            .iter()
            .filter(|p| p.status == PROPOSAL_STATUS_PENDING)
            .count();
        // The Hub is not the MCP server: it reports the profile a server started here without
        // --profile would use (KNOBYTE_MCP_PROFILE, then mcp.profile, else core).
        let profile = crate::mcp::resolve_profile_for(None, &config.scaffold_root);
        let active = profile.as_ref().map(|r| r.profile).unwrap_or_default();
        let tools: Vec<Value> = crate::mcp::get_tools_list()
            .into_iter()
            .map(|t| {
                let profiles: Vec<&str> =
                    crate::mcp::profiles::profiles_including(&t.name).into_iter().map(|p| p.name()).collect();
                json!({ "name": t.name, "description": t.description, "profiles": profiles, "active": active.includes(&t.name) })
            })
            .collect();
        let profiles: Vec<Value> = crate::mcp::McpProfile::ALL
            .iter()
            .map(|p| json!({ "name": p.name(), "summary": p.summary(), "tools": p.tool_names() }))
            .collect();
        let mcp_profile = json!({
            "name": active.name(),
            "source": profile.as_ref().map(|r| r.source.to_string()).unwrap_or_else(|_| "default".into()),
            "error": profile.as_ref().err(),
            "toolCount": active.tool_names().len(),
            "profiles": profiles,
        });
        let heartbeat = check_heartbeat(config, 14);
        json_ok(json!({
            "product": "knobyte",
            "version": crate::version::VERSION,
            "repo": config.project_name(),
            "projectRoot": config.project_root,
            "mode": config.mode,
            "bind": state.bind_addr.as_str(),
            "authRequired": state.security.access_token.is_some(),
            "currentMember": member_json(&get_current_member(config)),
            "memberCount": list_members(config).len(),
            "graph": graph_status,
            "graphJournalMode": graph_journal_mode(config),
            "wikiCount": wiki_count,
            "drift": {
                "score": drift.score,
                "status": drift.status,
                "fileCount": drift.file_count,
                "issueCount": drift.issue_count,
                "grounding": drift.grounding,
            },
            "heartbeat": { "ok": heartbeat.ok, "staleFiles": heartbeat.stale_files.len() },
            "relays": { "total": relays.len(), "open": open_relays },
            "inbox": { "pending": pending },
            "mcpTools": tools,
            "mcpProfile": mcp_profile,
        }))
    })
    .await
}

pub async fn fleet(State(state): State<HubState>) -> Response {
    blocking(move || {
        let projects = discover_projects_at(&state.registry_path, &state.config);
        let contributors = aggregate_contributors(&projects, &state.config);
        let available: Vec<_> = projects.iter().filter(|p| p.available).collect();
        let count = |s: &str| projects.iter().filter(|p| p.status == s).count();
        let aggregate = if available.is_empty() {
            None
        } else {
            Some(available.iter().map(|p| p.drift_score).sum::<f64>() / available.len() as f64)
        };
        json_ok(json!({
            "projects": projects,
            "stats": {
                "totalProjects": projects.len(),
                "healthyProjects": count("healthy"),
                "warningProjects": count("warning"),
                "driftedProjects": count("drifted"),
                "unavailableProjects": count("unavailable"),
                "aggregateDrift": aggregate,
                "totalNodes": available.iter().map(|p| p.node_count).sum::<usize>(),
                "totalEdges": available.iter().map(|p| p.edge_count).sum::<usize>(),
                "totalContributors": contributors.len(),
            }
        }))
    })
    .await
}

pub async fn projects(State(state): State<HubState>) -> Response {
    blocking(move || json_ok(json!(discover_projects_at(&state.registry_path, &state.config)))).await
}

pub async fn contributors(State(state): State<HubState>) -> Response {
    blocking(move || {
        let contributors = aggregate_contributors(&[], &state.config);
        json_ok(json!({
            "contributors": contributors,
            "project": state.config.project_name(),
            "hint": if contributors.is_empty() {
                Some("No team members are registered. Add one with `knobyte member add <id> --name \"Your Name\" --select`.")
            } else {
                None
            },
        }))
    })
    .await
}

pub async fn contributor_detail(AxumPath(id): AxumPath<String>, State(state): State<HubState>) -> Response {
    blocking(move || {
        match aggregate_contributors(&[], &state.config)
            .into_iter()
            .find(|c| c.id == id || c.display_name == id)
        {
            Some(c) => json_ok(json!(c)),
            None => json_err(StatusCode::NOT_FOUND, "Contributor not found"),
        }
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct FeedQuery {
    limit: Option<usize>,
    /// Items to skip (server-side paging).
    offset: Option<usize>,
    /// Only entries at or after: RFC 3339, `YYYY-MM-DD`, or relative `Nd` / `Nh`.
    since: Option<String>,
    /// all | decision | discovery | risk | team
    kind: Option<String>,
}

/// The activity feed: recorded events and team activity, newest first, paged on the server
/// (`limit`/`offset`, `nextOffset`), filtered by `since` and `kind`, each entry with its
/// context (entity, workstream, subjects, files, origin).
pub async fn feed(State(state): State<HubState>, Query(q): Query<FeedQuery>) -> Response {
    let limit = q.limit.unwrap_or(80).clamp(1, 500);
    let offset = q.offset.unwrap_or(0);
    let since = match q.since.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => match crate::team::activity::parse_since(s) {
            Ok(t) => Some(t),
            Err(e) => return json_err(StatusCode::BAD_REQUEST, e.detail),
        },
        None => None,
    };
    let kind = q.kind.clone().unwrap_or_else(|| "all".into()).to_lowercase();
    blocking(move || {
        let config = &state.config;
        let mut items: Vec<Value> = Vec::new();
        for ev in read_events(config).iter() {
            items.push(json!({
                "id": format!("ev:{}", ev.id),
                "source": "event",
                "timestamp": ev.timestamp,
                "actor": ev.actor,
                "kind": ev.kind,
                "summary": ev.summary,
                "details": ev.details,
                "files": ev.files,
                "tags": ev.tags,
                "context": { "files": ev.files, "tags": ev.tags },
            }));
        }
        for act in list_activity(config, usize::MAX) {
            items.push(json!({
                "id": format!("act:{}", act.id),
                "source": "activity",
                "timestamp": act.timestamp,
                "actor": act.actor,
                "kind": format!("team:{}", act.action),
                "summary": format!("{}: {}", act.entity_title, act.summary),
                "details": Value::Null,
                "files": Vec::<String>::new(),
                "tags": Vec::<String>::new(),
                "context": {
                    "entityKind": act.entity_kind,
                    "entityId": act.entity_id,
                    "entityTitle": act.entity_title,
                    "workstream": act.workstream,
                    "subjects": act.subjects,
                    "origin": act.origin,
                    "repoState": act.repo_state,
                    "label": act.label,
                },
            }));
        }
        items.retain(|it| {
            let ts = it["timestamp"].as_str().unwrap_or("");
            let after = since.as_ref().map(|s| {
                chrono::DateTime::parse_from_rfc3339(ts).map(|t| t.with_timezone(&chrono::Utc) >= *s).unwrap_or(false)
            });
            let k = it["kind"].as_str().unwrap_or("").to_lowercase();
            let kind_ok = match kind.as_str() {
                "all" | "" => true,
                "team" => k.starts_with("team:"),
                other => k == other,
            };
            after.unwrap_or(true) && kind_ok
        });
        items.sort_by(|a, b| {
            b["timestamp"].as_str().unwrap_or("").cmp(a["timestamp"].as_str().unwrap_or(""))
                .then_with(|| a["id"].as_str().unwrap_or("").cmp(b["id"].as_str().unwrap_or("")))
        });
        let total = items.len();
        let page: Vec<Value> = items.into_iter().skip(offset).take(limit).collect();
        let next = (offset + page.len() < total).then_some(offset + page.len());
        json_ok(json!({
            "items": page, "total": total, "offset": offset, "nextOffset": next,
            "truncated": next.is_some(), "generatedAt": chrono::Utc::now().to_rfc3339(),
        }))
    })
    .await
}

// ---------------------------------------------------------------------------
// legacy read endpoints (kept for API consumers)
// ---------------------------------------------------------------------------

pub async fn status(State(state): State<HubState>) -> Response {
    blocking(move || {
        let drift = run_drift_check(&state.config);
        let heartbeat = check_heartbeat(&state.config, 14);
        json_ok(json!({
            "product": "knobyte",
            "version": crate::version::VERSION,
            "scaffoldRoot": state.config.scaffold_root,
            "mode": state.config.mode,
            "driftScore": drift.score,
            "heartbeatOk": heartbeat.ok,
            "staleFilesCount": heartbeat.stale_files.len()
        }))
    })
    .await
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WikiEntitiesQuery {
    /// Comma-separated entity types.
    #[serde(rename = "type")]
    pub types: Option<String>,
    /// Comma-separated lifecycle states.
    pub status: Option<String>,
    pub topic: Option<String>,
    pub include_archived: Option<bool>,
    pub include_body: Option<bool>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

fn csv(v: &Option<String>) -> Vec<String> {
    v.as_deref()
        .unwrap_or("")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Bounded, paged entity listing: compact summaries (shadowed duplicates and archived hidden
/// unless `includeArchived`), bodies only with `includeBody`.
pub async fn wiki_entities(
    State(state): State<HubState>,
    Query(q): Query<WikiEntitiesQuery>,
) -> Response {
    blocking(move || {
        let Some(wiki) = open_wiki(&state.config) else {
            return json_ok(json!({ "items": [], "truncated": false, "nextOffset": null }));
        };
        let filter = crate::wiki::index::QueryFilter {
            types: csv(&q.types),
            statuses: csv(&q.status),
            topic: q.topic.clone(),
            include_archived: q.include_archived.unwrap_or(false),
            limit: q.limit,
            offset: q.offset.unwrap_or(0),
            ..Default::default()
        };
        match wiki.list_filtered(&filter) {
            Ok(page) => {
                let next = page.truncated.then(|| filter.offset + page.items.len());
                let items: Vec<Value> = page
                    .items
                    .into_iter()
                    .map(|s| {
                        let mut v = json!(s);
                        if q.include_body.unwrap_or(false) {
                            if let Ok(Some(e)) = wiki.show(&s.id) {
                                v["body"] = json!(e.body);
                            }
                        }
                        v
                    })
                    .collect();
                json_ok(json!({ "items": items, "truncated": page.truncated, "nextOffset": next }))
            }
            Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    })
    .await
}

pub async fn graph_status(State(state): State<HubState>) -> Response {
    blocking(move || match open_graph(&state.config).map(|g| g.status()) {
        Some(Ok(st)) => json_ok(json!(st)),
        Some(Err(e)) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        None => json_err(StatusCode::NOT_FOUND, "Code graph not built yet. Run `knobyte graph rebuild`."),
    })
    .await
}

pub async fn team_members(State(state): State<HubState>) -> Response {
    blocking(move || json_ok(json!(list_members(&state.config)))).await
}

pub async fn team_activity(State(state): State<HubState>) -> Response {
    blocking(move || json_ok(json!(list_activity(&state.config, 50)))).await
}

// ---------------------------------------------------------------------------
// Understand the project: context graph, entity / code detail, search
// ---------------------------------------------------------------------------

pub async fn graph_context(State(state): State<HubState>) -> Response {
    blocking(move || {
        let config = &state.config;
        let wiki = match open_wiki(config) {
            Some(w) => w,
            None => {
                return json_ok(json!({
                    "nodes": [], "edges": [], "truncated": false,
                    "hint": "Wiki index not built yet. Run `knobyte wiki rebuild-index`."
                }))
            }
        };
        let page = wiki
            .list_filtered(&crate::wiki::index::QueryFilter { limit: Some(MAX_GRAPH_ENTITIES), ..Default::default() })
            .unwrap_or(crate::wiki::index::Page { items: Vec::new(), truncated: false });
        let truncated = page.truncated;
        let entities: Vec<WikiEntity> = page.items.iter().filter_map(|s| wiki.show(&s.id).ok().flatten()).collect();

        let entity_ids: HashSet<&str> = entities.iter().map(|e| e.id.as_str()).collect();
        let mut nodes: Vec<Value> = Vec::new();
        let mut edges: Vec<Value> = Vec::new();

        for e in &entities {
            nodes.push(json!({
                "id": format!("e:{}", e.id),
                "group": "entity",
                "label": e.title,
                "entityId": e.id,
                "entityType": e.entity_type,
                "status": e.status,
            }));
            for r in &e.relations {
                if entity_ids.contains(r.target_id.as_str()) {
                    edges.push(json!({
                        "source": format!("e:{}", e.id),
                        "target": format!("e:{}", r.target_id),
                        "kind": r.rel_type,
                    }));
                }
            }
        }

        // Resolve groundings to code nodes.
        let mut all_ground_ids: Vec<String> = entities.iter().flat_map(|e| e.grounds_to.clone()).collect();
        all_ground_ids.sort();
        all_ground_ids.dedup();
        let resolved = resolve_groundings(config, &all_ground_ids);
        for gid in &all_ground_ids {
            match resolved.get(gid) {
                Some(n) => nodes.push(json!({
                    "id": format!("c:{}", gid),
                    "group": "code",
                    "label": n.name,
                    "nodeId": gid,
                    "resolvedId": n.id,
                    "kind": n.kind,
                    "file": n.file_path,
                })),
                None => nodes.push(json!({
                    "id": format!("c:{}", gid),
                    "group": "missing",
                    "label": gid,
                    "nodeId": gid,
                })),
            }
        }
        for e in &entities {
            for gid in &e.grounds_to {
                edges.push(json!({
                    "source": format!("e:{}", e.id),
                    "target": format!("c:{}", gid),
                    "kind": "grounds_to",
                }));
            }
        }

        json_ok(json!({ "nodes": nodes, "edges": edges, "truncated": truncated }))
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct IdQuery {
    id: String,
}

pub async fn wiki_entity(State(state): State<HubState>, Query(q): Query<IdQuery>) -> Response {
    blocking(move || {
        let config = &state.config;
        let wiki = match open_wiki(config) {
            Some(w) => w,
            None => return json_err(StatusCode::NOT_FOUND, "Wiki index not built yet"),
        };
        let entity = match wiki.show(&q.id) {
            Ok(Some(e)) => e,
            Ok(None) => return json_err(StatusCode::NOT_FOUND, format!("Entity '{}' not found", q.id)),
            Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        // Bounded locally until the wiki index offers a paged relations API.
        let related_all = wiki.related(&entity.id).unwrap_or_default();
        let backlinks_all = wiki.backlinks(&entity.id).unwrap_or_default();
        let related: Vec<Value> = related_all.iter().take(MAX_ENTITY_RELATIONS).map(entity_summary).collect();
        let backlinks: Vec<Value> = backlinks_all.iter().take(MAX_ENTITY_RELATIONS).map(entity_summary).collect();

        let graph = open_graph(config);
        let nodes = resolve_groundings(config, &entity.grounds_to);
        let health: HashMap<String, (Option<String>, Option<String>)> = wiki
            .groundings_for(&entity.id)
            .unwrap_or_default()
            .into_iter()
            .map(|(r, h, st)| (r, (h, st)))
            .collect();
        let groundings: Vec<Value> = entity
            .grounds_to
            .iter()
            .map(|gid| match nodes.get(gid) {
                Some(n) => json!({
                    "nodeId": gid,
                    "resolvedId": n.id,
                    "health": health.get(gid).and_then(|x| x.0.clone()),
                    "resolved": true,
                    "node": n,
                    "snippet": node_snippet(config, n),
                }),
                None => json!({ "nodeId": gid, "resolved": false,
                    "health": health.get(gid).and_then(|x| x.0.clone()), "state": health.get(gid).and_then(|x| x.1.clone()) }),
            })
            .collect();

        json_ok(json!({
            "entity": entity,
            "related": related,
            "relatedTotal": related_all.len(),
            "relatedTruncated": related_all.len() > MAX_ENTITY_RELATIONS,
            "backlinks": backlinks,
            "backlinksTotal": backlinks_all.len(),
            "backlinksTruncated": backlinks_all.len() > MAX_ENTITY_RELATIONS,
            "groundings": groundings,
            "graphAvailable": graph.is_some(),
        }))
    })
    .await
}

pub async fn code_node(State(state): State<HubState>, Query(q): Query<IdQuery>) -> Response {
    blocking(move || {
        let config = &state.config;
        let graph = match open_graph(config) {
            Some(g) => g,
            None => return json_err(StatusCode::NOT_FOUND, "Code graph not built yet. Run `knobyte graph rebuild`."),
        };
        let node = match super::explore::resolve_code_node(&graph, &q.id) {
            Ok(n) => n,
            Err(p) => return p.into_response(),
        };
        let callers: Vec<Value> = graph
            .query_who_calls(&node.name)
            .unwrap_or_default()
            .iter()
            .filter(|c| c.id != node.id)
            .take(25)
            .map(node_summary)
            .collect();
        let entities: Vec<Value> = open_wiki(config)
            .and_then(|w| w.for_code(&node.id).ok())
            .unwrap_or_default()
            .iter()
            .map(entity_summary)
            .collect();
        json_ok(json!({
            "node": node,
            "snippet": node_snippet(config, &node),
            "callers": callers,
            "entities": entities,
        }))
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    q: String,
}

pub async fn search(State(state): State<HubState>, Query(q): Query<SearchQuery>) -> Response {
    let text = q.q.trim().chars().take(200).collect::<String>();
    if text.is_empty() {
        return json_ok(json!({ "wiki": [], "code": [] }));
    }
    blocking(move || {
        let config = &state.config;
        let wiki: Vec<Value> = open_wiki(config)
            .and_then(|w| w.query(&text).ok())
            .unwrap_or_default()
            .iter()
            .take(25)
            .map(entity_summary)
            .collect();

        let mut code: Vec<Value> = Vec::new();
        if let Some(g) = open_graph(config) {
            let mut seen = HashSet::new();
            for n in g.query_where_defined(&text).unwrap_or_default() {
                if seen.insert(n.id.clone()) {
                    code.push(node_summary(&n));
                }
            }
            if code.len() < 25 {
                for s in g.query_scope_explained(&text).unwrap_or_default() {
                    if code.len() >= 25 {
                        break;
                    }
                    if seen.insert(s.node.id.clone()) {
                        code.push(node_summary(&s.node));
                    }
                }
            }
            code.truncate(25);
        }
        json_ok(json!({ "wiki": wiki, "code": code }))
    })
    .await
}

// ---------------------------------------------------------------------------
// Inbox review
// ---------------------------------------------------------------------------

pub async fn inbox_list(State(state): State<HubState>) -> Response {
    blocking(move || {
        let config = &state.config;
        json_ok(json!({
            "proposals": list_inbox_proposals(config),
            "drafts": list_inbox_drafts(config),
            "currentMember": member_json(&get_current_member(config)),
        }))
    })
    .await
}

/// Read the current content of a proposal target, confined to the scaffold.
fn read_target(config: &KnobyteConfig, target: &str) -> Result<(String, bool, String), String> {
    let rel = normalize_proposal_target(target)?;
    match resolve_confined_path(&config.scaffold_root, &rel) {
        Ok(path) => {
            let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            Ok((rel, true, content))
        }
        Err(e) if e.starts_with("File not found") => Ok((rel, false, String::new())),
        Err(e) => Err(e),
    }
}

fn preview_result(existing: &str, exists: bool, mode: &str, proposed: &str) -> String {
    let mut body = proposed.to_string();
    if !body.ends_with('\n') {
        body.push('\n');
    }
    if exists && mode == PROPOSAL_MODE_APPEND {
        let mut out = existing.to_string();
        if !out.is_empty() {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
        out.push_str(&body);
        out
    } else {
        body
    }
}

pub async fn inbox_detail(AxumPath(id): AxumPath<String>, State(state): State<HubState>) -> Response {
    blocking(move || {
        let config = &state.config;
        let proposal = match get_proposal(config, &id) {
            Ok(p) => p,
            Err(e) => return domain_err(e),
        };
        if proposal.status != PROPOSAL_STATUS_PENDING && proposal.status != "stale" {
            // Decided proposals: the target now reflects the decision; a diff
            // against it would be misleading.
            return json_ok(json!({ "proposal": proposal, "target": proposal.target, "decided": true, "diff": [] }));
        }
        match read_target(config, &proposal.target) {
            Ok((rel, exists, current)) => {
                let result = preview_result(&current, exists, &proposal.mode, &proposal.proposed_content);
                let diff = line_diff(&current, &result);
                json_ok(json!({
                    "proposal": proposal,
                    "target": rel,
                    "targetExists": exists,
                    "current": current,
                    "result": result,
                    "diff": diff,
                }))
            }
            Err(e) => json_ok(json!({
                "proposal": proposal,
                "targetError": e,
                "diff": [],
            })),
        }
    })
    .await
}

pub async fn inbox_approve(AxumPath(id): AxumPath<String>, State(state): State<HubState>, body: Bytes) -> Response {
    let note = match parse_note(&body) {
        Ok(n) => n,
        Err(r) => return r.into_response(),
    };
    blocking(move || {
        let config = &state.config;
        let member = match require_current_member(config) {
            Ok(m) => m,
            Err(r) => return r.into_response(),
        };
        match crate::team::inbox::approve_proposal_with(config, &id, &member.id, note.as_deref(), false) {
            Ok(p) => {
                // The approval wrote scaffold Markdown: refresh so the entity is reachable.
                let refresh_error = super::explore::refresh_wiki_after_write(config);
                json_ok(json!({ "proposal": p, "wikiRefreshed": refresh_error.is_none(), "wikiRefreshError": refresh_error }))
            }
            // This legacy route reports an already-decided proposal as 409 (as before);
            // everything else, including SELF_APPROVAL_REQUIRED (403), maps as typed.
            Err(e) if e.detail.contains("is not pending") => json_err(StatusCode::CONFLICT, e.detail),
            Err(e) => super::problem::Problem::from(e).into_response(),
        }
    })
    .await
}

pub async fn inbox_reject(AxumPath(id): AxumPath<String>, State(state): State<HubState>, body: Bytes) -> Response {
    let note = match parse_note(&body) {
        Ok(n) => n,
        Err(r) => return r.into_response(),
    };
    blocking(move || {
        let config = &state.config;
        let member = match require_current_member(config) {
            Ok(m) => m,
            Err(r) => return r.into_response(),
        };
        match reject_proposal(config, &id, &member.id, note.as_deref()) {
            Ok(p) => json_ok(json!({ "proposal": p })),
            Err(e) => domain_err(e),
        }
    })
    .await
}

pub async fn inbox_draft_publish(AxumPath(id): AxumPath<String>, State(state): State<HubState>) -> Response {
    blocking(move || {
        let config = &state.config;
        if let Err(r) = require_current_member(config) {
            return r.into_response();
        }
        match publish_inbox_draft(config, &id) {
            Ok(p) => json_ok(json!({ "proposal": p })),
            Err(e) => domain_err(e),
        }
    })
    .await
}

// ---------------------------------------------------------------------------
// Specs
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpecsQuery {
    /// Comma-separated lifecycle states (in_flight, promoted, deprecated, archived).
    pub lifecycle_states: Option<String>,
    pub include_archived: Option<bool>,
}

/// Specs, filtered by lifecycle: `?lifecycleStates=` keeps exactly those states; without it,
/// archived specs are hidden unless `includeArchived=true`.
pub async fn specs_list(State(state): State<HubState>, Query(q): Query<SpecsQuery>) -> Response {
    blocking(move || {
        let wanted = csv(&q.lifecycle_states);
        const STATES: [&str; 4] = ["in_flight", "promoted", "deprecated", "archived"];
        if let Some(bad) = wanted.iter().find(|w| !STATES.contains(&w.as_str())) {
            return json_err(StatusCode::BAD_REQUEST, format!("Unknown lifecycle state '{}'; use one of: {}", bad, STATES.join(", ")));
        }
        let all = list_specs(&state.config);
        let unfiltered_total = all.len();
        let specs: Vec<_> = all
            .into_iter()
            .filter(|s| {
                let state = if s.lifecycle_state.is_empty() { s.status.as_str() } else { s.lifecycle_state.as_str() };
                if wanted.is_empty() {
                    q.include_archived.unwrap_or(false) || state != "archived"
                } else {
                    wanted.iter().any(|w| w == state)
                }
            })
            .collect();
        // `total` counts what the filter matched; `unfilteredTotal` keeps the full count.
        json_ok(json!({ "specs": specs, "total": specs.len(), "unfilteredTotal": unfiltered_total, "lifecycleStates": wanted }))
    })
    .await
}

pub async fn spec_detail(AxumPath(id): AxumPath<String>, State(state): State<HubState>) -> Response {
    blocking(move || match get_spec(&state.config, &id) {
        Ok(s) => json_ok(json!(s)),
        Err(e) => domain_err(e),
    })
    .await
}

// ---------------------------------------------------------------------------
// Relays
// ---------------------------------------------------------------------------

pub async fn relays_list(State(state): State<HubState>) -> Response {
    blocking(move || {
        let config = &state.config;
        let mut relays = list_relays(config);
        relays.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        json_ok(json!({
            "relays": relays,
            "drafts": list_relay_drafts(config),
            "currentMember": member_json(&get_current_member(config)),
        }))
    })
    .await
}

pub async fn relay_detail(AxumPath(id): AxumPath<String>, State(state): State<HubState>) -> Response {
    blocking(move || {
        if crate::team::validate_entity_id(&id).is_err() {
            return json_err(StatusCode::BAD_REQUEST, "Invalid relay id");
        }
        match get_relay(&state.config, &id) {
            Some(r) => json_ok(json!(r)),
            None => json_err(StatusCode::NOT_FOUND, format!("Relay '{}' not found", id)),
        }
    })
    .await
}

pub async fn relay_draft_publish(AxumPath(id): AxumPath<String>, State(state): State<HubState>) -> Response {
    blocking(move || {
        let config = &state.config;
        if let Err(r) = require_current_member(config) {
            return r.into_response();
        }
        match publish_relay_draft(config, &id) {
            Ok(r) => json_ok(json!({ "relay": r })),
            Err(e) => domain_err(e),
        }
    })
    .await
}

pub async fn relay_claim(AxumPath(id): AxumPath<String>, State(state): State<HubState>) -> Response {
    blocking(move || {
        let config = &state.config;
        let member = match require_current_member(config) {
            Ok(m) => m,
            Err(r) => return r.into_response(),
        };
        match acknowledge_relay(config, &id, &member.id) {
            Ok(r) => json_ok(json!({ "relay": r })),
            Err(e) => domain_err(e),
        }
    })
    .await
}

pub async fn relay_close(AxumPath(id): AxumPath<String>, State(state): State<HubState>) -> Response {
    blocking(move || {
        let config = &state.config;
        let member = match require_current_member(config) {
            Ok(m) => m,
            Err(r) => return r.into_response(),
        };
        match close_relay(config, &id, &member.id) {
            Ok(r) => json_ok(json!({ "relay": r })),
            Err(e) => domain_err(e),
        }
    })
    .await
}

// ---------------------------------------------------------------------------
// Drift / self-healing groundings
// ---------------------------------------------------------------------------

pub async fn drift_report(State(state): State<HubState>) -> Response {
    blocking(move || json_ok(json!(run_drift_check(&state.config)))).await
}

#[derive(Debug, Deserialize)]
struct SyncBody {
    #[serde(default = "default_true", rename = "dryRun")]
    dry_run: bool,
}

fn default_true() -> bool {
    true
}

/// `POST /api/drift/sync` with `{"dryRun": true|false}` (default: dry run).
pub async fn drift_sync(State(state): State<HubState>, body: Bytes) -> Response {
    let dry_run = if body.iter().all(|b| b.is_ascii_whitespace()) {
        true
    } else {
        match serde_json::from_slice::<SyncBody>(&body) {
            Ok(b) => b.dry_run,
            Err(e) => return json_err(StatusCode::BAD_REQUEST, format!("Invalid JSON body: {}", e)),
        }
    };
    blocking(move || {
        let config = &state.config;
        match crate::drift::sync_groundings(config, dry_run) {
            Ok(result) => {
                let after = if dry_run { None } else { Some(run_drift_check(config)) };
                json_ok(json!({ "result": result, "driftAfter": after }))
            }
            Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, e),
        }
    })
    .await
}
