//! Knobyte Project Hub: a local web workstation served by `knobyte hub`.
//!
//! The page (`dashboard.html`) is a static, data-free shell; the front-end is
//! a set of build-free ES modules under `assets/js/` embedded in the binary.
//! All data comes from the JSON API. Every response passes through
//! [`security::hub_guard`] (Host/Origin checks, session authentication via a
//! one-time bootstrap link, CSRF on POST, strict Content-Security-Policy).
//! Errors are RFC 9457 `application/problem+json` ([`problem`]).

use std::collections::HashMap;
use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{header, HeaderValue, StatusCode},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use colored::Colorize;
use serde::Deserialize;
use serde_json::json;

pub mod api;
pub mod diff;
pub mod explore;
pub mod git;
pub mod jobs;
pub mod knowledge;
pub mod problem;
pub mod projects;
pub mod security;
pub mod setup_wizard;
mod snapshot;
pub mod team_ops;

pub use jobs::{Executor, JobContext, JobFailure, JobKind, JobManager, JobOutput};
pub use problem::Problem;
pub use security::{Auth, HubSecurity, CSRF_HEADER, HUB_TOKEN_ENV_VAR, SESSION_COOKIE_PREFIX};
pub use snapshot::render_dashboard_html;

use crate::config::KnobyteConfig;
use crate::mcp::security::is_loopback_host;

const DASHBOARD_TEMPLATE: &str = include_str!("dashboard.html");

/// Embedded front-end assets: (published path below `/assets/`, content, content type).
const ASSETS: &[(&str, &str, &str)] = &[
    ("app.css", include_str!("assets/app.css"), "text/css; charset=utf-8"),
    ("js/main.js", include_str!("assets/js/main.js"), JS),
    ("js/core.js", include_str!("assets/js/core.js"), JS),
    ("js/lifecycle.js", include_str!("assets/js/lifecycle.js"), JS),
    ("js/team.js", include_str!("assets/js/team.js"), JS),
    ("js/pages/overview.js", include_str!("assets/js/pages/overview.js"), JS),
    ("js/pages/understand.js", include_str!("assets/js/pages/understand.js"), JS),
    ("js/pages/knowledge.js", include_str!("assets/js/pages/knowledge.js"), JS),
    ("js/pages/search.js", include_str!("assets/js/pages/search.js"), JS),
    ("js/pages/symbol.js", include_str!("assets/js/pages/symbol.js"), JS),
    ("js/pages/inbox.js", include_str!("assets/js/pages/inbox.js"), JS),
    ("js/pages/relays.js", include_str!("assets/js/pages/relays.js"), JS),
    ("js/pages/members.js", include_str!("assets/js/pages/members.js"), JS),
    ("js/pages/workstreams.js", include_str!("assets/js/pages/workstreams.js"), JS),
    ("js/pages/playbooks.js", include_str!("assets/js/pages/playbooks.js"), JS),
    ("js/pages/catchup.js", include_str!("assets/js/pages/catchup.js"), JS),
    ("js/pages/specs.js", include_str!("assets/js/pages/specs.js"), JS),
    ("js/pages/activity.js", include_str!("assets/js/pages/activity.js"), JS),
    ("js/pages/groundings.js", include_str!("assets/js/pages/groundings.js"), JS),
    ("js/pages/mcp.js", include_str!("assets/js/pages/mcp.js"), JS),
    ("js/pages/health.js", include_str!("assets/js/pages/health.js"), JS),
    ("js/pages/jobs.js", include_str!("assets/js/pages/jobs.js"), JS),
    ("js/pages/setup.js", include_str!("assets/js/pages/setup.js"), JS),
    ("js/pages/settings.js", include_str!("assets/js/pages/settings.js"), JS),
];
const JS: &str = "text/javascript; charset=utf-8";

/// Default Hub bind host (loopback only).
pub const DEFAULT_HUB_HOST: &str = "127.0.0.1";
/// Default Hub port.
pub const DEFAULT_HUB_PORT: u16 = 4000;

#[derive(Clone)]
pub struct HubState {
    pub config: Arc<KnobyteConfig>,
    pub security: Arc<HubSecurity>,
    /// Path of the fleet registry (`~/.knobyte/projects.json` by default).
    pub registry_path: Arc<PathBuf>,
    /// Address the Hub is bound to (displayed in the UI).
    pub bind_addr: Arc<String>,
    pub jobs: Arc<JobManager>,
    pub setup: Arc<setup_wizard::SetupService>,
}

