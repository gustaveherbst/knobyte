//! Regression tests for Hub fixes: wiki health from recorded content hashes, inbox approval
//! reachability, bounded wiki listings, workstream archive, grounding display, routes,
//! specs/activity paging, member aliases and event-stream limits.

mod hub_support;

use std::fs;

use axum::http::StatusCode;
use serde_json::Value;

use hub_support::*;
use knobyte::team::inbox::{publish_inbox_draft, save_inbox_draft, InboxDraft};
use knobyte::team::members::{create_member, select_current_member};
use knobyte::wiki::WikiIndex;

fn wiki_service(h: &Value) -> Value {
    h["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "wiki")
        .unwrap()
        .clone()
}

fn build_wiki(hub: &Hub) {
    let mut idx = WikiIndex::open_for_rebuild(&hub.config.wiki_db_path()).unwrap();
    idx.rebuild(&hub.config.scaffold_root).unwrap();
}

#[tokio::test]
async fn wiki_health_compares_content_not_mtime_and_approval_refreshes() {
    let hub = Hub::new();
    build_wiki(&hub);
    let c = hub.client().await;

    let h = c.get_json("/api/health").await;
    assert_eq!(wiki_service(&h)["wiki"]["status"], "fresh", "{}", h);

    // A Markdown edit made outside Knobyte; reading the index must not mask it.
    fs::write(
        hub.config.scaffold_root.join("context/new-note.md"),
        "---\nid: kb_new_note\ntitle: New note\n---\n# New note\n",
    )
    .unwrap();
    let _ = c.get_json("/api/wiki/entities").await;
    let h = c.get_json("/api/health").await;
    let wiki = wiki_service(&h);
    assert_eq!(wiki["wiki"]["status"], "stale", "{}", wiki);
    assert_eq!(wiki["wiki"]["recommendedJob"], "wiki_refresh");
    assert_eq!(wiki["wiki"]["changes"]["added"], 1);
    build_wiki(&hub);

    // Inbox approval writes a new file; the entity is reachable and health is fresh.
    create_member(&hub.config, "sam", "Sam Lee", None, Some("reviewer")).unwrap();
    select_current_member(&hub.config, "sam").unwrap();
    save_inbox_draft(
        &hub.config,
        &InboxDraft {
            id: "draft_rl".into(),
            target: "context/rate-limit.md".into(),
            title: "Rate limits".into(),
            proposed_content: "---\nid: kb_rate_limits\ntitle: Rate limits\n---\n# Rate limits\n\n100 rps.\n".into(),
            reason: "Docs".into(),
            author: "alex".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        },
    )
    .unwrap();
    let prop = publish_inbox_draft(&hub.config, "draft_rl").unwrap();
    let v = c
        .post_json(&format!("/api/inbox/{}/approve", prop.id), r#"{"note":"ok"}"#)
        .await;
    assert_eq!(v["wikiRefreshed"], true, "{}", v);
    let e = c.get_json("/api/wiki/entity?id=kb_rate_limits").await;
    assert_eq!(e["entity"]["id"].as_str().or(e["id"].as_str()), Some("kb_rate_limits"), "{}", e);
    let h = c.get_json("/api/health").await;
    assert_eq!(wiki_service(&h)["wiki"]["status"], "fresh");
}

#[tokio::test]
async fn hub_wiki_entities_are_bounded_and_paged() {
    let hub = Hub::new();
    for i in 0..4 {
        fs::write(
            hub.config.scaffold_root.join(format!("context/e{}.md", i)),
            format!("---\nid: kb_e{}\ntitle: E{}\n---\n# E{}\n\nBody.\n", i, i, i),
        )
        .unwrap();
    }
    fs::write(
        hub.config.scaffold_root.join("context/arch.md"),
        "---\nid: kb_arch\ntitle: Arch\nstatus: archived\n---\n# Arch\n",
    )
    .unwrap();
    build_wiki(&hub);
    let c = hub.client().await;
    let v = c.get_json("/api/wiki/entities?limit=2").await;
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    assert_eq!(v["truncated"], true);
    assert_eq!(v["nextOffset"], 2);
    assert!(v["items"][0].get("body").is_none());
    let all = c.get_json("/api/wiki/entities?limit=500").await;
    assert!(all["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["id"] != "kb_arch"));
    let with = c
        .get_json("/api/wiki/entities?limit=500&includeArchived=true&includeBody=true&type=")
        .await;
    assert!(with["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["id"] == "kb_arch" && i["body"].is_string()));
    assert_eq!(c.get("/api/wiki/entities?limit=abc").await.0, StatusCode::BAD_REQUEST);
}

async fn run_job(c: &Client, body: &str) -> Value {
    let job = c.post_json("/api/jobs", body).await;
    let id = job["id"].as_str().unwrap().to_string();
    let done = c
        .poll(&format!("/api/jobs/{}", id), |v| {
            !["queued", "running"].contains(&v["state"].as_str().unwrap())
        })
        .await;
    assert_eq!(done["state"], "succeeded", "{}", done);
    done
}

#[tokio::test]
async fn readable_groundings_show_resolved_in_entity_and_context() {
    let hub = Hub::new();
    let root = hub.root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "/// Adds.\npub fn add_numbers(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n").unwrap();
    fs::write(
        hub.config.scaffold_root.join("context/adder.md"),
        "---\nid: kb_adder\ntitle: Adder\ngrounds_to: [function:src/lib.rs:add_numbers]\n---\n# Adder\n",
    )
    .unwrap();
    let c = hub.client().await;
    run_job(&c, r#"{"kind":"graph_rebuild","confirm":true}"#).await;
    run_job(&c, r#"{"kind":"wiki_refresh"}"#).await;

    let e = c.get_json("/api/wiki/entity?id=kb_adder").await;
    let g = &e["groundings"][0];
    assert_eq!(g["nodeId"], "function:src/lib.rs:add_numbers");
    assert_eq!(g["resolved"], true, "{}", e);
    assert_eq!(g["node"]["name"], "add_numbers");

    let ctx = c.get_json("/api/graph/context").await;
    let node = ctx["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["nodeId"] == "function:src/lib.rs:add_numbers")
        .unwrap_or_else(|| panic!("{}", ctx))
        .clone();
    assert_eq!(node["group"], "code", "{}", node);
}

#[tokio::test]
async fn self_approval_refusal_maps_to_checkbox_code() {
    let hub = Hub::new();
    create_member(&hub.config, "alex", "Alex", None, Some("maintainer")).unwrap();
    select_current_member(&hub.config, "alex").unwrap();
    save_inbox_draft(
        &hub.config,
        &InboxDraft {
            id: "draft_self".into(),
            target: "context/self.md".into(),
            title: "Self".into(),
            proposed_content: "# Self\n".into(),
            reason: "r".into(),
            author: "alex".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        },
    )
    .unwrap();
    let prop = publish_inbox_draft(&hub.config, "draft_self").unwrap();
    let c = hub.client().await;
    let action = serde_json::json!({ "action": { "kind": "inbox.approve", "proposalId": prop.id } });
    let (status, body) = c.post("/api/team/operations/preview", &action.to_string()).await;
    assert!(status.is_client_error(), "{} {}", status, body);
    let v = json(&body);
    assert_eq!(v["code"], "SELF_APPROVAL_REQUIRED", "{}", body);
    assert!(!v["detail"].as_str().unwrap().contains("--self-approve"));
    let js = c.get("/assets/js/pages/inbox.js").await.1;
    assert!(js.contains("SELF_APPROVAL_REQUIRED"));
}

#[tokio::test]
async fn workstream_archive_action_and_code_route() {
    let hub = Hub::new();
    create_member(&hub.config, "alex", "Alex", None, Some("maintainer")).unwrap();
    select_current_member(&hub.config, "alex").unwrap();
    let c = hub.client().await;
    let created = c
        .run_action(serde_json::json!({ "kind": "workstream.create", "workstream": { "title": "Billing", "state": "active" } }))
        .await;
    let id = created["result"]["id"].as_str().unwrap_or_else(|| panic!("{}", created)).to_string();
    // Editing to "archived" is refused; the dedicated action works.
    let (status, _) = c
        .post(
            "/api/team/operations/preview",
            &serde_json::json!({ "action": { "kind": "workstream.update", "workstreamId": id, "patch": { "state": "archived" } } }).to_string(),
        )
        .await;
    assert!(status.is_client_error());
    c.run_action(serde_json::json!({ "kind": "workstream.archive", "workstreamId": id })).await;
    let w = c.get_json(&format!("/api/workstreams/{}", id)).await;
    assert_eq!(w["workstream"]["status"], "archived", "{}", w);

    let js = c.get("/assets/js/pages/workstreams.js").await.1;
    assert!(js.contains("kind: 'workstream.archive'"));
    assert!(js.contains("EDIT_STATES.map("), "edit form offers only editable states");
    let main = c.get("/assets/js/main.js").await.1;
    assert!(main.contains("route('/code', "), "the /code nav target has a route");
    let (status, page) = c.get("/code").await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("/assets/js/main.js"));
}

#[tokio::test]
async fn specs_filter_by_lifecycle_states() {
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
    let ids = |v: &Value| -> Vec<String> {
        v["specs"].as_array().unwrap().iter().map(|s| s["id"].as_str().unwrap().to_string()).collect()
    };
    let all = c.get_json("/api/specs").await;
    assert!(!ids(&all).contains(&"kb_s3".to_string()), "archived hidden by default: {}", all);
    let one = c.get_json("/api/specs?lifecycleStates=in_flight").await;
    assert_eq!(ids(&one), vec!["kb_s2"]);
    let two = c.get_json("/api/specs?lifecycleStates=promoted,archived").await;
    let mut got = ids(&two);
    got.sort();
    assert_eq!(got, vec!["kb_s1", "kb_s3"]);
    assert_eq!(c.get("/api/specs?lifecycleStates=bogus").await.0, StatusCode::BAD_REQUEST);
    let detail = c.get_json("/api/specs/kb_s1").await;
    assert!(detail["hierarchy"].is_object() && detail["groundingRollup"].is_object());
    let js = c.get("/assets/js/pages/specs.js").await.1;
    assert!(js.contains("lifecycleStates") && js.contains("groundingRollup") && js.contains("acceptanceCriteria"));
}

#[tokio::test]
async fn activity_feed_pages_since_and_context() {
    let hub = Hub::new();
    for i in 0..5 {
        knobyte::events::append_event(&hub.config, &format!("decision {}", i), "decision", &[], &[], None).unwrap();
    }
    let c = hub.client().await;
    let p1 = c.get_json("/api/feed?limit=2").await;
    assert_eq!(p1["items"].as_array().unwrap().len(), 2);
    assert_eq!(p1["nextOffset"], 2);
    assert!(p1["total"].as_u64().unwrap() >= 5);
    let p2 = c.get_json("/api/feed?limit=2&offset=2").await;
    assert_ne!(p1["items"][0]["id"], p2["items"][0]["id"]);
    assert!(p2["items"][0]["context"].is_object());
    let future = c.get_json("/api/feed?since=2999-01-01").await;
    assert_eq!(future["total"], 0);
    let recent = c.get_json("/api/feed?since=1d&kind=decision").await;
    assert!(recent["total"].as_u64().unwrap() >= 5);
    assert_eq!(c.get("/api/feed?since=yesterday-ish").await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn member_dialogs_edit_git_aliases() {
    let hub = Hub::new();
    let c = hub.client().await;
    let js = c.get("/assets/js/pages/members.js").await.1;
    assert!(js.contains("mem-aliases") && js.contains("patch.gitAliases"));
    c.run_action(serde_json::json!({ "kind": "member.add", "member": { "id": "ada", "displayName": "Ada",
        "gitAliases": [{ "name": "Ada L", "email": "ada@example.com" }] } }))
        .await;
    c.run_action(serde_json::json!({ "kind": "member.update", "memberId": "ada",
        "patch": { "gitAliases": [{ "email": "ada@work.example" }] } }))
        .await;
    let m = c.get_json("/api/team/members/ada").await;
    let text = m.to_string();
    assert!(text.contains("ada@work.example"), "{}", text);
    assert!(!text.contains("ada@example.com"), "{}", text);
}

#[tokio::test]
async fn event_streams_are_capped_and_end_on_logout() {
    use knobyte::hub::{build_hub_app, Executor, JobContext, JobFailure, JobKind};
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;
    let hub = Hub::new();
    let mut opts = hub.options();
    let mut ex: HashMap<JobKind, Executor> = HashMap::new();
    ex.insert(
        JobKind::GraphRefresh,
        Arc::new(|ctx: &JobContext| {
            for _ in 0..3000 {
                ctx.checkpoint()?;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(JobFailure::Failed("not cancelled".into()))
        }) as Executor,
    );
    opts.job_executors = ex;
    let app = build_hub_app(hub.config.clone(), opts);
    let security = app.state.security.clone();
    let c = Client::login(app.router).await;
    let job = c.post_json("/api/jobs", r#"{"kind":"graph_refresh"}"#).await;
    let id = job["id"].as_str().unwrap().to_string();
    let events = format!("/api/jobs/{}/events", id);

    // At the cap, a new stream is refused with 429.
    let held: Vec<_> = (0..knobyte::hub::security::MAX_EVENT_STREAMS)
        .map(|_| security.acquire_stream().unwrap())
        .collect();
    let (status, body) = c.get(&events).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{}", body);
    assert_eq!(json(&body)["code"], "TOO_MANY_STREAMS");
    drop(held);
    assert_eq!(security.open_streams(), 0);

    // An open stream ends when its session logs out.
    let req = c.get_req(&events);
    let router = c.router.clone();
    let reader = tokio::spawn(async move {
        use tower::ServiceExt;
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(security.open_streams(), 1);
    let (status, _) = c.post("/api/session/logout", "{}").await;
    assert!(status.is_success());
    let body = tokio::time::timeout(Duration::from_secs(15), reader)
        .await
        .expect("stream ended after logout")
        .unwrap();
    assert!(body.contains("event: session-ended"), "{}", body);
    assert_eq!(security.open_streams(), 0);
}
