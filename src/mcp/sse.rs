//! HTTP transports for the MCP server:
//!
//! * Legacy HTTP+SSE (`GET /sse` + `POST /messages?sessionId=...`), where
//!   responses are delivered only over the SSE stream and POSTs get `202`.
//! * Streamable HTTP (`POST /mcp`, `DELETE /mcp`) with `Mcp-Session-Id`.
//!
//! [`HttpTransport`] selects which of the two is served (`knobyte mcp --http` / `--sse`; both by
//! default). The endpoints of a disabled transport answer `404`.
//!
//! Every route is protected by Host/Origin validation (DNS-rebinding and
//! browser-driven attack protection) and, when configured or when bound to a
//! non-loopback interface, bearer-token authentication.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    body::Bytes,
    extract::{Query, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::sse::{Event, KeepAlive, Sse},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use colored::Colorize;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use uuid::Uuid;

use crate::config::KnobyteConfig;
use crate::mcp::handler::{
    handle_value, is_request_free, server_config, PARSE_ERROR, SUPPORTED_PROTOCOL_VERSIONS,
};
use crate::mcp::profiles::McpProfile;
use crate::mcp::protocol::JsonRpcResponse;
use crate::mcp::security::{
    bearer_from_header, constant_time_eq, generate_token, host_allowed, is_loopback_host, origin_allowed,
    TOKEN_ENV_VAR,
};

// Kept for API compatibility: callers used to import these from `sse`.
pub use crate::mcp::handler::{process_jsonrpc_request, process_jsonrpc_request_with_config};

/// Header carrying the streamable-HTTP session id.
pub const SESSION_HEADER: &str = "mcp-session-id";
/// Header carrying the negotiated protocol version (streamable HTTP).
pub const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
/// Streamable-HTTP sessions idle for longer than this are evicted.
const HTTP_SESSION_IDLE_TTL: Duration = Duration::from_secs(24 * 3600);
/// Capacity of each SSE session's outbound queue.
const SSE_CHANNEL_CAPACITY: usize = 100;

type SseSessionMap = Arc<Mutex<HashMap<String, mpsc::Sender<Event>>>>;
type HttpSessionMap = Arc<Mutex<HashMap<String, Instant>>>;

/// Security settings for the HTTP server.
#[derive(Debug, Clone, Default)]
pub struct ServerSecurity {
    /// Bearer token required on every request, if any.
    pub token: Option<String>,
    /// Whether the server is bound to a loopback interface. Enables strict
    /// Host-header validation.
    pub loopback_bind: bool,
}

/// Which HTTP transports the server offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HttpTransport {
    /// Streamable HTTP (`/mcp`) and legacy HTTP+SSE (`/sse` + `/messages`): the default.
    #[default]
    Both,
    /// Streamable HTTP only (`knobyte mcp --http`).
    StreamableHttp,
    /// Legacy HTTP+SSE only (`knobyte mcp --sse`).
    Sse,
}

impl HttpTransport {
    /// Whether `POST`/`DELETE /mcp` is served.
    pub fn streamable(self) -> bool {
        matches!(self, HttpTransport::Both | HttpTransport::StreamableHttp)
    }

    /// Whether `GET /sse` and `POST /messages` are served.
    pub fn sse(self) -> bool {
        matches!(self, HttpTransport::Both | HttpTransport::Sse)
    }

    /// Transport names as listed by `GET /` (`stdio` is a separate process mode).
    pub fn names(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.sse() {
            out.push("sse");
        }
        if self.streamable() {
            out.push("streamable-http");
        }
        out
    }
}

/// Options for [`start_sse_server_with_options`].
#[derive(Debug, Clone, Default)]
pub struct SseServerOptions {
    /// Explicit bearer token. When `None`, `KNOBYTE_MCP_TOKEN` is consulted by
    /// [`start_sse_server`]; if still unset and the bind host is not loopback,
    /// a random token is generated and printed to stderr.
    pub token: Option<String>,
    /// HTTP transports to serve (both by default).
    pub transport: HttpTransport,
    /// Tool profile; `None` uses the project's configured profile (env, then config.json).
    pub profile: Option<McpProfile>,
}