/// Options for [`build_hub_router`].
#[derive(Clone, Default)]
pub struct HubOptions {
    /// Reusable access token (required for non-loopback binds).
    pub access_token: Option<String>,
    /// Whether the Hub is bound to loopback (enables strict Host validation).
    pub loopback_bind: bool,
    /// Override the fleet registry path (tests); defaults to `~/.knobyte/projects.json`.
    pub registry_path: Option<PathBuf>,
    /// Fixed one-time bootstrap token (tests); random otherwise.
    pub bootstrap_token: Option<String>,
    /// Bind address shown in the UI.
    pub bind_addr: Option<String>,
    /// PATH used to find agent CLIs for setup population (tests use fake CLIs).
    pub agent_path: Option<OsString>,
    /// Replace job executors (tests).
    pub job_executors: HashMap<JobKind, Executor>,
}

/// The router plus the state it serves (so callers can read the bootstrap token).
pub struct HubApp {
    pub router: Router,
    pub state: HubState,
}

/// Build the Hub router (used by [`start_hub_server`] and integration tests).
pub fn build_hub_router(config: KnobyteConfig, options: HubOptions) -> Router {
    build_hub_app(config, options).router
}

pub fn build_hub_app(config: KnobyteConfig, options: HubOptions) -> HubApp {
    let bound_port = options.bind_addr.as_deref().and_then(|a| a.parse::<SocketAddr>().ok()).map(|a| a.port());
    let security = Arc::new(
        HubSecurity::new(options.access_token, options.loopback_bind, options.bootstrap_token).with_bound_port(bound_port),
    );
    let jobs = JobManager::new(&config, options.job_executors);
    let state = HubState {
        config: Arc::new(config),
        security: security.clone(),
        registry_path: Arc::new(options.registry_path.unwrap_or_else(projects::registry_file_path)),
        bind_addr: Arc::new(options.bind_addr.unwrap_or_default()),
        jobs,
        setup: setup_wizard::SetupService::new(options.agent_path),
    };

    let router = Router::new()
        .route("/assets/{*path}", get(asset_handler))
        .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
        .route("/healthz", get(health_handler))
        // session
        .route("/api/session/bootstrap", post(bootstrap_handler))
        .route("/api/session", get(session_handler))
        .route("/api/session/logout", post(logout_handler))
        // shell / overview / fleet
        .route("/api/shell", get(explore::shell))
        .route("/api/home", get(explore::home))
        .route("/api/overview", get(api::overview))
        .route("/api/fleet", get(api::fleet))
        .route("/api/projects", get(api::projects))
        .route("/api/status", get(api::status))
        .route("/api/feed", get(api::feed))
        .route("/api/health", get(explore::health))
        // contributors / team
        .route("/api/contributors", get(api::contributors))
        .route("/api/contributor/{id}", get(api::contributor_detail))
        .route("/api/actor", get(team_ops::current_actor))
        .route("/api/team/members", get(team_ops::members_page))
        .route("/api/team/members/{id}", get(team_ops::member_detail))
        .route("/api/team/relays", get(api::relays_list))
        .route("/api/team/activity", get(api::team_activity))
        .route("/api/team/operations/preview", post(team_ops::preview_operation))
        .route("/api/team/operations/apply", post(team_ops::apply_operation))
        .route("/api/workstreams", get(team_ops::workstreams_page))
        .route("/api/workstreams/{id}", get(team_ops::workstream_detail))
        .route("/api/playbooks", get(team_ops::playbooks_page))
        .route("/api/playbooks/{id}", get(team_ops::playbook_detail))
        .route("/api/playbook-runs", get(team_ops::playbook_runs_page))
        .route("/api/playbook-runs/{id}", get(team_ops::playbook_run_detail))
        .route("/api/catch-up", get(team_ops::catch_up))
        // understand the project
        .route("/api/wiki/entities", get(api::wiki_entities))
        .route("/api/wiki/entity", get(api::wiki_entity))
        .route("/api/wiki/entity/drift", get(knowledge::entity_drift))
        .route("/api/wiki/entity/timeline", get(knowledge::entity_timeline))
        .route("/api/wiki/entity/evidence", get(knowledge::entity_evidence))
        .route("/api/graph/status", get(api::graph_status))
        .route("/api/graph/context", get(api::graph_context))
        .route("/api/code/node", get(api::code_node))
        .route("/api/code/symbol", get(explore::symbol))
        .route("/api/code/symbol/source", get(explore::symbol_source))
        .route("/api/code/symbol/callers", get(explore::symbol_callers))
        .route("/api/code/symbol/callees", get(explore::symbol_callees))
        .route("/api/code/symbol/impact", get(explore::symbol_impact))
        .route("/api/search", get(api::search))
        .route("/api/search/full", get(explore::search))
        // inbox
        .route("/api/inbox", get(api::inbox_list))
        .route("/api/inbox/drafts", get(team_ops::inbox_drafts_page))
        .route("/api/inbox/drafts/{id}", get(team_ops::inbox_draft_detail))
        .route("/api/inbox/proposals", get(team_ops::inbox_proposals_page))
        .route("/api/inbox/{id}", get(api::inbox_detail))
        .route("/api/inbox/{id}/approve", post(api::inbox_approve))
        .route("/api/inbox/{id}/reject", post(api::inbox_reject))
        .route("/api/inbox/drafts/{id}/publish", post(api::inbox_draft_publish))
        // specs
        .route("/api/specs", get(api::specs_list))
        .route("/api/specs/{id}", get(api::spec_detail))
        // relays
        .route("/api/relays", get(api::relays_list))
        .route("/api/relays/page", get(team_ops::relays_page))
        .route("/api/relays/drafts/{id}", get(team_ops::relay_draft_detail))
        .route("/api/relays/{id}", get(api::relay_detail))
        .route("/api/relays/drafts/{id}/publish", post(api::relay_draft_publish))
        .route("/api/relays/{id}/claim", post(api::relay_claim))
        .route("/api/relays/{id}/close", post(api::relay_close))
        // drift / groundings
        .route("/api/drift", get(api::drift_report))
        .route("/api/drift/sync", post(api::drift_sync))
        // jobs
        .route("/api/jobs", get(jobs::list_jobs).post(jobs::start_job))
        .route("/api/jobs/events", get(jobs::lifecycle_events))
        .route("/api/jobs/{id}", get(jobs::get_job))
        .route("/api/jobs/{id}/cancel", post(jobs::cancel_job))
        .route("/api/jobs/{id}/events", get(jobs::job_events))
        // setup wizard
        .route("/api/setup", get(setup_wizard::get_setup).post(setup_wizard::start_setup))
        .route("/api/setup/run", get(setup_wizard::get_run))
        .route("/api/setup/events", get(setup_wizard::run_events))
        .route("/api/setup/git-init", post(setup_wizard::git_init))
        .route("/api/setup/population/preview", post(setup_wizard::population_preview))
        .route("/api/setup/population", post(setup_wizard::start_population))
        .route("/api/setup/population/skip", post(setup_wizard::skip_population))
        .route("/api/setup/cancel", post(setup_wizard::cancel))
        .route("/api/setup/finalize", post(setup_wizard::finalize))
        .route("/api/setup/transcript", get(setup_wizard::transcript))
        .route("/api/setup/transcript/events", get(setup_wizard::transcript_events))
        .route("/api/setup/commit/preview", post(setup_wizard::commit_preview))
        .route("/api/setup/commit/diff", post(setup_wizard::commit_diff))
        .route("/api/setup/commit", post(setup_wizard::commit))
        // settings
        .route("/api/settings/logging", get(setup_wizard::get_logging).post(setup_wizard::set_logging))
        .route("/api/settings/onboarding", get(setup_wizard::get_onboarding).post(setup_wizard::set_onboarding))
        .route("/api", get(api_not_found).post(api_not_found))
        .route("/api/{*rest}", get(api_not_found).post(api_not_found))
        // every other GET serves the shell (client-side routes, 404 page)
        .fallback(page_handler)
        .layer(middleware::from_fn_with_state(security, security::hub_guard))
        .with_state(state.clone());
    HubApp { router, state }
}

