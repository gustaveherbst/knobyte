//! Regression tests for Hub review fixes: readable grounding refs on code
//! endpoints, problem+json query rejections, legacy approve self-approval,
//! exact Host port, member.add attribution, cancel-at-finish and spec totals.

mod hub_support;

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};

use hub_support::*;
use knobyte::hub::{build_hub_app, Executor, JobContext, JobKind, JobOutput};
use knobyte::team::inbox::{publish_inbox_draft, save_inbox_draft, InboxDraft};
use knobyte::team::members::{create_member, select_current_member};

async fn run_job(c: &Client, body: &str) -> Value {
    let job = c.post_json("/api/jobs", body).await;
    let id = job["id"].as_str().unwrap().to_string();
    let done = c.poll(&format!("/api/jobs/{}", id), |v| !["queued", "running"].contains(&v["state"].as_str().unwrap())).await;
    assert_eq!(done["state"], "succeeded", "{}", done);
    done
}

#[tokio::test]
async fn code_endpoints_resolve_readable_grounding_refs() {
    let hub = Hub::new();
    let root = hub.root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn double(x: i32) -> i32 {\n    x * 2\n}\n").unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n").unwrap();
    let c = hub.client().await;
    run_job(&c, r#"{"kind":"graph_rebuild","confirm":true}"#).await;

    let readable = "function%3Asrc%2Flib.rs%3Adouble";
    let node = c.get_json(&format!("/api/code/node?id={}", readable)).await;
    assert_eq!(node["node"]["name"], "double", "{}", node);
    let sym = c.get_json(&format!("/api/code/symbol?id={}", readable)).await;
    assert_eq!(sym["node"]["name"], "double", "{}", sym);
    let src = c.get_json(&format!("/api/code/symbol/source?id={}", readable)).await;
    assert!(src.to_string().contains("x * 2"), "{}", src);

    let (status, body) = c.get("/api/code/node?id=function%3Asrc%2Flib.rs%3Anope").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", body);
    assert_eq!(json(&body)["code"], "NOT_FOUND");
    // Clients prefer the server-resolved id for code links.
    let js = c.get("/assets/js/pages/understand.js").await.1;
    assert!(js.contains("nd.resolvedId || nd.nodeId"));
}

#[tokio::test]
async fn query_rejections_are_problem_json() {
    let hub = Hub::new();
    let c = hub.client().await;
    for uri in ["/api/feed?limit=abc", "/api/feed?limit=-3", "/api/wiki/entity", "/api/setup/transcript", "/api/code/node"] {
        let (status, headers, body) = send(&c.router, c.get_req(uri)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{} -> {}", uri, body);
        assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "application/problem+json", "{}", uri);
        let p = json(&body);
        assert_eq!(p["code"], "INVALID_REQUEST", "{}", uri);
        assert_eq!(p["status"], 400);
    }
}

#[tokio::test]
async fn legacy_approve_route_requires_self_approval() {
    let hub = Hub::new();
    create_member(&hub.config, "ada", "Ada", None, None).unwrap();
    select_current_member(&hub.config, "ada").unwrap();
    let d = InboxDraft {
        id: "d1".into(),
        target: "context/limits.md".into(),
        title: "Limits".into(),
        proposed_content: "100 rps".into(),
        reason: "accuracy".into(),
        author: "ada".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        ..Default::default()
    };
    save_inbox_draft(&hub.config, &d).unwrap();
    let p = publish_inbox_draft(&hub.config, "d1").unwrap();
    let c = hub.client().await;
    let (status, body) = c.post(&format!("/api/inbox/{}/approve", p.id), "{}").await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", body);
    assert_eq!(json(&body)["code"], "SELF_APPROVAL_REQUIRED");
    assert!(!hub.config.scaffold_root.join("context/limits.md").exists());
}