#[derive(Clone)]
pub struct AppState {
    pub sessions: SseSessionMap,
    pub http_sessions: HttpSessionMap,
    pub config: Arc<KnobyteConfig>,
    pub security: Arc<ServerSecurity>,
    pub transport: HttpTransport,
    pub profile: McpProfile,
}

#[derive(Debug, Deserialize)]
pub struct MessageQuery {
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TokenQuery {
    pub token: Option<String>,
}

/// Decide which token (if any) the server must enforce.
/// Returns `(token, generated)`.
pub fn resolve_auth_token(host: &str, configured: Option<String>) -> (Option<String>, bool) {
    match configured.filter(|t| !t.trim().is_empty()) {
        Some(t) => (Some(t.trim().to_string()), false),
        None if !is_loopback_host(host) => (Some(generate_token()), true),
        None => (None, false),
    }
}

/// Build the axum router for the given project and security settings, serving both HTTP
/// transports.
pub fn build_router(config: KnobyteConfig, security: ServerSecurity) -> Router {
    build_router_with(config, security, HttpTransport::Both)
}

/// Build the axum router serving only the selected HTTP transport(s). The endpoints of a
/// disabled transport answer `404` with a message naming the flag that disabled them.
pub fn build_router_with(config: KnobyteConfig, security: ServerSecurity, transport: HttpTransport) -> Router {
    let profile = McpProfile::configured(&config.scaffold_root);
    build_router_with_profile(config, security, transport, profile)
}

/// Build the axum router serving `transport` and the tools of `profile`.
pub fn build_router_with_profile(
    config: KnobyteConfig,
    security: ServerSecurity,
    transport: HttpTransport,
    profile: McpProfile,
) -> Router {
    let state = AppState {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        http_sessions: Arc::new(Mutex::new(HashMap::new())),
        config: Arc::new(config),
        security: Arc::new(security),
        transport,
        profile,
    };

    let mut router = Router::new();
    router = if transport.sse() {
        router
            .route("/sse", get(sse_handler))
            .route("/messages", post(messages_handler))
    } else {
        router
            .route("/sse", axum::routing::any(sse_disabled_handler))
            .route("/messages", axum::routing::any(sse_disabled_handler))
    };
    router = if transport.streamable() {
        router.route(
            "/mcp",
            post(mcp_post_handler).delete(mcp_delete_handler).get(mcp_get_handler),
        )
    } else {
        router.route("/mcp", axum::routing::any(streamable_disabled_handler))
    };
    router
        .route("/health", get(health_handler))
        .route("/dashboard", get(dashboard_handler))
        .route("/", get(root_handler))
        .layer(middleware::from_fn_with_state(state.clone(), guard_middleware))
        .with_state(state)
}

/// Start the HTTP MCP server with both transports. The bearer token is read from the
/// `KNOBYTE_MCP_TOKEN` environment variable; when unset and `host` is not a
/// loopback address, a random token is generated and printed to stderr.
pub async fn start_sse_server(host: &str, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    start_http_server(host, port, HttpTransport::Both).await
}

/// Start the HTTP MCP server serving `transport` (token as in [`start_sse_server`]).
pub async fn start_http_server(
    host: &str,
    port: u16,
    transport: HttpTransport,
) -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var(TOKEN_ENV_VAR).ok();
    start_sse_server_with_options(host, port, SseServerOptions { token, transport, profile: None }).await
}

