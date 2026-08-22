//! Team reads (paged) and the two-step team mutation protocol used by every
//! Hub write: `POST /api/team/operations/preview` returns a signed envelope
//! describing the exact file changes; `POST /api/team/operations/apply` applies
//! that envelope and refuses if the actor, any touched file, or the planned
//! result changed since the preview.

use axum::{
    body::Bytes,
    extract::{Path as AxumPath, State},
    response::{IntoResponse, Response},
    Json,
};
use super::problem::Query;
use serde::Deserialize;
use serde_json::{json, Value};

use super::problem::Problem;
use super::HubState;
use crate::team::workflow::{apply, preview, ActorChoice, PreviewEnvelope, RevisionExpectation, TeamCommand};
use crate::team::{resolve_actor, TeamError};

const MAX_BODY: usize = 512 * 1024;

async fn blocking<F>(f: F) -> Response
where
    F: FnOnce() -> Response + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|_| Problem::internal("Hub worker failed").into_response())
}

fn team(r: Result<Value, TeamError>) -> Response {
    match r {
        Ok(v) => Json(v).into_response(),
        Err(e) => Problem::from(e).into_response(),
    }
}

fn actor_choice(member: &Option<String>) -> ActorChoice {
    match member.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(m) => ActorChoice::member(m),
        None => ActorChoice::resolved(),
    }
}

/// Action kinds the Hub exposes (everything the workflow supports).
const ALLOWED_PREFIXES: &[&str] = &["member.", "workstream.", "inbox.", "relay.", "playbook.", "catchup."];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewBody {
    action: Value,
    #[serde(default, rename = "operationId")]
    operation_id: Option<String>,
    #[serde(default, rename = "expectedRevisions")]
    expected_revisions: Vec<RevisionExpectation>,
    #[serde(default, rename = "actorMemberId")]
    actor_member_id: Option<String>,
}

pub async fn preview_operation(State(state): State<HubState>, body: Bytes) -> Response {
    if body.len() > MAX_BODY {
        return Problem::bad_request("Request body too large").into_response();
    }
    let b: PreviewBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return Problem::bad_request(format!("Invalid preview request: {}", e)).into_response(),
    };
    let kind = b.action.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string();
    if !ALLOWED_PREFIXES.iter().any(|p| kind.starts_with(p)) {
        return Problem::bad_request(format!("Unsupported team action '{}'", kind)).into_response();
    }
    blocking(move || {
        let mut cmd = TeamCommand::new(b.action);
        if let Some(op) = b.operation_id {
            cmd.operation_id = op;
        }
        cmd.expected_revisions = b.expected_revisions;
        team(preview(&state.config, &cmd, &actor_choice(&b.actor_member_id)).map(|env| json!({ "envelope": env })))
    })
    .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApplyBody {
    envelope: PreviewEnvelope,
    #[serde(default, rename = "actorMemberId")]
    actor_member_id: Option<String>,
}

pub async fn apply_operation(State(state): State<HubState>, body: Bytes) -> Response {
    if body.len() > MAX_BODY {
        return Problem::bad_request("Request body too large").into_response();
    }
    let b: ApplyBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return Problem::bad_request(format!("Invalid apply request: {}", e)).into_response(),
    };
    blocking(move || {
        let writes_wiki = b.envelope.request.action.get("kind").and_then(|k| k.as_str()) == Some("inbox.approve");
        let result = apply(&state.config, &b.envelope, &actor_choice(&b.actor_member_id)).map(|r| json!(r));
        if writes_wiki && result.is_ok() {
            // An approval writes scaffold Markdown: refresh so the new entity is reachable.
            let _ = super::explore::refresh_wiki_after_write(&state.config);
        }
        team(result)
    })
    .await
}