#[tokio::test]
async fn host_header_must_name_the_bound_port() {
    let hub = Hub::new();
    let router = hub.router();
    let req = |host: &str| Request::builder().uri("/api/session").header(header::HOST, host).body(Body::empty()).unwrap();
    assert_ne!(send(&router, req(HOST)).await.0, StatusCode::FORBIDDEN);
    for bad in ["127.0.0.1:4001", "localhost:80", "127.0.0.1"] {
        let (status, _, body) = send(&router, req(bad)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{} -> {}", bad, body);
        assert_eq!(json(&body)["code"], "ORIGIN_REJECTED");
    }
}

#[tokio::test]
async fn member_add_activity_names_the_real_actor() {
    let hub = Hub::new();
    let c = hub.client().await;
    let actor_of = |id: &str| {
        knobyte::team::activity::list_activity(&hub.config, 100)
            .into_iter()
            .find(|a| a.action == "member.add" && a.entity_id == id)
            .unwrap()
            .actor
    };
    c.run_action(json!({ "kind": "member.add", "member": { "id": "ada", "displayName": "Ada" } })).await;
    assert_eq!(actor_of("ada"), "ada");
    // No member selected: the created member is never recorded as the actor.
    c.run_action(json!({ "kind": "member.add", "member": { "id": "bob", "displayName": "Bob" } })).await;
    assert_ne!(actor_of("bob"), "bob");
    c.run_action(json!({ "kind": "member.select", "memberId": "ada" })).await;
    c.run_action(json!({ "kind": "member.add", "member": { "id": "cy", "displayName": "Cy" } })).await;
    assert_eq!(actor_of("cy"), "ada");
}

#[tokio::test]
async fn cancel_at_finish_ends_interrupted_not_succeeded() {
    let hub = Hub::new();
    let mut opts = hub.options();
    let mut ex: HashMap<JobKind, Executor> = HashMap::new();
    // Finishes its work right after the cancel lands, without another checkpoint.
    ex.insert(
        JobKind::GraphRefresh,
        Arc::new(|ctx: &JobContext| {
            ctx.phase("discover", 1, Some(1), None);
            for _ in 0..2000 {
                if ctx.cancelled() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(JobOutput { summary: "done".into(), result: json!({ "ok": true }) })
        }) as Executor,
    );
    opts.job_executors = ex;
    let c = Client::login(build_hub_app(hub.config.clone(), opts).router).await;
    let job = c.post_json("/api/jobs", r#"{"kind":"graph_refresh"}"#).await;
    let id = job["id"].as_str().unwrap().to_string();
    c.poll(&format!("/api/jobs/{}", id), |v| v["state"] == "running" && v["phase"] == "discover").await;
    c.post_json(&format!("/api/jobs/{}/cancel", id), "{}").await;
    let done = c.poll(&format!("/api/jobs/{}", id), |v| !["queued", "running"].contains(&v["state"].as_str().unwrap())).await;
    assert_eq!(done["state"], "interrupted", "{}", done);
    assert_eq!(done["interruptedReason"], "user_cancelled");
    assert_eq!(done["cancelRequested"], true);

    // A job that already finished stays succeeded and does not pick up a cancel request.
    let mut opts = hub.options();
    let mut ex: HashMap<JobKind, Executor> = HashMap::new();
    ex.insert(JobKind::CozoSync, Arc::new(|_: &JobContext| Ok(JobOutput { summary: "synced".into(), result: json!({}) })) as Executor);
    opts.job_executors = ex;
    let c = Client::login(build_hub_app(hub.config.clone(), opts).router).await;
    let job = c.post_json("/api/jobs", r#"{"kind":"cozo_sync"}"#).await;
    let id = job["id"].as_str().unwrap().to_string();
    let done = c.poll(&format!("/api/jobs/{}", id), |v| v["state"] == "succeeded").await;
    assert_eq!(done["cancelRequested"], false);
    let after = c.post_json(&format!("/api/jobs/{}/cancel", id), "{}").await;
    assert_eq!(after["state"], "succeeded");
    assert_eq!(after["cancelRequested"], false);
}

#[tokio::test]
async fn filtered_specs_report_filtered_total() {
    let hub = Hub::new();
    let specs = hub.config.scaffold_root.join("specs");
    fs::create_dir_all(&specs).unwrap();
    for (id, status) in [("kb_s1", "promoted"), ("kb_s2", "in_flight"), ("kb_s3", "archived")] {
        fs::write(
            specs.join(format!("{}.md", id)),
            format!("---\nid: {}\ntype: spec\ntitle: {}\nstatus: {}\n---\n# {}\n", id, id, status, id),
        )
        .unwrap();
    }
    let c = hub.client().await;
    let one = c.get_json("/api/specs?lifecycleStates=in_flight").await;
    assert_eq!(one["total"], 1, "{}", one);
    assert_eq!(one["unfilteredTotal"], 3);
    let default = c.get_json("/api/specs").await;
    assert_eq!(default["total"], default["specs"].as_array().unwrap().len());
}