/// Start the HTTP MCP server with explicit options.
pub async fn start_sse_server_with_options(
    host: &str,
    port: u16,
    options: SseServerOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let config = server_config();
    let loopback_bind = is_loopback_host(host);
    let (token, generated) = resolve_auth_token(host, options.token);

    let transport = options.transport;
    let profile = options.profile.unwrap_or_else(|| McpProfile::configured(&config.scaffold_root));
    let app = build_router_with_profile(config, ServerSecurity { token: token.clone(), loopback_bind }, transport, profile);

    let addr = if host.contains(':') && !host.starts_with('[') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    };
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    println!("{} Knobyte Remote MCP Server listening on http://{}", "[mcp]".cyan().bold(), addr);
    println!("   Transports: {}", transport_summary(transport));
    if transport.streamable() {
        println!("   - Streamable MCP:   http://{}/mcp", addr);
    }
    if transport.sse() {
        println!("   - SSE Endpoint:     http://{}/sse", addr);
        println!("   - Messages:         http://{}/messages", addr);
    }
    println!("   - Dashboard:        http://{}/dashboard", addr);
    println!("   - Health check:     http://{}/health", addr);
    match (&token, generated) {
        (Some(t), true) => {
            eprintln!(
                "{} Bound to non-loopback address {}; bearer authentication is required.",
                "[mcp]".yellow().bold(),
                host
            );
            eprintln!("   Generated token (set {} to choose your own):", TOKEN_ENV_VAR);
            eprintln!("   Authorization: Bearer {}", t);
        }
        (Some(_), false) => {
            eprintln!(
                "{} Bearer authentication enabled (token from {}).",
                "[mcp]".cyan().bold(),
                TOKEN_ENV_VAR
            );
        }
        (None, _) => {}
    }

    axum::serve(listener, app).await?;
    Ok(())
}

/// One-line description of the live transports, printed at startup.
pub fn transport_summary(transport: HttpTransport) -> &'static str {
    match transport {
        HttpTransport::Both => "streamable HTTP (/mcp) and legacy SSE (/sse + /messages)",
        HttpTransport::StreamableHttp => "streamable HTTP only (/mcp); legacy SSE is off (--http)",
        HttpTransport::Sse => "legacy SSE only (/sse + /messages); streamable HTTP is off (--sse)",
    }
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

fn percent_decode(s: &str) -> String {
    fn hex(b: u8) -> Option<u8> {
        (b as char).to_digit(16).map(|d| d as u8)
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_token(query: Option<&str>) -> Option<String> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token=").map(percent_decode))
}

fn plain_error(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

/// Host/Origin validation and bearer-token authentication for every route.
async fn guard_middleware(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let headers = req.headers();
    let host = header_str(headers, header::HOST);
    let origin = header_str(headers, header::ORIGIN);
    let security = &state.security;

    if !host_allowed(host, security.loopback_bind) {
        return plain_error(StatusCode::FORBIDDEN, "Host header not allowed");
    }
    if !origin_allowed(origin, host, security.loopback_bind) {
        return plain_error(StatusCode::FORBIDDEN, "Origin not allowed");
    }

    if let Some(expected) = &security.token {
        let header_token = header_str(headers, header::AUTHORIZATION).and_then(bearer_from_header);
        // Query-string tokens are accepted only where clients cannot set headers:
        // EventSource (GET /sse), the endpoint URL it hands out (POST /messages),
        // and browser navigation to the dashboard.
        let path = req.uri().path();
        let query_allowed = matches!(
            (req.method(), path),
            (&Method::GET, "/sse") | (&Method::POST, "/messages") | (&Method::GET, "/dashboard") | (&Method::GET, "/")
        );
        let qt = if query_allowed { query_token(req.uri().query()) } else { None };
        let presented = header_token.map(|s| s.to_string()).or(qt);
        let ok = presented
            .as_deref()
            .is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes()));
        if !ok {
            let mut resp = plain_error(StatusCode::UNAUTHORIZED, "Missing or invalid bearer token");
            resp.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            return resp;
        }
    }

    next.run(req).await
}

