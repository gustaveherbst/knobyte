use std::net::SocketAddr;
use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use tower_http::cors::CorsLayer;
use colored::Colorize;

pub mod projects;
use self::projects::{aggregate_contributors, discover_projects};

use crate::config::KnobyteConfig;
use crate::drift::checker::run_drift_check;
use crate::events::read_events;
use crate::graph::GraphEngine;
use crate::heartbeat::check_heartbeat;
use crate::team::activity::list_activity;
use crate::team::members::list_members;
use crate::team::relay::list_relays;
use crate::wiki::WikiIndex;

const DASHBOARD_TEMPLATE: &str = include_str!("dashboard.html");

#[derive(Clone)]
pub struct HubState {
    pub config: KnobyteConfig,
}

pub async fn start_hub_server(config: KnobyteConfig, host: &str, port: u16, open_browser: bool) -> Result<(), Box<dyn std::error::Error>> {
    let state = HubState { config };

    let app = Router::new()
        .route("/", get(dashboard_handler))
        .route("/health", get(health_handler))
        .route("/api/fleet", get(api_fleet_handler))
        .route("/api/projects", get(api_projects_handler))
        .route("/api/contributors", get(api_contributors_handler))
        .route("/api/contributor/{id}", get(api_contributor_detail_handler))
        .route("/api/status", get(api_status_handler))
        .route("/api/wiki/entities", get(api_wiki_entities_handler))
        .route("/api/graph/status", get(api_graph_status_handler))
        .route("/api/team/members", get(api_team_members_handler))
        .route("/api/team/relays", get(api_team_relays_handler))
        .route("/api/team/activity", get(api_team_activity_handler))
        .route("/api/drift/sync", post(api_drift_sync_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", host, port).parse()?;
    println!("{} Knobyte Project Hub running on http://{}", "[hub]".cyan().bold(), addr);
    use std::io::Write;
    let _ = std::io::stdout().flush();

    if open_browser {
        let url = format!("http://{}", addr);
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(&url).spawn();
        #[cfg(target_os = "linux")]
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
        #[cfg(target_os = "windows")]
        let _ = std::process::Command::new("cmd").args(["/C", "start", &url]).spawn();
    }

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok", "hub": "knobyte" })))
}

async fn api_fleet_handler(State(state): State<HubState>) -> impl IntoResponse {
    let projects = discover_projects(&state.config);
    let contributors = aggregate_contributors(&projects, &state.config);

    let healthy_count = projects.iter().filter(|p| p.drift_score >= 95.0).count();
    let warning_count = projects.iter().filter(|p| p.drift_score >= 80.0 && p.drift_score < 95.0).count();
    let drifted_count = projects.iter().filter(|p| p.drift_score < 80.0).count();

    let total_nodes: usize = projects.iter().map(|p| p.node_count).sum();
    let total_edges: usize = projects.iter().map(|p| p.edge_count).sum();

    Json(serde_json::json!({
        "projects": projects,
        "contributors": contributors,
        "stats": {
            "totalProjects": projects.len(),
            "healthyProjects": healthy_count,
            "warningProjects": warning_count,
            "driftedProjects": drifted_count,
            "totalNodes": total_nodes,
            "totalEdges": total_edges,
            "totalContributors": contributors.len()
        }
    }))
}

async fn api_projects_handler(State(state): State<HubState>) -> impl IntoResponse {
    let projects = discover_projects(&state.config);
    Json(serde_json::json!(projects))
}

async fn api_contributors_handler(State(state): State<HubState>) -> impl IntoResponse {
    let projects = discover_projects(&state.config);
    let contributors = aggregate_contributors(&projects, &state.config);
    Json(serde_json::json!(contributors))
}

async fn api_contributor_detail_handler(
    AxumPath(id): AxumPath<String>,
    State(state): State<HubState>,
) -> impl IntoResponse {
    let projects = discover_projects(&state.config);
    let contributors = aggregate_contributors(&projects, &state.config);

    if let Some(c) = contributors.into_iter().find(|c| c.id == id || c.display_name == id) {
        (StatusCode::OK, Json(serde_json::json!(c)))
    } else {
        (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Contributor not found" })))
    }
}

async fn api_status_handler(State(state): State<HubState>) -> impl IntoResponse {
    let drift = run_drift_check(&state.config);
    let heartbeat = check_heartbeat(&state.config, 14);

    Json(serde_json::json!({
        "product": "knobyte",
        "version": crate::version::VERSION,
        "scaffoldRoot": state.config.scaffold_root,
        "mode": state.config.mode,
        "driftScore": drift.score,
        "heartbeatOk": heartbeat.ok,
        "staleFilesCount": heartbeat.stale_files.len()
    }))
}

async fn api_wiki_entities_handler(State(state): State<HubState>) -> impl IntoResponse {
    let index = match WikiIndex::open(&state.config.wiki_db_path()) {
        Ok(idx) => idx,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))),
    };

    match index.list() {
        Ok(entities) => (StatusCode::OK, Json(serde_json::json!(entities))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))),
    }
}