/// `GET /api/actor`: who the Hub acts as, and the members one can switch to.
pub async fn current_actor(State(state): State<HubState>) -> Response {
    blocking(move || {
        let config = &state.config;
        let res = resolve_actor(config);
        let member = res.actor.member_id().and_then(|id| crate::team::members::get_member(config, id));
        let members: Vec<Value> = crate::team::members::list_members(config)
            .into_iter()
            .filter(|m| m.is_active())
            .map(|m| json!({ "id": m.id, "displayName": m.display_name, "role": m.role }))
            .collect();
        Json(json!({
            "actor": res.actor,
            "actorId": res.actor.id(),
            "source": res.source,
            "member": member,
            "members": members,
            "diagnostics": res.diagnostics,
        }))
        .into_response()
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct PageQuery {
    cursor: Option<String>,
    limit: Option<usize>,
    active: Option<bool>,
    state: Option<String>,
    #[serde(rename = "includeArchived")]
    include_archived: Option<bool>,
    perspective: Option<String>,
    workstream: Option<String>,
    topic: Option<String>,
    playbook: Option<String>,
}

fn states(q: &Option<String>) -> Vec<String> {
    q.as_deref()
        .map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect())
        .unwrap_or_default()
}

pub async fn members_page(State(state): State<HubState>, Query(q): Query<PageQuery>) -> Response {
    blocking(move || {
        team(
            crate::team::members::list_members_page(&state.config, q.active, q.cursor.as_deref(), q.limit)
                .map(|p| json!(p)),
        )
    })
    .await
}

pub async fn member_detail(State(state): State<HubState>, AxumPath(id): AxumPath<String>) -> Response {
    blocking(move || match crate::team::members::get_member(&state.config, &id) {
        Some(m) => {
            let activity: Vec<Value> = crate::team::activity::list_activity(&state.config, 200)
                .into_iter()
                .filter(|a| a.actor == m.id)
                .take(25)
                .map(|a| json!(a))
                .collect();
            Json(json!({ "member": m, "activity": activity })).into_response()
        }
        None => Problem::not_found(format!("Member '{}' not found", id)).into_response(),
    })
    .await
}

pub async fn workstreams_page(State(state): State<HubState>, Query(q): Query<PageQuery>) -> Response {
    blocking(move || {
        team(
            crate::team::workstreams::list_workstreams_page(
                &state.config,
                &states(&q.state),
                q.include_archived.unwrap_or(false),
                q.cursor.as_deref(),
                q.limit,
            )
            .map(|p| json!(p)),
        )
    })
    .await
}

pub async fn workstream_detail(State(state): State<HubState>, AxumPath(id): AxumPath<String>) -> Response {
    blocking(move || match crate::team::workstreams::get_workstream(&state.config, &id) {
        Some(w) => {
            let relays: Vec<Value> = crate::team::relay::list_relays(&state.config)
                .into_iter()
                .filter(|r| r.workstream.as_deref() == Some(w.id.as_str()))
                .map(|r| json!({ "id": r.id, "title": r.title, "status": r.status, "sender": r.sender, "updatedAt": r.updated_at }))
                .collect();
            Json(json!({ "workstream": w, "relays": relays })).into_response()
        }
        None => Problem::not_found(format!("Workstream '{}' not found", id)).into_response(),
    })
    .await
}

pub async fn inbox_drafts_page(State(state): State<HubState>, Query(q): Query<PageQuery>) -> Response {
    blocking(move || {
        team(crate::team::inbox::list_inbox_drafts_page(&state.config, q.cursor.as_deref(), q.limit).map(|p| json!(p)))
    })
    .await
}

pub async fn inbox_draft_detail(State(state): State<HubState>, AxumPath(id): AxumPath<String>) -> Response {
    blocking(move || {
        if crate::team::validate_entity_id(&id).is_err() {
            return Problem::bad_request("Invalid draft id").into_response();
        }
        match crate::team::inbox::get_inbox_draft(&state.config, &id) {
            Some(d) => Json(json!(d)).into_response(),
            None => Problem::not_found(format!("Inbox draft '{}' not found", id)).into_response(),
        }
    })
    .await
}

pub async fn inbox_proposals_page(State(state): State<HubState>, Query(q): Query<PageQuery>) -> Response {
    blocking(move || {
        team(
            crate::team::inbox::list_inbox_proposals_page(&state.config, &states(&q.state), q.cursor.as_deref(), q.limit)
                .map(|p| json!(p)),
        )
    })
    .await
}

pub async fn relays_page(State(state): State<HubState>, Query(q): Query<PageQuery>) -> Response {
    blocking(move || {
        team(
            crate::team::relay::list_relays_page(
                &state.config,
                q.perspective.as_deref(),
                &states(&q.state),
                q.workstream.as_deref(),
                q.cursor.as_deref(),
                q.limit,
            )
            .map(|p| json!(p)),
        )
    })
    .await
}

pub async fn playbooks_page(State(state): State<HubState>, Query(q): Query<PageQuery>) -> Response {
    blocking(move || {
        team(
            crate::team::playbooks::list_playbooks_page(
                &state.config,
                &states(&q.state),
                q.topic.as_deref(),
                q.include_archived.unwrap_or(false),
                q.cursor.as_deref(),
                q.limit,
            )
            .map(|p| json!(p)),
        )
    })
    .await
}

pub async fn playbook_detail(State(state): State<HubState>, AxumPath(id): AxumPath<String>) -> Response {
    blocking(move || {
        if crate::team::validate_entity_id(&id).is_err() {
            return Problem::bad_request("Invalid playbook id").into_response();
        }
        team(crate::team::playbooks::playbook_detail(&state.config, &id))
    })
    .await
}

pub async fn playbook_runs_page(State(state): State<HubState>, Query(q): Query<PageQuery>) -> Response {
    blocking(move || {
        let f = crate::team::playbooks::RunFilter { playbook: q.playbook.clone(), workstream: q.workstream.clone(), states: states(&q.state) };
        team(crate::team::playbooks::list_runs_page(&state.config, &f, q.cursor.as_deref(), q.limit).map(|p| {
            let items: Vec<Value> = p.items.iter().map(crate::team::playbooks::run_summary).collect();
            json!({ "items": items, "nextCursor": p.next_cursor, "truncated": p.truncated, "total": p.total, "deterministicRevision": p.deterministic_revision })
        }))
    })
    .await
}

pub async fn playbook_run_detail(State(state): State<HubState>, AxumPath(id): AxumPath<String>) -> Response {
    blocking(move || {
        if crate::team::validate_entity_id(&id).is_err() {
            return Problem::bad_request("Invalid run id").into_response();
        }
        match crate::team::playbooks::get_run(&state.config, &id) {
            Some(r) => Json(json!(r)).into_response(),
            None => Problem::not_found(format!("Playbook run '{}' not found", id)).into_response(),
        }
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct CatchUpQuery {
    since: Option<String>,
    workstream: Option<String>,
    #[serde(rename = "includeMine")]
    include_mine: Option<bool>,
    group: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

/// `GET /api/catch-up`: the digest for the Hub's actor (this checkout's resolved actor).
pub async fn catch_up(State(state): State<HubState>, Query(q): Query<CatchUpQuery>) -> Response {
    blocking(move || {
        let req = crate::team::catchup::CatchUpRequest {
            since: q.since.filter(|s| !s.trim().is_empty()),
            workstream: q.workstream.filter(|s| !s.trim().is_empty()),
            include_mine: q.include_mine.unwrap_or(false),
            groups: states(&q.group),
            cursor: q.cursor,
            limit: q.limit,
        };
        team(crate::team::catchup::catch_up_digest(&state.config, &req).map(|(mut data, diags)| {
            data["diagnostics"] = json!(diags);
            data
        }))
    })
    .await
}

pub async fn relay_draft_detail(State(state): State<HubState>, AxumPath(id): AxumPath<String>) -> Response {
    blocking(move || {
        if crate::team::validate_entity_id(&id).is_err() {
            return Problem::bad_request("Invalid draft id").into_response();
        }
        match crate::team::relay::get_relay_draft(&state.config, &id) {
            Some(d) => Json(json!(d)).into_response(),
            None => Problem::not_found(format!("Relay draft '{}' not found", id)).into_response(),
        }
    })
    .await
}
