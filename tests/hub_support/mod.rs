//! Shared helpers for the Hub integration tests: a temp project, a router
//! with a fixed bootstrap token, and a client that signs in like the browser.
#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;

use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use serde_json::Value;
use tempfile::{tempdir, TempDir};
use tower::ServiceExt;

use knobyte::config::KnobyteConfig;
use knobyte::hub::{build_hub_app, HubApp, HubOptions, CSRF_HEADER};
use knobyte::setup::run_setup;

pub const BOOT: &str = "test-bootstrap-token-0123456789";
pub const HOST: &str = "127.0.0.1:4000";
pub const ORIGIN: &str = "http://127.0.0.1:4000";

/// Keep fleet registration out of the developer's real `~/.knobyte/projects.json`.
pub fn isolate_user_home() {
    static HOME: std::sync::OnceLock<TempDir> = std::sync::OnceLock::new();
    let dir = HOME.get_or_init(|| tempdir().unwrap());
    std::env::set_var("KNOBYTE_HOME", dir.path());
}

pub struct Hub {
    pub dir: TempDir,
    pub config: KnobyteConfig,
    pub registry: PathBuf,
}

impl Hub {
    /// A project with a scaffold (no git).
    pub fn new() -> Self {
        let h = Self::bare();
        run_setup(&h.config, "code-repo", false).unwrap();
        h
    }

    /// An empty project folder (no scaffold, no git).
    pub fn bare() -> Self {
        isolate_user_home();
        let dir = tempdir().unwrap();
        let root = dir.path().join("project");
        fs::create_dir_all(&root).unwrap();
        let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
        let registry = dir.path().join("registry").join("projects.json");
        Hub { dir, config, registry }
    }

    pub fn root(&self) -> PathBuf {
        self.config.project_root.clone()
    }

    pub fn options(&self) -> HubOptions {
        HubOptions {
            access_token: None,
            loopback_bind: true,
            registry_path: Some(self.registry.clone()),
            bootstrap_token: Some(BOOT.to_string()),
            bind_addr: Some(HOST.to_string()),
            ..Default::default()
        }
    }

    pub fn app(&self) -> HubApp {
        build_hub_app(self.config.clone(), self.options())
    }

    pub fn router(&self) -> Router {
        self.app().router
    }

    /// A signed-in client (bootstrap exchange + CSRF token).
    pub async fn client(&self) -> Client {
        Client::login(self.router()).await
    }
}

pub fn git(root: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git").args(args).current_dir(root).output().unwrap();
    assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
}

pub fn git_init(root: &std::path::Path) {
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "hub-test@example.invalid"]);
    git(root, &["config", "user.name", "Hub Test"]);
    git(root, &["config", "commit.gpgsign", "false"]);
}

pub fn raw_get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).header(header::HOST, HOST).body(Body::empty()).unwrap()
}

pub async fn send(router: &Router, req: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 32 * 1024 * 1024).await.unwrap();
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

pub fn json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("invalid JSON ({}): {}", e, body))
}

pub struct Client {
    pub router: Router,
    pub cookie: String,
    pub csrf: String,
}

impl Client {
    pub async fn login(router: Router) -> Client {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/session/bootstrap")
            .header(header::HOST, HOST)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(format!(r#"{{"token":"{}"}}"#, BOOT)))
            .unwrap();
        let (status, headers, body) = send(&router, req).await;
        assert_eq!(status, StatusCode::CREATED, "{}", body);
        let set = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
        let cookie = set.split(';').next().unwrap().to_string();
        let req = Request::builder()
            .uri("/api/session")
            .header(header::HOST, HOST)
            .header(header::COOKIE, cookie.clone())
            .body(Body::empty())
            .unwrap();
        let (status, _, body) = send(&router, req).await;
        assert_eq!(status, StatusCode::OK, "{}", body);
        let csrf = json(&body)["csrfToken"].as_str().unwrap().to_string();
        Client { router, cookie, csrf }
    }

    pub fn get_req(&self, uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(header::HOST, HOST)
            .header(header::COOKIE, self.cookie.clone())
            .body(Body::empty())
            .unwrap()
    }

    pub fn post_req(&self, uri: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header(header::HOST, HOST)
            .header(header::ORIGIN, ORIGIN)
            .header(header::COOKIE, self.cookie.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .header(CSRF_HEADER, self.csrf.clone())
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    pub async fn get(&self, uri: &str) -> (StatusCode, String) {
        let (s, _, b) = send(&self.router, self.get_req(uri)).await;
        (s, b)
    }

    pub async fn get_json(&self, uri: &str) -> Value {
        let (s, b) = self.get(uri).await;
        assert_eq!(s, StatusCode::OK, "GET {} -> {}", uri, b);
        json(&b)
    }

    pub async fn post(&self, uri: &str, body: &str) -> (StatusCode, String) {
        let (s, _, b) = send(&self.router, self.post_req(uri, body)).await;
        (s, b)
    }

    pub async fn post_json(&self, uri: &str, body: &str) -> Value {
        let (s, b) = self.post(uri, body).await;
        assert!(s.is_success(), "POST {} -> {} {}", uri, s, b);
        json(&b)
    }

    /// Preview then apply a team action; returns the apply result.
    pub async fn run_action(&self, action: Value) -> Value {
        let env = self.post_json("/api/team/operations/preview", &serde_json::json!({ "action": action }).to_string()).await;
        self.post_json("/api/team/operations/apply", &serde_json::json!({ "envelope": env["envelope"] }).to_string()).await
    }

    /// Poll `uri` until `done(json)` (bounded).
    pub async fn poll(&self, uri: &str, done: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..2400 {
            let v = self.get_json(uri).await;
            if done(&v) {
                return v;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("timed out polling {}", uri);
    }
}