async fn api_graph_status_handler(State(state): State<HubState>) -> impl IntoResponse {
    let engine = match GraphEngine::open(&state.config.graph_db_path()) {
        Ok(e) => e,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))),
    };

    match engine.status() {
        Ok(st) => (StatusCode::OK, Json(serde_json::json!(st))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))),
    }
}

async fn api_team_members_handler(State(state): State<HubState>) -> impl IntoResponse {
    let members = list_members(&state.config);
    Json(serde_json::json!(members))
}

async fn api_team_relays_handler(State(state): State<HubState>) -> impl IntoResponse {
    let relays = list_relays(&state.config);
    Json(serde_json::json!(relays))
}

async fn api_team_activity_handler(State(state): State<HubState>) -> impl IntoResponse {
    let activity = list_activity(&state.config, 50);
    Json(serde_json::json!(activity))
}

async fn api_drift_sync_handler(State(state): State<HubState>) -> impl IntoResponse {
    match crate::drift::sync_groundings(&state.config, false) {
        Ok(result) => (StatusCode::OK, Json(serde_json::json!(result))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e }))),
    }
}

async fn dashboard_handler(State(state): State<HubState>) -> impl IntoResponse {
    Html(render_dashboard_html(&state.config))
}

pub fn render_dashboard_html(config: &KnobyteConfig) -> String {
    let projects = discover_projects(config);
    let contributors = aggregate_contributors(&projects, config);
    let drift = run_drift_check(config);
    let relays = list_relays(config);
    let events = read_events(config);
    let activities = list_activity(config, 30);

    let graph_status = GraphEngine::open(&config.graph_db_path()).ok().and_then(|g| g.status().ok());
    let graph_nodes = graph_status.as_ref().map(|s| s.node_count as usize).unwrap_or(0);
    let graph_edges = graph_status.as_ref().map(|s| s.edge_count as usize).unwrap_or(0);
    let wiki_count = WikiIndex::open(&config.wiki_db_path()).ok().and_then(|w| w.list().ok()).map(|l| l.len()).unwrap_or(0);

    let healthy_projects = projects.iter().filter(|p| p.drift_score >= 95.0).count();
    let warning_projects = projects.iter().filter(|p| p.drift_score >= 80.0 && p.drift_score < 95.0).count();
    let drifted_projects = projects.iter().filter(|p| p.drift_score < 80.0).count();

    // Serialized data for client-side interactivity
    let contributors_json = serde_json::to_string(&contributors).unwrap_or_else(|_| "[]".to_string());
    let projects_json = serde_json::to_string(&projects).unwrap_or_else(|_| "[]".to_string());

    // Project cards
    let project_cards = projects.iter().map(|p| {
        let is_curr = p.is_current;
        let badge = if is_curr {
            r#"<span class="badge badge-accent">ACTIVE</span>"#
        } else {
            r#"<span class="badge badge-neutral">STANDBY</span>"#
        };
        let health_color = if p.drift_score >= 95.0 { "var(--success)" } else if p.drift_score >= 80.0 { "var(--warning)" } else { "var(--danger)" };

        format!(
            r#"<div class="project-card {}" data-name="{}">
                <div class="project-card-header">
                    <div>
                        <div class="project-title">{}</div>
                        <div class="project-path">{}</div>
                    </div>
                    {}
                </div>
                <div class="project-stats-grid">
                    <div>
                        <div class="stat-mini-label">Drift Health</div>
                        <div class="stat-mini-val" style="color: {};">{:.1}%</div>
                    </div>
                    <div>
                        <div class="stat-mini-label">AST Symbols</div>
                        <div class="stat-mini-val">{}</div>
                    </div>
                    <div>
                        <div class="stat-mini-label">Call Edges</div>
                        <div class="stat-mini-val">{}</div>
                    </div>
                    <div>
                        <div class="stat-mini-label">Topics</div>
                        <div class="stat-mini-val">{}</div>
                    </div>
                </div>
                <div class="project-card-footer">
                    <span class="mode-badge">{}</span>
                    <span class="last-active-text">Last touch: {}</span>
                </div>
            </div>"#,
            if is_curr { "card-active" } else { "" },
            p.name.to_lowercase(),
            p.name,
            p.path,
            badge,
            health_color,
            p.drift_score,
            p.node_count,
            p.edge_count,
            p.wiki_count,
            p.mode,
            p.last_active.chars().take(19).collect::<String>().replace('T', " ")
        )
    }).collect::<Vec<_>>().join("\n");

    // Contributor cards (clickable)
    let contributor_cards = contributors.iter().map(|c| {
        let initials: String = c.display_name.split_whitespace().filter_map(|w| w.chars().next()).collect();
        let safe_initials = if initials.is_empty() { "U".to_string() } else { initials };

        let total_contributions = c.decisions_count + c.discoveries_count + c.notes_count + c.relays_authored;
        let in_flight_html = if let Some(ref r) = c.in_flight_relay {
            format!(r#"<div class="contributor-task"><span class="pulse-dot"></span> In-Flight: <strong>{}</strong></div>"#, r)
        } else {
            r#"<div class="contributor-task text-muted">No active relay claimed</div>"#.to_string()
        };

        format!(
            r#"<div class="contributor-card" onclick="openContributorDrawer('{}')" data-name="{}">
                <div class="contributor-header">
                    <div class="avatar">{}</div>
                    <div class="contributor-meta">
                        <div class="contributor-name">{}</div>
                        <div class="contributor-handle">@{}</div>
                    </div>
                    <span class="badge badge-success">{}</span>
                </div>
                <div class="contributor-metrics">
                    <div class="c-metric">
                        <span class="c-val">{}</span>
                        <span class="c-lbl">Decisions</span>
                    </div>
                    <div class="c-metric">
                        <span class="c-val">{}</span>
                        <span class="c-lbl">Discoveries</span>
                    </div>
                    <div class="c-metric">
                        <span class="c-val">{}</span>
                        <span class="c-lbl">Files</span>
                    </div>
                    <div class="c-metric">
                        <span class="c-val">{}</span>
                        <span class="c-lbl">Total</span>
                    </div>
                </div>
                {}
                <div class="click-hint">Click to inspect breakdown & history &rarr;</div>
            </div>"#,
            c.id,
            c.display_name.to_lowercase(),
            safe_initials,
            c.display_name,
            c.git_alias,
            c.status,
            c.decisions_count,
            c.discoveries_count,
            c.files_touched_count,
            total_contributions,
            in_flight_html
        )
    }).collect::<Vec<_>>().join("\n");

    // Timeline / audit entries
    let mut timeline_entries = Vec::new();

    for ev in events.iter().rev().take(30) {
        timeline_entries.push((
            ev.timestamp.clone(),
            ev.actor.clone().unwrap_or_else(|| "contributor".to_string()),
            ev.kind.clone(),
            ev.summary.clone(),
            ev.files.clone(),
        ));
    }

    for act in activities.iter().take(30) {
        timeline_entries.push((
            act.timestamp.clone(),
            act.actor.clone(),
            format!("team:{}", act.action),
            format!("{}: {}", act.entity_title, act.summary),
            Vec::new(),
        ));
    }

    timeline_entries.sort_by(|a, b| b.0.cmp(&a.0));
    timeline_entries.truncate(30);

    let timeline_rows = if timeline_entries.is_empty() {
        r#"<div class="empty-state">No recorded activity yet. Run <code>knobyte log &lt;message&gt;</code> to record a decision.</div>"#.to_string()
    } else {
        timeline_entries.iter().map(|(ts, actor, kind, summary, files)| {
            let files_badges = if files.is_empty() {
                String::new()
            } else {
                let file_spans: Vec<String> = files.iter().take(4).map(|f| {
                    format!(r#"<span class="file-pill">{}</span>"#, f)
                }).collect();
                format!(r#"<div class="file-tags">{}</div>"#, file_spans.join(""))
            };

            let kind_class = match kind.as_str() {
                "decision" => "badge-decision",
                "discovery" => "badge-discovery",
                "risk" => "badge-risk",
                _ => "badge-neutral",
            };

            let formatted_time = ts.chars().take(19).collect::<String>().replace('T', " ");

            format!(
                r#"<div class="activity-row" data-kind="{}" data-actor="{}">
                    <div class="act-col-actor">
                        <div class="actor-title">@{}</div>
                        <span class="badge {}">{}</span>
                    </div>
                    <div class="act-col-body">
                        <div class="act-summary">{}</div>
                        {}
                    </div>
                    <div class="act-col-time">{}</div>
                </div>"#,
                kind.to_lowercase(),
                actor.to_lowercase(),
                actor,
                kind_class,
                kind,
                summary,
                files_badges,
                formatted_time
            )
        }).collect::<Vec<_>>().join("\n")
    };

    let repo_name = config.project_name();

    let healthy_dash = if projects.is_empty() { 0.0 } else { (healthy_projects as f64 / projects.len() as f64) * 100.0 };
    let healthy_space = if projects.is_empty() { 100.0 } else { 100.0 - healthy_dash };
    let warning_dash = if projects.is_empty() { 0.0 } else { (warning_projects as f64 / projects.len() as f64) * 100.0 };
    let warning_space = if projects.is_empty() { 100.0 } else { 100.0 - warning_dash };
    let warning_offset = if projects.is_empty() { 25.0 } else { 25.0 - healthy_dash };

    DASHBOARD_TEMPLATE
        .replace("__VERSION__", crate::version::VERSION)
        .replace("__REPO__", &repo_name)
        .replace("__TOTAL_PROJECTS__", &projects.len().to_string())
        .replace("__TOTAL_CONTRIBUTORS__", &contributors.len().to_string())
        .replace("__PROJECT_CARDS__", &project_cards)
        .replace("__CONTRIBUTOR_CARDS__", &contributor_cards)
        .replace("__TIMELINE_ROWS__", &timeline_rows)
        .replace("__DRIFT_SCORE__", &format!("{:.1}", drift.score))
        .replace("__FILE_COUNT__", &drift.file_count.to_string())
        .replace("__GRAPH_NODES__", &graph_nodes.to_string())
        .replace("__GRAPH_EDGES__", &graph_edges.to_string())
        .replace("__WIKI_COUNT__", &wiki_count.to_string())
        .replace("__RELAYS_COUNT__", &relays.len().to_string())
        .replace("__HEALTHY_PROJECTS__", &healthy_projects.to_string())
        .replace("__WARNING_PROJECTS__", &warning_projects.to_string())
        .replace("__DRIFTED_PROJECTS__", &drifted_projects.to_string())
        .replace("__HEALTHY_DASH__", &format!("{:.1}", healthy_dash))
        .replace("__HEALTHY_SPACE__", &format!("{:.1}", healthy_space))
        .replace("__WARNING_DASH__", &format!("{:.1}", warning_dash))
        .replace("__WARNING_SPACE__", &format!("{:.1}", warning_space))
        .replace("__WARNING_OFFSET__", &format!("{:.1}", warning_offset))
        .replace("__CONTRIBUTORS_JSON__", &contributors_json)
        .replace("__PROJECTS_JSON__", &projects_json)
}
