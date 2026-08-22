//! Request guard for the Project Hub.
//!
//! * Host / Origin validation (DNS rebinding and cross-site protection); proxy
//!   (`Forwarded`, `X-Forwarded-*`) headers are refused on loopback binds.
//! * Authentication: `knobyte hub` prints (and opens) a one-time bootstrap link
//!   `http://host/#token=...`. The page exchanges the token at
//!   `POST /api/session/bootstrap` for an HttpOnly, SameSite=Strict session
//!   cookie scoped to `/api`. Every `/api` route (except the exchange) requires
//!   that session, even on loopback. The bootstrap token expires after five
//!   minutes and works once. A configured access token (`--token` /
//!   `KNOBYTE_HUB_TOKEN`) can be exchanged repeatedly and is accepted as a
//!   Bearer token for scripted API clients.
//! * CSRF: every mutating request authenticated by a session cookie must carry
//!   the session's CSRF token (`X-Knobyte-CSRF`) and a same-origin `Origin`.
//! * A strict Content-Security-Policy and other headers on every response.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, HeaderValue, Method},
    middleware::Next,
    response::{IntoResponse, Response},
};

use super::problem::Problem;
use crate::mcp::security::{bearer_from_header, constant_time_eq, generate_token, host_allowed, is_loopback_host, origin_allowed};

/// Environment variable holding the Hub access token (`knobyte hub --token` sets it).
pub const HUB_TOKEN_ENV_VAR: &str = "KNOBYTE_HUB_TOKEN";
/// Header that must carry the CSRF token on every mutating request.
pub const CSRF_HEADER: &str = "x-knobyte-csrf";
/// Prefix of the per-process session cookie name.
pub const SESSION_COOKIE_PREFIX: &str = "knobyte_hub_session_";
/// The bootstrap link is valid this long after the Hub starts.
pub const BOOTSTRAP_TTL: Duration = Duration::from_secs(5 * 60);
/// Sessions last this long.
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// Upper bound on concurrently retained sessions.
const MAX_SESSIONS: usize = 64;
/// Upper bound on concurrently open event streams (SSE) across all sessions.
pub const MAX_EVENT_STREAMS: usize = 16;
/// How often an open event stream re-checks that its session is still valid.
const STREAM_SESSION_CHECK: Duration = Duration::from_secs(5);

/// Content-Security-Policy applied to every Hub response. All script and
/// style are served from the Hub itself; no inline script, no external hosts.
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
img-src 'self' data:; connect-src 'self'; font-src 'self'; manifest-src 'self'; base-uri 'none'; \
form-action 'self'; frame-ancestors 'none'";

const FORWARDED_HEADERS: [&str; 6] =
    ["forwarded", "x-forwarded-for", "x-forwarded-host", "x-forwarded-port", "x-forwarded-proto", "x-real-ip"];

#[derive(Debug, Clone)]
pub struct HubSession {
    pub id: String,
    pub csrf_token: String,
    pub expires_at: chrono::DateTime<chrono::Utc>,
    expires: Instant,
}

#[derive(Debug)]
struct Bootstrap {
    token: String,
    expires: Instant,
    consumed: bool,
}

/// How a request was authenticated.
#[derive(Debug, Clone)]
pub enum Auth {
    Session(HubSession),
    Bearer,
}

#[derive(Debug)]
pub struct HubSecurity {
    /// Reusable access token (required for non-loopback binds), if any.
    pub access_token: Option<String>,
    /// Whether the Hub is bound to a loopback interface (strict Host checks).
    pub loopback_bind: bool,
    /// Port the Hub listens on. For loopback binds the Host header must name
    /// exactly this port; `None` (unknown / ephemeral) skips the check.
    pub bound_port: Option<u16>,
    /// Session cookie name (random per process, so stale cookies never match).
    pub cookie_name: String,
    bootstrap: Mutex<Bootstrap>,
    sessions: Mutex<HashMap<String, HubSession>>,
    open_streams: Arc<std::sync::atomic::AtomicUsize>,
    revoked: Arc<tokio::sync::Notify>,
}