pub async fn start_hub_server(config: KnobyteConfig, host: &str, port: u16, open_browser: bool) -> Result<(), Box<dyn std::error::Error>> {
    let configured = std::env::var(HUB_TOKEN_ENV_VAR).ok();
    let (token, generated) = security::resolve_hub_token(host, configured);
    let loopback_bind = is_loopback_host(host);

    let ip_host = host.trim_start_matches('[').trim_end_matches(']');
    let addr: SocketAddr = if ip_host.contains(':') {
        format!("[{}]:{}", ip_host, port).parse()?
    } else {
        format!("{}:{}", ip_host, port).parse()?
    };

    // Refresh this project's registry entry once per Hub start.
    let registry = projects::registry_file_path();
    if config.scaffold_root.is_dir() {
        let _ = projects::register_project_at(&registry, &config, true);
    }

    let app = build_hub_app(
        config,
        HubOptions {
            access_token: token.clone(),
            loopback_bind,
            registry_path: Some(registry),
            bind_addr: Some(addr.to_string()),
            ..Default::default()
        },
    );

    // An unspecified bind (0.0.0.0 / ::) is reachable locally via loopback.
    let display_authority = if addr.ip().is_unspecified() {
        format!("127.0.0.1:{}", port)
    } else {
        addr.to_string()
    };
    let bootstrap_url = format!("http://{}/#token={}", display_authority, app.state.security.bootstrap_token());

    println!("{} Knobyte Project Hub running on http://{}", "[hub]".cyan().bold(), addr);
    println!("{} One-time sign-in link (valid for 5 minutes):", "[hub]".cyan().bold());
    println!("      {}", bootstrap_url);
    if !loopback_bind {
        eprintln!(
            "{} Hub is bound to a non-loopback address ({}); an access token is required.",
            "[hub]".yellow().bold(),
            addr
        );
    }
    if let Some(t) = &token {
        if generated || !loopback_bind {
            eprintln!(
                "{} Reusable access link: http://{}/#token={}",
                "[hub]".yellow().bold(),
                display_authority,
                t
            );
            if addr.ip().is_unspecified() {
                eprintln!(
                    "{} From other machines, replace 127.0.0.1 with this machine's address.",
                    "[hub]".yellow().bold()
                );
            }
        } else {
            eprintln!("{} Access token also accepted (from {}).", "[hub]".yellow().bold(), HUB_TOKEN_ENV_VAR);
        }
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();

    let listener = tokio::net::TcpListener::bind(&addr).await?;

    if open_browser {
        let url = bootstrap_url;
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(&url).spawn();
        #[cfg(target_os = "linux")]
        let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
        #[cfg(target_os = "windows")]
        let _ = std::process::Command::new("cmd").args(["/C", "start", "", &url]).spawn();
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        let _ = url;
    } else {
        println!("{} Browser not opened (--no-open); open the link above.", "[hub]".cyan().bold());
    }

    axum::serve(listener, app.router).await?;
    Ok(())
}

/// Escape text for safe inclusion in HTML text and attribute values.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render the Hub page shell. It carries no project data; everything is
/// fetched from the authenticated JSON API by `assets/js/main.js`.
pub fn render_hub_shell() -> String {
    DASHBOARD_TEMPLATE.replace("__VERSION__", &escape_html(crate::version::VERSION))
}

async fn page_handler(method: axum::http::Method) -> Response {
    if method != axum::http::Method::GET && method != axum::http::Method::HEAD {
        return Problem::from_status(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed").into_response();
    }
    Html(render_hub_shell()).into_response()
}

async fn api_not_found() -> Response {
    Problem::not_found("The requested Hub API resource does not exist.").into_response()
}

async fn asset_handler(AxumPath(path): AxumPath<String>) -> Response {
    match ASSETS.iter().find(|(p, _, _)| *p == path) {
        Some((_, body, ct)) => {
            let mut resp = (StatusCode::OK, *body).into_response();
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(ct));
            h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
            resp
        }
        None => Problem::not_found("Unknown asset").into_response(),
    }
}

async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "status": "ok", "hub": "knobyte" })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapBody {
    token: String,
}

