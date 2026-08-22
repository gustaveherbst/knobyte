//! Integration tests for the Project Hub HTTP API and its security guard.

mod hub_support;

use std::fs;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};

use hub_support::*;
use knobyte::events::append_event;
use knobyte::hub::projects::{load_registry_at, save_registry_at, ProjectRegistryEntry};
use knobyte::hub::{build_hub_router, render_dashboard_html, HubOptions, CSRF_HEADER};
use knobyte::team::inbox::{publish_inbox_draft, save_inbox_draft, InboxDraft};
use knobyte::team::members::{create_member, select_current_member};
use knobyte::team::relay::{publish_relay_draft, save_relay_draft, RelayDraft};

#[tokio::test]
async fn post_requires_csrf_token_and_same_origin() {
    let hub = Hub::new();
    let c = hub.client().await;

    let mut req = c.post_req("/api/drift/sync", r#"{"dryRun":true}"#);
    req.headers_mut().remove(CSRF_HEADER);
    let (status, headers, body) = send(&c.router, req).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "application/problem+json");
    assert_eq!(json(&body)["code"], "ORIGIN_REJECTED");

    let mut req = c.post_req("/api/drift/sync", r#"{"dryRun":true}"#);
    req.headers_mut().insert(CSRF_HEADER, "wrong".parse().unwrap());
    assert_eq!(send(&c.router, req).await.0, StatusCode::FORBIDDEN);

    // A missing Origin on a mutation is refused even with the right token.
    let mut req = c.post_req("/api/drift/sync", r#"{"dryRun":true}"#);
    req.headers_mut().remove(header::ORIGIN);
    assert_eq!(send(&c.router, req).await.0, StatusCode::FORBIDDEN);

    let (status, body) = c.post("/api/drift/sync", r#"{"dryRun":true}"#).await;
    assert_eq!(status, StatusCode::OK, "{}", body);

    // Mutating routes are POST-only; other methods are refused.
    let (status, _) = c.get("/api/drift/sync").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    let req = Request::builder()
        .method(Method::PUT)
        .uri("/api/drift/sync")
        .header(header::HOST, HOST)
        .header(header::COOKIE, c.cookie.clone())
        .header(CSRF_HEADER, c.csrf.clone())
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(&c.router, req).await.0, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn shell_is_static_and_data_free() {
    let hub = Hub::new();
    let r = hub.router();
    let (status, headers, body) = send(&r, raw_get("/")).await;
    assert_eq!(status, StatusCode::OK);
    let csp = headers.get(header::CONTENT_SECURITY_POLICY).unwrap().to_str().unwrap();
    assert!(csp.contains("script-src 'self'"));
    assert!(!csp.contains("unsafe-inline"));
    assert!(headers.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).is_none());
    assert!(!body.contains("<script>"));
    assert!(!body.contains("style=\""));
    assert!(body.contains(r#"<script type="module" src="/assets/js/main.js"></script>"#));

    let (status, headers, js) = send(&r, raw_get("/assets/js/main.js")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap().starts_with("text/javascript"));
    assert!(!js.contains(".innerHTML"));
}

#[tokio::test]
async fn bad_origin_and_host_are_rejected() {
    let hub = Hub::new();
    let c = hub.client().await;

    let mut req = c.get_req("/api/overview");
    req.headers_mut().insert(header::ORIGIN, "https://evil.example".parse().unwrap());
    assert_eq!(send(&c.router, req).await.0, StatusCode::FORBIDDEN);

    // Cross-site POST with the right CSRF token is still refused by Origin.
    let mut req = c.post_req("/api/drift/sync", "");
    req.headers_mut().insert(header::ORIGIN, "http://localhost.evil.example".parse().unwrap());
    assert_eq!(send(&c.router, req).await.0, StatusCode::FORBIDDEN);

    // DNS rebinding: Host must be loopback on a loopback bind.
    let mut req = c.get_req("/api/overview");
    req.headers_mut().insert(header::HOST, "evil.example:4000".parse().unwrap());
    assert_eq!(send(&c.router, req).await.0, StatusCode::FORBIDDEN);

    // Same-origin reads are fine.
    let mut req = c.get_req("/api/overview");
    req.headers_mut().insert(header::ORIGIN, ORIGIN.parse().unwrap());
    assert_eq!(send(&c.router, req).await.0, StatusCode::OK);
}

#[tokio::test]
async fn access_token_works_as_bearer_and_reusable_bootstrap() {
    let hub = Hub::new();
    let r = build_hub_router(
        hub.config.clone(),
        HubOptions { access_token: Some("s3cret-token".into()), ..hub.options() },
    );
    assert_eq!(send(&r, raw_get("/api/overview")).await.0, StatusCode::UNAUTHORIZED);
    // Liveness stays reachable.
    assert_eq!(send(&r, raw_get("/healthz")).await.0, StatusCode::OK);

    let bearer = |t: &str| {
        Request::builder()
            .uri("/api/overview")
            .header(header::HOST, HOST)
            .header(header::AUTHORIZATION, format!("Bearer {}", t))
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(send(&r, bearer("s3cret-token")).await.0, StatusCode::OK);
    assert_eq!(send(&r, bearer("nope")).await.0, StatusCode::UNAUTHORIZED);

    // Query tokens are not accepted anywhere.
    assert_eq!(send(&r, raw_get("/api/overview?token=s3cret-token")).await.0, StatusCode::UNAUTHORIZED);

    // The access token can be exchanged repeatedly (remote binds, several devices).
    for _ in 0..2 {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/session/bootstrap")
            .header(header::HOST, HOST)
            .header(header::ORIGIN, ORIGIN)
            .body(Body::from(r#"{"token":"s3cret-token"}"#))
            .unwrap();
        assert_eq!(send(&r, req).await.0, StatusCode::CREATED);
    }
}

#[tokio::test]
async fn malicious_event_summary_is_never_rendered_as_markup() {
    let hub = Hub::new();
    let payload = "<script>alert('xss')</script></script><img src=x onerror=alert(1)>";
    append_event(&hub.config, payload, "decision", &[], &["<b>evil.rs</b>".to_string()], Some("<svg onload=alert(2)>")).unwrap();
    let c = hub.client().await;

    let (status, _, page) = send(&c.router, raw_get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!page.contains("alert("));

    let (status, headers, body) = send(&c.router, c.get_req("/api/feed")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap().starts_with("application/json"));
    assert_eq!(headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
    let v = json(&body);
    let item = v["items"].as_array().unwrap().iter().find(|i| i["source"] == "event").unwrap();
    assert_eq!(item["summary"], payload);

    let html = render_dashboard_html(&hub.config);
    assert!(!html.contains("<script>"));
    assert!(!html.contains("<img"));
    assert!(html.contains("&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;"));
}

fn draft(id: &str, target: &str, content: &str) -> InboxDraft {
    InboxDraft {
        id: id.to_string(),
        target: target.to_string(),
        title: format!("Proposal {}", id),
        proposed_content: content.to_string(),
        reason: "Keep docs accurate".to_string(),
        author: "alex".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    }
}

#[tokio::test]
async fn approve_endpoint_writes_target_as_current_member() {
    let hub = Hub::new();
    let config = &hub.config;
    create_member(config, "sam", "Sam Lee", None, Some("reviewer")).unwrap();
    save_inbox_draft(config, &draft("draft_rl", "context/rate-limit.md", "# Rate limits\n\n100 rps.")).unwrap();
    let prop = publish_inbox_draft(config, "draft_rl").unwrap();
    let c = hub.client().await;

    let v = c.get_json(&format!("/api/inbox/{}", prop.id)).await;
    assert_eq!(v["targetExists"], false);
    assert!(v["diff"].as_array().unwrap().iter().all(|l| l["op"] == "+"));

    let uri = format!("/api/inbox/{}/approve", prop.id);
    let (status, body) = c.post(&uri, r#"{"note":"lgtm"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", body);
    assert!(json(&body)["detail"].as_str().unwrap().contains("member"));
    assert!(!config.scaffold_root.join("context/rate-limit.md").exists());

    select_current_member(config, "sam").unwrap();
    let v = c.post_json(&uri, r#"{"note":"lgtm"}"#).await;
    assert_eq!(v["proposal"]["status"], "approved");
    assert_eq!(v["proposal"]["decisionBy"], "sam");
    let written = fs::read_to_string(config.scaffold_root.join("context/rate-limit.md")).unwrap();
    assert!(written.contains("100 rps."));

    assert_eq!(c.post(&uri, "").await.0, StatusCode::CONFLICT);

    // Decided proposals carry no (misleading) diff.
    let v = c.get_json(&format!("/api/inbox/{}", prop.id)).await;
    assert_eq!(v["decided"], true);

    save_inbox_draft(config, &draft("draft_x", "context/other.md", "nope")).unwrap();
    let p2 = publish_inbox_draft(config, "draft_x").unwrap();
    let v = c.post_json(&format!("/api/inbox/{}/reject", p2.id), r#"{"note":"dup"}"#).await;
    assert_eq!(v["proposal"]["status"], "rejected");
    assert!(!config.scaffold_root.join("context/other.md").exists());
}

fn relay_draft(id: &str, sender: &str) -> RelayDraft {
    RelayDraft {
        id: id.to_string(),
        title: "Webhook <retries>".to_string(),
        summary: "Moved to exponential backoff".to_string(),
        sender: sender.to_string(),
        open_to_team: true,
        progress: vec!["unit tests pass".to_string()],
        next_actions: vec!["staging test".to_string()],
        created_at: chrono::Utc::now().to_rfc3339(),
        ..Default::default()
    }
}

#[tokio::test]
async fn relay_publish_claim_and_close_endpoints() {
    let hub = Hub::new();
    let config = &hub.config;
    create_member(config, "alex", "Alex Kim", None, None).unwrap();
    create_member(config, "sam", "Sam Lee", None, None).unwrap();
    select_current_member(config, "alex").unwrap();
    save_relay_draft(config, &relay_draft("rd_1", "alex")).unwrap();
    let c = hub.client().await;

    assert_eq!(c.get_json("/api/relays").await["drafts"].as_array().unwrap().len(), 1);
    let v = c.post_json("/api/relays/drafts/rd_1/publish", "").await;
    let relay_id = v["relay"]["id"].as_str().unwrap().to_string();

    select_current_member(config, "sam").unwrap();
    let v = c.post_json(&format!("/api/relays/{}/claim", relay_id), "").await;
    assert_eq!(v["relay"]["status"], "acknowledged");
    assert_eq!(v["relay"]["claimant"], "sam");
    assert_eq!(c.get_json("/api/contributor/sam").await["in_flight_relay"], "Webhook <retries>");
    assert_eq!(c.post(&format!("/api/relays/{}/claim", relay_id), "").await.0, StatusCode::CONFLICT);

    let v = c.post_json(&format!("/api/relays/{}/close", relay_id), "").await;
    assert_eq!(v["relay"]["status"], "closed");
    assert!(c.get_json("/api/contributor/sam").await["in_flight_relay"].is_null());
    assert!(c.get_json(&format!("/api/relays/{}", relay_id)).await["closedAt"].is_string());

    let status = c.post("/api/relays/..%2Fx/close", "").await.0;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn relay_claim_without_member_is_refused() {
    let hub = Hub::new();
    create_member(&hub.config, "alex", "Alex Kim", None, None).unwrap();
    save_relay_draft(&hub.config, &relay_draft("rd_2", "alex")).unwrap();
    let relay = publish_relay_draft(&hub.config, "rd_2").unwrap();
    let c = hub.client().await;
    let (status, body) = c.post(&format!("/api/relays/{}/claim", relay.id), "").await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", body);
}

#[tokio::test]
async fn sync_preview_endpoint_is_a_dry_run() {
    let hub = Hub::new();
    let c = hub.client().await;
    let v = c.post_json("/api/drift/sync", r#"{"dryRun":true}"#).await;
    assert_eq!(v["result"]["dry_run"], true);
    assert!(v["driftAfter"].is_null());
    assert_eq!(c.post_json("/api/drift/sync", "").await["result"]["dry_run"], true);
    let v = c.post_json("/api/drift/sync", r#"{"dryRun":false}"#).await;
    assert_eq!(v["result"]["dry_run"], false);
    assert!(v["driftAfter"]["score"].is_number());
    let (status, body) = c.post("/api/drift/sync", "not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json(&body)["code"], "INVALID_REQUEST");
}

#[tokio::test]
async fn contributors_have_no_fabricated_fallback() {
    let hub = Hub::new();
    append_event(&hub.config, "something happened", "decision", &[], &[], None).unwrap();
    let c = hub.client().await;
    let v = c.get_json("/api/contributors").await;
    assert!(v["contributors"].as_array().unwrap().is_empty());
    assert!(v["hint"].as_str().unwrap().contains("knobyte member add"));
}

#[tokio::test]
async fn fleet_marks_missing_projects_unavailable_and_registry_is_stable() {
    let hub = Hub::new();
    save_registry_at(
        &hub.registry,
        &[ProjectRegistryEntry {
            name: "ghost".to_string(),
            path: "/nonexistent/knobyte-ghost".to_string(),
            scaffold_root: "/nonexistent/knobyte-ghost/.knobyte".to_string(),
            mode: "code-repo".to_string(),
            last_active: "2026-01-01T00:00:00Z".to_string(),
        }],
    )
    .unwrap();
    let c = hub.client().await;
    let v = c.get_json("/api/fleet").await;
    let ghost = v["projects"].as_array().unwrap().iter().find(|p| p["name"] == "ghost").unwrap();
    assert_eq!(ghost["available"], false);
    assert_eq!(v["stats"]["unavailableProjects"], 1);
    let before = fs::read_to_string(&hub.registry).unwrap();
    assert_eq!(load_registry_at(&hub.registry).len(), 2);
    c.get_json("/api/fleet").await;
    assert_eq!(before, fs::read_to_string(&hub.registry).unwrap());
}

#[tokio::test]
async fn understand_endpoints_respond() {
    let hub = Hub::new();
    let c = hub.client().await;
    for uri in ["/api/overview", "/api/graph/context", "/api/search?q=rate", "/api/inbox", "/api/specs", "/api/drift"] {
        c.get_json(uri).await;
    }
    let v = c.get_json("/api/overview").await;
    assert_eq!(v["mcpTools"].as_array().unwrap().len(), knobyte::mcp::get_tools_list().len());
    assert_eq!(v["bind"], HOST);
    let (status, body) = c.get("/api/wiki/entity?id=does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["code"], "NOT_FOUND");
}
