//! Hub Playbooks and Catch Up: read endpoints, preview -> apply writes through
//! the shared team operation protocol, session/CSRF enforcement, embedded pages.

mod hub_support;

use axum::http::StatusCode;
use serde_json::json as j;

use hub_support::*;

#[tokio::test]
async fn playbooks_and_catch_up_through_the_hub() {
    let hub = Hub::new();
    let c = hub.client().await;
    c.run_action(j!({ "kind": "member.add", "member": { "id": "ada", "displayName": "Ada" } })).await;
    c.run_action(j!({ "kind": "member.select", "memberId": "ada" })).await;

    // Create (preview first: nothing written), then publish and start a run.
    let action = j!({ "kind": "playbook.create", "playbook": { "title": "Rotate keys", "steps": [
        { "id": "gen", "title": "Generate", "expectedEvidence": ["key id"] }, { "id": "roll", "title": "Roll out" } ] } });
    let env = c.post_json("/api/team/operations/preview", &j!({ "action": action }).to_string()).await;
    assert!(env["envelope"]["preview"]["changes"].as_array().unwrap().iter().any(|ch| ch["path"] == "playbooks/rotate-keys.json"));
    assert!(!hub.config.scaffold_root.join("playbooks/rotate-keys.json").exists());
    let applied = c.post_json("/api/team/operations/apply", &j!({ "envelope": env["envelope"] }).to_string()).await;
    assert_eq!(applied["result"]["state"], "draft");

    let list = c.get_json("/api/playbooks").await;
    assert_eq!(list["items"][0]["id"], "rotate-keys");
    assert_eq!(c.get_json("/api/playbooks?state=active").await["items"].as_array().unwrap().len(), 0);
    assert_eq!(c.get("/api/playbooks?state=bogus").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(c.get("/api/playbooks/ghost").await.0, StatusCode::NOT_FOUND);
    assert_eq!(c.get("/api/playbooks/..%2Fx").await.0, StatusCode::BAD_REQUEST);

    c.run_action(j!({ "kind": "playbook.update", "playbookId": "rotate-keys", "patch": { "state": "active" } })).await;
    let run = c.run_action(j!({ "kind": "playbook.run.start", "playbookId": "rotate-keys" })).await;
    let run_id = run["result"]["id"].as_str().unwrap().to_string();
    let done = c.run_action(j!({ "kind": "playbook.run.complete-step", "runId": run_id, "stepId": "gen",
        "evidence": [{ "kind": "manual", "note": "key 42" }] })).await;
    assert_eq!(done["result"]["steps"][0]["completedBy"], "ada");

    let detail = c.get_json("/api/playbooks/rotate-keys").await;
    assert_eq!(detail["runs"][0]["stepsCompleted"], 1);
    let runs = c.get_json("/api/playbook-runs?playbook=rotate-keys&state=active").await;
    assert_eq!(runs["items"].as_array().unwrap().len(), 1);
    let r = c.get_json(&format!("/api/playbook-runs/{}", run_id)).await;
    assert_eq!(r["steps"][0]["evidence"][0]["note"], "key 42");
    assert_eq!(c.get("/api/playbook-runs/nope").await.0, StatusCode::NOT_FOUND);

    // Catch up: digest, then mark through preview/apply (local scope only).
    let d = c.get_json("/api/catch-up?includeMine=true").await;
    assert_eq!(d["actorId"], "ada");
    assert!(d["items"].as_array().unwrap().iter().any(|i| i["group"] == "playbooks"));
    let env = c.post_json("/api/team/operations/preview", &j!({ "action": { "kind": "catchup.mark", "at": d["observedAt"] } }).to_string()).await;
    assert_eq!(env["envelope"]["preview"]["scope"], "local");
    c.post_json("/api/team/operations/apply", &j!({ "envelope": env["envelope"] }).to_string()).await;
    let d = c.get_json("/api/catch-up?includeMine=true").await;
    assert_eq!(d["baselineSource"], "cursor");
    assert!(d["items"].as_array().unwrap().is_empty(), "{}", d);
    assert_eq!(c.get("/api/catch-up?since=garbage").await.0, StatusCode::BAD_REQUEST);

    // Writes still need the session's CSRF token.
    let mut no_csrf = c.post_req("/api/team/operations/preview", &j!({ "action": { "kind": "catchup.mark" } }).to_string());
    no_csrf.headers_mut().remove(knobyte::hub::CSRF_HEADER);
    let (status, _, _) = send(&c.router, no_csrf).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Reads need a session.
    let (status, _, _) = send(&c.router, raw_get("/api/catch-up")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The pages are embedded and served as same-origin modules.
    for asset in ["/assets/js/pages/playbooks.js", "/assets/js/pages/catchup.js"] {
        let (status, body) = c.get(asset).await;
        assert_eq!(status, StatusCode::OK, "{}", asset);
        assert!(body.contains("export async function page"));
    }
    let (_, main) = c.get("/assets/js/main.js").await;
    assert!(main.contains("'/playbooks'") && main.contains("'/catch-up'"));
}