/// `POST /api/session/bootstrap` `{"token": "..."}` → session cookie.
async fn bootstrap_handler(State(state): State<HubState>, body: Bytes) -> Response {
    if body.len() > 4096 {
        return Problem::bad_request("Request body too large").into_response();
    }
    let b: BootstrapBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return Problem::bad_request(format!("Invalid bootstrap request: {}", e)).into_response(),
    };
    match state.security.exchange(b.token.trim()) {
        Ok(session) => {
            let mut resp = (StatusCode::CREATED, Json(json!({ "expiresAt": session.expires_at.to_rfc3339() }))).into_response();
            if let Ok(v) = HeaderValue::from_str(&state.security.session_cookie(&session)) {
                resp.headers_mut().insert(header::SET_COOKIE, v);
            }
            resp
        }
        Err(p) => p.into_response(),
    }
}

/// `GET /api/session`: the session's CSRF token and expiry.
async fn session_handler(auth: Option<Extension<Auth>>) -> Response {
    match auth.map(|Extension(a)| a) {
        Some(Auth::Session(s)) => Json(json!({ "csrfToken": s.csrf_token, "expiresAt": s.expires_at.to_rfc3339(), "kind": "session" })).into_response(),
        Some(Auth::Bearer) => Json(json!({ "csrfToken": null, "expiresAt": null, "kind": "bearer" })).into_response(),
        None => Problem::unauthorized("A valid Hub session is required.").into_response(),
    }
}

async fn logout_handler(State(state): State<HubState>, auth: Option<Extension<Auth>>) -> Response {
    if let Some(Extension(Auth::Session(s))) = auth {
        state.security.revoke(&s.id);
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    if let Ok(v) = HeaderValue::from_str(&format!("{}=; HttpOnly; SameSite=Strict; Path=/api; Max-Age=0", state.security.cookie_name)) {
        resp.headers_mut().insert(header::SET_COOKIE, v);
    }
    resp
}