/// Holds one event-stream slot; released on drop.
#[derive(Debug)]
pub struct StreamSlot {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl HubSecurity {
    pub fn new(access_token: Option<String>, loopback_bind: bool, bootstrap_token: Option<String>) -> Self {
        let suffix: String = generate_token().chars().take(22).collect();
        HubSecurity {
            access_token: access_token.filter(|t| !t.trim().is_empty()).map(|t| t.trim().to_string()),
            loopback_bind,
            bound_port: None,
            cookie_name: format!("{}{}", SESSION_COOKIE_PREFIX, suffix),
            bootstrap: Mutex::new(Bootstrap {
                token: bootstrap_token.filter(|t| !t.is_empty()).unwrap_or_else(generate_token),
                expires: Instant::now() + BOOTSTRAP_TTL,
                consumed: false,
            }),
            sessions: Mutex::new(HashMap::new()),
            open_streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            revoked: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Pin the listening port the Host header must match (loopback binds).
    pub fn with_bound_port(mut self, port: Option<u16>) -> Self {
        self.bound_port = port.filter(|p| *p != 0);
        self
    }

    /// Whether `host` (a Host header) names the bound port exactly.
    fn host_port_matches(&self, host: Option<&str>) -> bool {
        let (Some(expected), true) = (self.bound_port, self.loopback_bind) else { return true };
        let Some(host) = host else { return true };
        let host = host.trim();
        let rest = &host[crate::mcp::security::authority_host(host).len()..];
        rest.strip_prefix(':').and_then(|p| p.parse::<u16>().ok()) == Some(expected)
    }

    /// Reserve an event-stream slot, or `None` when [`MAX_EVENT_STREAMS`] are already open.
    pub fn acquire_stream(&self) -> Option<StreamSlot> {
        let prev = self.open_streams.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let slot = StreamSlot {
            counter: self.open_streams.clone(),
        };
        if prev >= MAX_EVENT_STREAMS {
            drop(slot);
            return None;
        }
        Some(slot)
    }

    /// Number of currently open event streams.
    pub fn open_streams(&self) -> usize {
        self.open_streams.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Whether a session id is still valid (exists and not expired).
    pub fn session_alive(&self, id: &str) -> bool {
        self.authenticate_session(Some(id)).is_some()
    }

    /// The current one-time bootstrap token (printed by `knobyte hub`).
    pub fn bootstrap_token(&self) -> String {
        self.bootstrap.lock().map(|b| b.token.clone()).unwrap_or_default()
    }

    /// Issue a fresh one-time bootstrap token (valid for [`BOOTSTRAP_TTL`]).
    pub fn rotate_bootstrap(&self) -> String {
        let token = generate_token();
        if let Ok(mut b) = self.bootstrap.lock() {
            b.token = token.clone();
            b.expires = Instant::now() + BOOTSTRAP_TTL;
            b.consumed = false;
        }
        token
    }

    /// Exchange a bootstrap (or configured access) token for a new session.
    pub fn exchange(&self, presented: &str) -> Result<HubSession, Problem> {
        let mut ok = false;
        if let Some(access) = &self.access_token {
            ok = constant_time_eq(presented.as_bytes(), access.as_bytes());
        }
        if !ok {
            let mut b = self.bootstrap.lock().map_err(|_| Problem::internal("Hub state poisoned"))?;
            if !b.consumed && Instant::now() < b.expires && constant_time_eq(presented.as_bytes(), b.token.as_bytes()) {
                b.consumed = true;
                ok = true;
            }
        }
        if !ok {
            return Err(Problem::unauthorized(
                "The bootstrap token is invalid, expired, or already used. Run `knobyte hub` again for a fresh link.",
            ));
        }
        let session = HubSession {
            id: generate_token(),
            csrf_token: generate_token(),
            expires_at: chrono::Utc::now() + chrono::Duration::from_std(SESSION_TTL).unwrap_or_default(),
            expires: Instant::now() + SESSION_TTL,
        };
        let mut sessions = self.sessions.lock().map_err(|_| Problem::internal("Hub state poisoned"))?;
        let now = Instant::now();
        sessions.retain(|_, s| s.expires > now);
        if sessions.len() >= MAX_SESSIONS {
            if let Some(oldest) = sessions.values().min_by_key(|s| s.expires).map(|s| s.id.clone()) {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(session.id.clone(), session.clone());
        Ok(session)
    }

    fn authenticate_session(&self, id: Option<&str>) -> Option<HubSession> {
        let id = id?;
        let mut sessions = self.sessions.lock().ok()?;
        let s = sessions.get(id)?.clone();
        if Instant::now() >= s.expires {
            sessions.remove(id);
            return None;
        }
        Some(s)
    }

    /// End a session (`POST /api/session/logout`).
    pub fn revoke(&self, id: &str) {
        if let Ok(mut s) = self.sessions.lock() {
            s.remove(id);
        }
        // Wake open event streams so a logged-out session's streams end at once.
        self.revoked.notify_waiters();
    }

    pub fn session_cookie(&self, session: &HubSession) -> String {
        format!(
            "{}={}; HttpOnly; SameSite=Strict; Path=/api; Max-Age={}",
            self.cookie_name,
            session.id,
            SESSION_TTL.as_secs()
        )
    }
}

/// Decide which access token the Hub must enforce for a bind `host`.
/// Returns `(token, generated)`: an explicit token always wins; a non-loopback
/// bind without one gets a freshly generated token.
pub fn resolve_hub_token(host: &str, configured: Option<String>) -> (Option<String>, bool) {
    match configured.filter(|t| !t.trim().is_empty()) {
        Some(t) => (Some(t.trim().to_string()), false),
        None if !is_loopback_host(host) => (Some(generate_token()), true),
        None => (None, false),
    }
}

fn header_str(headers: &HeaderMap, name: impl header::AsHeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

pub(crate) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    for value in headers.get_all(header::COOKIE) {
        if let Ok(v) = value.to_str() {
            for part in v.split(';') {
                if let Some((k, val)) = part.trim().split_once('=') {
                    if k.trim() == name {
                        return Some(val.trim().to_string());
                    }
                }
            }
        }
    }
    None
}

/// Attach security headers to every Hub response.
pub fn apply_security_headers(resp: &mut Response) {
    let h = resp.headers_mut();
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CONTENT_SECURITY_POLICY));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert("cross-origin-opener-policy", HeaderValue::from_static("same-origin"));
    h.insert("cross-origin-resource-policy", HeaderValue::from_static("same-origin"));
    h.insert("permissions-policy", HeaderValue::from_static("camera=(), microphone=(), geolocation=()"));
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
}

/// Middleware enforcing Host/Origin checks, session authentication and CSRF.
pub async fn hub_guard(State(security): State<Arc<HubSecurity>>, req: Request, next: Next) -> Response {
    let mut resp = guard_inner(&security, req, next).await;
    apply_security_headers(&mut resp);
    resp
}

/// Same-origin check for state-changing requests: an `Origin` must be present,
/// allowed, and name the Host the request was sent to.
fn same_origin(origin: Option<&str>, host: Option<&str>, loopback_bind: bool) -> bool {
    let (Some(origin), Some(host)) = (origin, host) else { return false };
    if !origin_allowed(Some(origin), Some(host), loopback_bind) {
        return false;
    }
    let authority = origin.trim().split_once("://").map(|(_, a)| a.trim_end_matches('/')).unwrap_or("");
    authority.eq_ignore_ascii_case(host.trim())
}

async fn guard_inner(security: &HubSecurity, mut req: Request, next: Next) -> Response {
    let headers = req.headers();
    let host = header_str(headers, header::HOST);
    let origin = header_str(headers, header::ORIGIN);

    if security.loopback_bind && FORWARDED_HEADERS.iter().any(|h| headers.contains_key(*h)) {
        return Problem::bad_request("The local Hub does not accept forwarded or proxy request headers.").into_response();
    }
    if !host_allowed(host, security.loopback_bind) || !security.host_port_matches(host) {
        return Problem::forbidden("ORIGIN_REJECTED", "Invalid Host header", "Host header not allowed").into_response();
    }
    if !origin_allowed(origin, host, security.loopback_bind) {
        return Problem::forbidden("ORIGIN_REJECTED", "Origin rejected", "Origin not allowed").into_response();
    }

    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let safe_method = matches!(method, Method::GET | Method::HEAD);
    if !safe_method && method != Method::POST {
        return Problem::from_status(axum::http::StatusCode::METHOD_NOT_ALLOWED, "Method not allowed").into_response();
    }

    if !path.starts_with("/api/") && path != "/api" {
        // Static shell and assets carry no project data.
        if !safe_method {
            return Problem::from_status(axum::http::StatusCode::METHOD_NOT_ALLOWED, "Method not allowed").into_response();
        }
        return next.run(req).await;
    }

    if path == "/api/session/bootstrap" {
        if method != Method::POST {
            return Problem::from_status(axum::http::StatusCode::METHOD_NOT_ALLOWED, "Use POST").into_response();
        }
        if !same_origin(origin, host, security.loopback_bind) {
            return Problem::forbidden(
                "ORIGIN_REJECTED",
                "Origin rejected",
                "The session exchange must originate from this local Hub instance.",
            )
            .into_response();
        }
        return next.run(req).await;
    }

    // Bearer access token (scripted clients): no ambient credential, so no CSRF.
    if let Some(expected) = &security.access_token {
        if let Some(b) = header_str(headers, header::AUTHORIZATION).and_then(bearer_from_header) {
            if constant_time_eq(b.as_bytes(), expected.as_bytes()) {
                req.extensions_mut().insert(Auth::Bearer);
                return next.run(req).await;
            }
            return Problem::unauthorized("Invalid Hub access token").into_response();
        }
    }

    let cookie = cookie_value(headers, &security.cookie_name);
    let Some(session) = security.authenticate_session(cookie.as_deref()) else {
        let mut resp = Problem::unauthorized(
            "A valid Hub session is required. Open the link printed by `knobyte hub` (it ends in #token=...).",
        )
        .into_response();
        resp.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Session"));
        return resp;
    };

    if !safe_method {
        if !same_origin(origin, host, security.loopback_bind) {
            return Problem::forbidden(
                "ORIGIN_REJECTED",
                "Origin rejected",
                "The mutation must originate from this local Hub instance.",
            )
            .into_response();
        }
        let presented = header_str(headers, CSRF_HEADER);
        if !presented.is_some_and(|p| constant_time_eq(p.as_bytes(), session.csrf_token.as_bytes())) {
            return Problem::forbidden(
                "ORIGIN_REJECTED",
                "CSRF token rejected",
                "Missing or invalid CSRF token; reload the Hub page.",
            )
            .into_response();
        }
    }
    req.extensions_mut().insert(Auth::Session(session));
    next.run(req).await
}

/// The 429 answer for a request over the event-stream cap.
pub fn too_many_streams() -> Response {
    use axum::response::IntoResponse;
    Problem::new(
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        "TOO_MANY_STREAMS",
        "Too many event streams",
        format!(
            "At most {} live event streams may be open at once. Close another Hub tab and retry.",
            MAX_EVENT_STREAMS
        ),
    )
    .into_response()
}

/// Wrap an SSE event stream so it holds `slot` while open and ends (after a final
/// `session-ended` event) as soon as the session behind `auth` expires or logs out.
/// Bearer-authenticated streams have no session to end.
pub fn session_bound_stream<S>(
    inner: S,
    security: Arc<HubSecurity>,
    auth: Option<Auth>,
    slot: StreamSlot,
) -> impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>> + Send
where
    S: futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>> + Send + 'static,
{
    let session_id = match auth {
        Some(Auth::Session(s)) => Some(s.id),
        _ => None,
    };
    async_stream::stream! {
        use futures_util::StreamExt;
        let _slot = slot;
        let mut inner = Box::pin(inner);
        let revoked = security.revoked.clone();
        let mut tick = tokio::time::interval(STREAM_SESSION_CHECK);
        tick.tick().await;
        loop {
            let check = tokio::select! {
                item = inner.next() => match item {
                    Some(item) => { yield item; false }
                    None => break,
                },
                _ = tick.tick() => true,
                _ = revoked.notified() => true,
            };
            if check {
                if let Some(id) = &session_id {
                    if !security.session_alive(id) {
                        yield Ok(axum::response::sse::Event::default().event("session-ended").data("{}"));
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_tokens() {
        assert_eq!(resolve_hub_token("127.0.0.1", None), (None, false));
        assert_eq!(resolve_hub_token("127.0.0.1", Some("abc".into())), (Some("abc".into()), false));
        let (t, generated) = resolve_hub_token("0.0.0.0", None);
        assert!(t.is_some() && generated);
    }

    #[test]
    fn bootstrap_is_one_time() {
        let sec = HubSecurity::new(None, true, Some("boot".into()));
        assert!(sec.exchange("nope").is_err());
        let s = sec.exchange("boot").unwrap();
        assert!(sec.authenticate_session(Some(&s.id)).is_some());
        assert!(sec.exchange("boot").is_err());
        let fresh = sec.rotate_bootstrap();
        assert!(sec.exchange(&fresh).is_ok());
    }

    #[test]
    fn access_token_is_reusable() {
        let sec = HubSecurity::new(Some("acc".into()), false, None);
        assert!(sec.exchange("acc").is_ok());
        assert!(sec.exchange("acc").is_ok());
    }

    #[test]
    fn same_origin_requires_matching_host() {
        assert!(same_origin(Some("http://127.0.0.1:4000"), Some("127.0.0.1:4000"), true));
        assert!(!same_origin(Some("http://localhost:4000"), Some("127.0.0.1:4000"), true));
        assert!(!same_origin(None, Some("127.0.0.1:4000"), true));
    }

    #[test]
    fn host_must_name_the_bound_port() {
        let sec = HubSecurity::new(None, true, None).with_bound_port(Some(4400));
        assert!(sec.host_port_matches(Some("127.0.0.1:4400")));
        assert!(sec.host_port_matches(Some("localhost:4400")));
        assert!(sec.host_port_matches(Some("[::1]:4400")));
        assert!(!sec.host_port_matches(Some("127.0.0.1:4401")));
        assert!(!sec.host_port_matches(Some("127.0.0.1")));
        assert!(!sec.host_port_matches(Some("127.0.0.1:4400x")));
        let any = HubSecurity::new(None, true, None).with_bound_port(Some(0));
        assert!(any.host_port_matches(Some("127.0.0.1:1")));
    }

    #[test]
    fn parses_cookies() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("a=1; knobyte_hub_session_x=xyz"));
        assert_eq!(cookie_value(&h, "knobyte_hub_session_x").as_deref(), Some("xyz"));
    }
}