async fn dashboard_handler(State(state): State<AppState>) -> impl IntoResponse {
    let config = state.config.clone();
    match tokio::task::spawn_blocking(move || crate::hub::render_dashboard_html(&config)).await {
        Ok(html) => Html(html).into_response(),
        Err(_) => plain_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to render dashboard"),
    }
}

async fn root_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let accept = header_str(&headers, header::ACCEPT).unwrap_or("");
    if accept.contains("text/html") {
        return dashboard_handler(State(state)).await.into_response();
    }
    Json(json!({
        "product": crate::version::APP_NAME,
        "version": crate::version::VERSION,
        "protocol": "model-context-protocol",
        "protocolVersions": SUPPORTED_PROTOCOL_VERSIONS,
        "transports": state.transport.names(),
        "profile": state.profile.name(),
        "tools": state.profile.tool_names().len(),
        "auth": if state.security.token.is_some() { "bearer" } else { "none" },
        "endpoints": endpoints(state.transport),
    }))
    .into_response()
}

fn endpoints(transport: HttpTransport) -> Value {
    let mut e = serde_json::Map::new();
    e.insert("dashboard".into(), json!("/dashboard"));
    if transport.sse() {
        e.insert("sse".into(), json!("/sse"));
        e.insert("messages".into(), json!("/messages"));
    }
    if transport.streamable() {
        e.insert("mcp".into(), json!("/mcp"));
    }
    e.insert("health".into(), json!("/health"));
    Value::Object(e)
}

async fn sse_disabled_handler() -> Response {
    plain_error(
        StatusCode::NOT_FOUND,
        "The legacy SSE transport is disabled (server started with --http); use POST /mcp",
    )
}

async fn streamable_disabled_handler() -> Response {
    plain_error(
        StatusCode::NOT_FOUND,
        "The streamable HTTP transport is disabled (server started with --sse); use GET /sse",
    )
}

async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({
            "status": "ok",
            "service": format!("{}-mcp", crate::version::APP_NAME),
            "version": crate::version::VERSION,
            "profile": state.profile.name(),
            "tools": state.profile.tool_names(),
        })),
    )
}

/// Removes an SSE session from the map when its stream is dropped
/// (i.e. when the client disconnects).
struct SessionGuard {
    sessions: SseSessionMap,
    id: String,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if let Ok(mut map) = self.sessions.lock() {
            map.remove(&self.id);
        }
    }
}

async fn sse_handler(State(state): State<AppState>, Query(q): Query<TokenQuery>) -> impl IntoResponse {
    let session_id = Uuid::new_v4().simple().to_string();
    let (tx, rx) = mpsc::channel::<Event>(SSE_CHANNEL_CAPACITY);

    // If the client authenticated with ?token=, hand it back in the endpoint
    // URL so that it can POST messages without setting headers.
    let token_suffix = match (&state.security.token, q.token) {
        (Some(_), Some(t)) => format!("&token={}", url_encode(&t)),
        _ => String::new(),
    };
    let endpoint_uri = format!("/messages?sessionId={}{}", session_id, token_suffix);
    let _ = tx.try_send(Event::default().event("endpoint").data(endpoint_uri));

    if let Ok(mut sessions) = state.sessions.lock() {
        sessions.insert(session_id.clone(), tx);
    }

    let guard = SessionGuard {
        sessions: state.sessions.clone(),
        id: session_id,
    };
    let stream = ReceiverStream::new(rx).map(move |event| {
        let _keep_alive = &guard;
        Ok::<Event, Infallible>(event)
    });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn parse_error_response(e: impl std::fmt::Display) -> Response {
    let body = JsonRpcResponse::error(Some(Value::Null), PARSE_ERROR, &format!("Parse error: {}", e));
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

async fn messages_handler(
    State(state): State<AppState>,
    Query(query): Query<MessageQuery>,
    body: Bytes,
) -> Response {
    let sid = match query.session_id {
        Some(s) => s,
        None => return plain_error(StatusCode::BAD_REQUEST, "Missing sessionId query parameter"),
    };
    let tx = state.sessions.lock().ok().and_then(|m| m.get(&sid).cloned());
    let tx = match tx {
        Some(tx) => tx,
        None => return plain_error(StatusCode::NOT_FOUND, "Unknown or expired session"),
    };

    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return parse_error_response(e),
    };

    let config = state.config.clone();
    let profile = state.profile;
    tokio::spawn(async move {
        let result = tokio::task::spawn_blocking(move || handle_value(value, &config, profile)).await;
        if let Ok(Some(resp)) = result {
            let _ = tx.send(Event::default().event("message").data(resp.to_string())).await;
        }
    });

    StatusCode::ACCEPTED.into_response()
}

fn session_header(headers: &HeaderMap) -> Option<&str> {
    headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok())
}

fn is_initialize(value: &Value) -> bool {
    value.get("method").and_then(|m| m.as_str()) == Some("initialize")
}

async fn mcp_post_handler(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if let Some(v) = headers.get(PROTOCOL_VERSION_HEADER).and_then(|v| v.to_str().ok()) {
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&v) {
            return plain_error(
                StatusCode::BAD_REQUEST,
                &format!("Unsupported MCP-Protocol-Version '{}'", v),
            );
        }
    }

    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return parse_error_response(e),
    };

    let initializing = is_initialize(&value);
    let mut new_session: Option<String> = None;

    if initializing {
        let sid = Uuid::new_v4().simple().to_string();
        if let Ok(mut map) = state.http_sessions.lock() {
            let now = Instant::now();
            map.retain(|_, seen| now.duration_since(*seen) < HTTP_SESSION_IDLE_TTL);
            map.insert(sid.clone(), now);
        }
        new_session = Some(sid);
    } else {
        let sid = match session_header(&headers) {
            Some(s) => s.to_string(),
            None => return plain_error(StatusCode::BAD_REQUEST, "Missing Mcp-Session-Id header"),
        };
        let known = state
            .http_sessions
            .lock()
            .map(|mut m| match m.get_mut(&sid) {
                Some(seen) => {
                    *seen = Instant::now();
                    true
                }
                None => false,
            })
            .unwrap_or(false);
        if !known {
            return plain_error(StatusCode::NOT_FOUND, "Unknown or expired session");
        }
    }

    let only_notifications = is_request_free(&value);
    let config = state.config.clone();
    let profile = state.profile;
    let result = tokio::task::spawn_blocking(move || handle_value(value, &config, profile)).await;
    let response = match result {
        Ok(r) => r,
        Err(_) => return plain_error(StatusCode::INTERNAL_SERVER_ERROR, "Request processing failed"),
    };

    let mut resp = match response {
        Some(body) if !only_notifications => (StatusCode::OK, Json(body)).into_response(),
        _ => StatusCode::ACCEPTED.into_response(),
    };
    if let Some(sid) = new_session {
        if let Ok(v) = HeaderValue::from_str(&sid) {
            resp.headers_mut().insert(SESSION_HEADER, v);
        }
    }
    resp
}

async fn mcp_delete_handler(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let sid = match session_header(&headers) {
        Some(s) => s.to_string(),
        None => return plain_error(StatusCode::BAD_REQUEST, "Missing Mcp-Session-Id header"),
    };
    let removed = state
        .http_sessions
        .lock()
        .map(|mut m| m.remove(&sid).is_some())
        .unwrap_or(false);
    if removed {
        StatusCode::NO_CONTENT.into_response()
    } else {
        plain_error(StatusCode::NOT_FOUND, "Unknown or expired session")
    }
}

/// This server does not offer a server-initiated stream on /mcp.
async fn mcp_get_handler() -> Response {
    let mut resp = plain_error(
        StatusCode::METHOD_NOT_ALLOWED,
        "GET /mcp is not supported; use POST (streamable HTTP) or GET /sse (legacy SSE)",
    );
    resp.headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("POST, DELETE"));
    resp
}
