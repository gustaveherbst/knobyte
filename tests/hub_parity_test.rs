//! Project Hub parity: session bootstrap, problem+json, background jobs,
//! setup wizard (fake agent CLI only), team preview/apply mutations, search,
//! symbol workspace, health/overview/shell, settings and the embedded UI.

mod hub_support;

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use serde_json::{json as j, Value};

use hub_support::*;
use knobyte::hub::{build_hub_app, Executor, JobContext, JobFailure, JobKind, JobOutput};

fn bootstrap_req(token: &str, origin: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri("/api/session/bootstrap")
        .header(header::HOST, HOST)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(o) = origin {
        b = b.header(header::ORIGIN, o);
    }
    b.body(Body::from(j!({ "token": token }).to_string())).unwrap()
}

#[tokio::test]
async fn bootstrap_token_is_exchanged_once_for_a_session_cookie() {
    let hub = Hub::new();
    let r = hub.router();

    // No session: API refused (even on loopback), page and assets served.
    let (status, headers, body) = send(&r, raw_get("/api/shell")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "application/problem+json");
    let p = json(&body);
    assert_eq!(p["code"], "UNAUTHORIZED");
    assert_eq!(p["status"], 401);
    assert_eq!(p["type"], "about:blank");
    assert_eq!(send(&r, raw_get("/")).await.0, StatusCode::OK);
    assert_eq!(send(&r, raw_get("/assets/app.css")).await.0, StatusCode::OK);

    // Cross-origin or origin-less exchange is refused.
    assert_eq!(send(&r, bootstrap_req(BOOT, Some("http://evil.example"))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(send(&r, bootstrap_req(BOOT, None)).await.0, StatusCode::FORBIDDEN);
    // Wrong token.
    assert_eq!(send(&r, bootstrap_req("wrong", Some(ORIGIN))).await.0, StatusCode::UNAUTHORIZED);

    let (status, headers, body) = send(&r, bootstrap_req(BOOT, Some(ORIGIN))).await;
    assert_eq!(status, StatusCode::CREATED, "{}", body);
    assert!(json(&body)["expiresAt"].is_string());
    let cookie = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
    assert!(cookie.starts_with("knobyte_hub_session_"));
    for attr in ["HttpOnly", "SameSite=Strict", "Path=/api", "Max-Age="] {
        assert!(cookie.contains(attr), "{} missing in {}", attr, cookie);
    }

    // One-time: a second exchange fails.
    assert_eq!(send(&r, bootstrap_req(BOOT, Some(ORIGIN))).await.0, StatusCode::UNAUTHORIZED);

    let pair = cookie.split(';').next().unwrap().to_string();
    let authed = |uri: &str| Request::builder().uri(uri).header(header::HOST, HOST).header(header::COOKIE, pair.clone()).body(Body::empty()).unwrap();
    let (status, _, body) = send(&r, authed("/api/session")).await;
    assert_eq!(status, StatusCode::OK);
    let csrf = json(&body)["csrfToken"].as_str().unwrap().to_string();
    assert!(csrf.len() >= 32);
    assert_eq!(send(&r, authed("/api/shell")).await.0, StatusCode::OK);

    // A forged cookie value is refused.
    let forged = Request::builder().uri("/api/shell").header(header::HOST, HOST)
        .header(header::COOKIE, format!("{}=forged", pair.split('=').next().unwrap())).body(Body::empty()).unwrap();
    assert_eq!(send(&r, forged).await.0, StatusCode::UNAUTHORIZED);

    // Logout revokes the session.
    let logout = Request::builder().method(Method::POST).uri("/api/session/logout").header(header::HOST, HOST)
        .header(header::ORIGIN, ORIGIN).header(header::COOKIE, pair.clone()).header("x-knobyte-csrf", csrf).body(Body::empty()).unwrap();
    assert_eq!(send(&r, logout).await.0, StatusCode::NO_CONTENT);
    assert_eq!(send(&r, authed("/api/shell")).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn proxy_headers_unknown_routes_and_problem_details() {
    let hub = Hub::new();
    let c = hub.client().await;
    let mut req = c.get_req("/api/shell");
    req.headers_mut().insert("x-forwarded-for", "10.0.0.1".parse().unwrap());
    let (status, _, body) = send(&c.router, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json(&body)["code"], "INVALID_REQUEST");

    let (status, body) = c.get("/api/does-not-exist").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["code"], "NOT_FOUND");

    // Unknown pages get the shell (the client renders its 404 page).
    let (status, _, body) = send(&c.router, raw_get("/code/symbols/fn%3Aabc")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("/assets/js/main.js"));
    assert_eq!(send(&c.router, raw_get("/assets/js/nope.js")).await.0, StatusCode::NOT_FOUND);
}

fn blocking_executor() -> Executor {
    Arc::new(|ctx: &JobContext| {
        ctx.phase("discover", 1, Some(10), Some("waiting for cancel".into()));
        for _ in 0..1000 {
            ctx.checkpoint()?;
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(JobFailure::Failed("not cancelled in time".into()))
    })
}

#[tokio::test]
async fn jobs_lifecycle_cancel_history_and_events() {
    let hub = Hub::new();
    let mut opts = hub.options();
    let mut ex: HashMap<JobKind, Executor> = HashMap::new();
    ex.insert(JobKind::GraphRefresh, blocking_executor());
    ex.insert(JobKind::CozoSync, Arc::new(|_: &JobContext| Ok(JobOutput { summary: "synced".into(), result: j!({ "ok": true }) })));
    opts.job_executors = ex;
    let c = Client::login(build_hub_app(hub.config.clone(), opts).router).await;

    // Unknown kind / missing confirmation for destructive kinds.
    assert_eq!(c.post("/api/jobs", r#"{"kind":"format_disk"}"#).await.0, StatusCode::BAD_REQUEST);
    let (status, body) = c.post("/api/jobs", r#"{"kind":"graph_rebuild"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{}", body);
    assert!(json(&body)["detail"].as_str().unwrap().contains("confirm"));

    let (status, body) = c.post("/api/jobs", r#"{"kind":"graph_refresh"}"#).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{}", body);
    let job = json(&body);
    let id = job["id"].as_str().unwrap().to_string();
    assert_eq!(job["state"], "queued");
    assert_eq!(job["phases"][0], "discover");
    let running = c.poll(&format!("/api/jobs/{}", id), |v| v["state"] == "running" && v["progress"]["total"] == 10).await;
    assert_eq!(running["phase"], "discover");

    // A second job is refused while one is active, naming the active job.
    let (status, body) = c.post("/api/jobs", r#"{"kind":"cozo_sync"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let p = json(&body);
    assert_eq!(p["code"], "JOB_ALREADY_RUNNING");
    assert_eq!(p["activeJobId"], id.as_str());
    assert_eq!(c.get_json("/api/jobs").await["active"]["id"], id.as_str());

    let v = c.post_json(&format!("/api/jobs/{}/cancel", id), "{}").await;
    assert_eq!(v["cancelRequested"], true);
    let done = c.poll(&format!("/api/jobs/{}", id), |v| v["state"] == "interrupted").await;
    assert_eq!(done["interruptedReason"], "user_cancelled");
    assert!(done["finishedAt"].is_string());

    // Cancel of a terminal job is a no-op returning the snapshot.
    assert_eq!(c.post_json(&format!("/api/jobs/{}/cancel", id), "{}").await["state"], "interrupted");

    // The SSE stream of a finished job delivers the terminal snapshot and ends.
    let (status, headers, body) = send(&c.router, c.get_req(&format!("/api/jobs/{}/events", id))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap().starts_with("text/event-stream"));
    assert!(body.contains("event: terminal"), "{}", body);
    assert!(body.contains("user_cancelled"));

    // Next job runs; history keeps both, newest first.
    let (status, body) = c.post("/api/jobs", r#"{"kind":"cozo_sync"}"#).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{}", body);
    let id2 = json(&body)["id"].as_str().unwrap().to_string();
    let ok = c.poll(&format!("/api/jobs/{}", id2), |v| v["state"] == "succeeded").await;
    assert_eq!(ok["summary"], "synced");
    let list = c.get_json("/api/jobs").await;
    let items = list["items"].as_array().unwrap();
    assert_eq!(items[0]["id"], id2.as_str());
    assert_eq!(items[1]["id"], id.as_str());
    assert!(list["active"].is_null());

    // Bad ids.
    assert_eq!(c.get("/api/jobs/..%2Fx").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(c.get("/api/jobs/job_0000").await.0, StatusCode::NOT_FOUND);

    // History survives a Hub restart.
    let c2 = Client::login(hub.router()).await;
    let list = c2.get_json("/api/jobs").await;
    assert_eq!(list["items"].as_array().unwrap().len(), 2);
}

fn write_sources(root: &std::path::Path) {
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "/// Adds two numbers.\npub fn add_numbers(a: i32, b: i32) -> i32 {\n    a + b\n}\n\n/// Sums a slice with add_numbers.\npub fn total_sum(xs: &[i32]) -> i32 {\n    let mut t = 0;\n    for x in xs {\n        t = add_numbers(t, *x);\n    }\n    t\n}\n",
    )
    .unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n").unwrap();
}

#[tokio::test]
async fn real_jobs_build_graph_then_search_and_symbol_pages() {
    let hub = Hub::new();
    write_sources(&hub.root());
    let c = hub.client().await;

    let job = c.post_json("/api/jobs", r#"{"kind":"graph_rebuild","confirm":true}"#).await;
    let id = job["id"].as_str().unwrap();
    let done = c.poll(&format!("/api/jobs/{}", id), |v| !["queued", "running"].contains(&v["state"].as_str().unwrap())).await;
    assert_eq!(done["state"], "succeeded", "{}", done);
    assert!(done["summary"].as_str().unwrap().contains("Rebuilt the code graph"));

    let wiki = c.post_json("/api/jobs", r#"{"kind":"wiki_refresh"}"#).await;
    let done = c.poll(&format!("/api/jobs/{}", wiki["id"].as_str().unwrap()), |v| !["queued", "running"].contains(&v["state"].as_str().unwrap())).await;
    assert_eq!(done["state"], "succeeded", "{}", done);

    let drift = c.post_json("/api/jobs", r#"{"kind":"drift_check"}"#).await;
    let done = c.poll(&format!("/api/jobs/{}", drift["id"].as_str().unwrap()), |v| v["state"] == "succeeded").await;
    assert!(done["result"]["score"].is_number());

    // Hybrid search: full-text + Cozo vectors; reports the embedding backend.
    let s = c.get_json("/api/search/full?q=total_sum").await;
    assert_eq!(s["backend"]["embedding"]["backend"], "hashed");
    let items = s["items"].as_array().unwrap();
    let hit = items.iter().find(|i| i["kind"] == "code" && i["title"] == "total_sum").unwrap_or_else(|| panic!("{}", s));
    assert!(hit["sources"].as_array().unwrap().contains(&j!("fts")));
    let symbol_id = hit["id"].as_str().unwrap().to_string();
    assert!(s["facets"]["codeKinds"].is_object());

    let fts = c.get_json("/api/search/full?q=total_sum&mode=fts&scope=code&limit=1").await;
    assert_eq!(fts["backend"]["vector"], "skipped");
    assert_eq!(fts["items"].as_array().unwrap().len(), 1);
    let wiki_only = c.get_json("/api/search/full?q=total_sum&scope=wiki&mode=fts").await;
    assert!(wiki_only["items"].as_array().unwrap().iter().all(|i| i["kind"] == "wiki"));
    let filtered = c.get_json("/api/search/full?q=total_sum&scope=code&kind=struct").await;
    assert!(filtered["items"].as_array().unwrap().iter().all(|i| i["nodeKind"] == "struct"));
    let empty = c.get_json("/api/search/full?q=").await;
    assert_eq!(empty["total"], 0);

    // Symbol workspace.
    let enc = |s: &str| s.replace(':', "%3A");
    let sym = c.get_json(&format!("/api/code/symbol?id={}", enc(&symbol_id))).await;
    assert_eq!(sym["node"]["name"], "total_sum");
    assert_eq!(sym["source"]["available"], true);
    assert!(sym["source"]["lines"][0]["text"].as_str().unwrap().contains("pub fn total_sum"));
    assert_eq!(sym["callees"]["total"], 1);

    let page = c.get_json(&format!("/api/code/symbol/source?id={}&from={}&limit=2", enc(&symbol_id), sym["node"]["startLine"])).await;
    assert_eq!(page["lines"].as_array().unwrap().len(), 2);
    assert!(page["nextLine"].is_number());

    let callees = c.get_json(&format!("/api/code/symbol/callees?id={}", enc(&symbol_id))).await;
    let add_id = callees["items"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(callees["items"][0]["name"], "add_numbers");
    let callers = c.get_json(&format!("/api/code/symbol/callers?id={}", enc(&add_id))).await;
    assert_eq!(callers["total"], 1);
    assert_eq!(callers["items"][0]["name"], "total_sum");
    let impact = c.get_json(&format!("/api/code/symbol/impact?id={}", enc(&add_id))).await;
    assert!(impact["items"].as_array().unwrap().iter().any(|i| i["name"] == "total_sum"), "{}", impact);

    assert_eq!(c.get("/api/code/symbol?id=nope").await.0, StatusCode::NOT_FOUND);

    // Health reflects the fresh graph and recorded snapshot.
    let h = c.get_json("/api/health").await;
    let graph = h["services"].as_array().unwrap().iter().find(|s| s["id"] == "graph").unwrap();
    assert_eq!(graph["graph"]["status"], "fresh");
    assert!(graph["graph"]["parse_health"]["total"].as_u64().unwrap() >= 1);
    let wiki = h["services"].as_array().unwrap().iter().find(|s| s["id"] == "wiki").unwrap();
    assert!(wiki["wiki"]["entities"].as_u64().unwrap() > 0);

    // A changed source file shows up as a delta with its path.
    fs::write(hub.root().join("src/extra.rs"), "pub fn extra() {}\n").unwrap();
    let h = c.get_json("/api/health").await;
    let graph = h["services"].as_array().unwrap().iter().find(|s| s["id"] == "graph").unwrap();
    assert_eq!(graph["graph"]["status"], "stale");
    assert_eq!(graph["recommendedJob"], "graph_refresh");
    assert!(graph["graph"]["changes"]["added"].as_array().unwrap().iter().any(|p| p == "src/extra.rs"));
}

#[tokio::test]
async fn team_mutations_preview_apply_and_refuse_stale_envelopes() {
    let hub = Hub::new();
    let c = hub.client().await;

    // Unsupported or malformed actions are refused before planning.
    assert_eq!(c.post("/api/team/operations/preview", r#"{"action":{"kind":"activity.record"}}"#).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(c.post("/api/team/operations/preview", r#"{"action":{"kind":"member.add","bogus":1}}"#).await.0, StatusCode::BAD_REQUEST);

    // Preview writes nothing.
    let env = c.post_json("/api/team/operations/preview", &j!({ "action": { "kind": "member.add", "member": { "id": "ada", "displayName": "Ada" } } }).to_string()).await;
    let changes = env["envelope"]["preview"]["changes"].as_array().unwrap();
    assert!(changes.iter().any(|ch| ch["path"] == "team/members/ada.json" && ch["kind"] == "create"));
    assert!(!hub.config.members_dir().join("ada.json").exists());
    let applied = c.post_json("/api/team/operations/apply", &j!({ "envelope": env["envelope"] }).to_string()).await;
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["idempotentReplay"], false);
    assert!(hub.config.members_dir().join("ada.json").exists());

    // Replaying the same envelope is idempotent.
    let again = c.post_json("/api/team/operations/apply", &j!({ "envelope": env["envelope"] }).to_string()).await;
    assert_eq!(again["idempotentReplay"], true);

    c.run_action(j!({ "kind": "member.add", "member": { "id": "bob", "displayName": "Bob" } })).await;
    c.run_action(j!({ "kind": "member.select", "memberId": "ada" })).await;
    let actor = c.get_json("/api/actor").await;
    assert_eq!(actor["actorId"], "ada");
    assert_eq!(actor["members"].as_array().unwrap().len(), 2);

    // Stale envelope: the member changes between preview and apply.
    let stale = c.post_json("/api/team/operations/preview", &j!({ "action": { "kind": "member.update", "memberId": "bob", "patch": { "displayName": "Robert" } } }).to_string()).await;
    c.run_action(j!({ "kind": "member.update", "memberId": "bob", "patch": { "role": "lead" } })).await;
    let (status, body) = c.post("/api/team/operations/apply", &j!({ "envelope": stale["envelope"] }).to_string()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", body);
    assert_eq!(json(&body)["code"], "REVISION_CONFLICT");
    let bob: Value = serde_json::from_str(&fs::read_to_string(hub.config.members_dir().join("bob.json")).unwrap()).unwrap();
    assert_eq!(bob["displayName"], "Bob");

    // A tampered envelope fails signature verification.
    let mut tampered = c.post_json("/api/team/operations/preview", &j!({ "action": { "kind": "member.deactivate", "memberId": "bob" } }).to_string()).await;
    tampered["envelope"]["preview"]["summary"] = j!("something else");
    let (status, body) = c.post("/api/team/operations/apply", &j!({ "envelope": tampered["envelope"] }).to_string()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{}", body);

    // Deactivate / reactivate / clear.
    c.run_action(j!({ "kind": "member.deactivate", "memberId": "bob" })).await;
    assert_eq!(c.get_json("/api/team/members?active=true").await["items"].as_array().unwrap().len(), 1);
    c.run_action(j!({ "kind": "member.reactivate", "memberId": "bob" })).await;
    assert_eq!(c.get_json("/api/team/members?active=true").await["total"], 2);
    let detail = c.get_json("/api/team/members/bob").await;
    assert_eq!(detail["member"]["role"], "lead");

    // Inbox: author a typed knowledge draft, publish, then a teammate approves.
    let saved = c.run_action(j!({ "kind": "inbox.draft.save", "draft": {
        "change": { "kind": "knowledge.create", "entityKind": "decision", "title": "Use exponential backoff", "body": "Retries back off exponentially." },
        "rationale": "Agreed in review", "evidence": [{ "kind": "manual", "note": "design review" }],
    } })).await;
    let draft_id = saved["result"]["id"].as_str().unwrap().to_string();
    assert_eq!(c.get_json("/api/inbox/drafts").await["total"], 1);
    assert_eq!(c.get_json(&format!("/api/inbox/drafts/{}", draft_id)).await["reason"], "Agreed in review");
    let published = c.run_action(j!({ "kind": "inbox.publish", "draftId": draft_id })).await;
    let prop_id = published["result"]["id"].as_str().unwrap().to_string();
    // The author cannot reject their own proposal.
    let (status, _) = c.post("/api/team/operations/preview", &j!({ "action": { "kind": "inbox.reject", "proposalId": prop_id } }).to_string()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    c.run_action(j!({ "kind": "member.select", "memberId": "bob" })).await;
    let approved = c.run_action(j!({ "kind": "inbox.approve", "proposalId": prop_id, "rationale": "lgtm" })).await;
    assert_eq!(approved["result"]["status"], "approved");
    assert_eq!(c.get_json("/api/inbox/proposals?state=approved").await["total"], 1);

    // Relays: ada sends to bob; bob acknowledges and closes.
    c.run_action(j!({ "kind": "member.select", "memberId": "ada" })).await;
    let rd = c.run_action(j!({ "kind": "relay.draft.save", "draft": {
        "title": "Backoff handoff", "summary": "Retries done; staging next", "audience": "members", "recipients": ["bob"],
        "nextActions": ["staging test"], "code": [{ "kind": "file", "path": "src/lib.rs" }],
    } })).await;
    let rel = c.run_action(j!({ "kind": "relay.publish", "draftId": rd["result"]["id"] })).await;
    let relay_id = rel["result"]["id"].as_str().unwrap().to_string();
    c.run_action(j!({ "kind": "member.select", "memberId": "bob" })).await;
    let mine = c.get_json("/api/relays/page?perspective=mine").await;
    assert_eq!(mine["total"], 1);
    assert_eq!(c.run_action(j!({ "kind": "relay.acknowledge", "relayId": relay_id })).await["result"]["status"], "acknowledged");
    let closed = c.run_action(j!({ "kind": "relay.close", "relayId": relay_id })).await;
    assert_eq!(closed["result"]["status"], "closed");

    // Workstreams: create and update.
    let ws = c.run_action(j!({ "kind": "workstream.create", "workstream": { "title": "Retries", "state": "active", "owners": ["bob"] } })).await;
    let ws_id = ws["result"]["id"].as_str().unwrap().to_string();
    c.run_action(j!({ "kind": "workstream.update", "workstreamId": ws_id, "patch": { "state": "blocked", "blockers": ["staging down"] } })).await;
    let detail = c.get_json(&format!("/api/workstreams/{}", ws_id)).await;
    assert_eq!(detail["workstream"]["status"], "blocked");
    assert_eq!(c.get_json("/api/workstreams?state=blocked").await["total"], 1);
    assert_eq!(c.get("/api/workstreams/missing").await.0, StatusCode::NOT_FOUND);

    // Overview and shell reflect team state.
    let shell = c.get_json("/api/shell").await;
    assert_eq!(shell["actor"]["member"]["id"], "bob");
    assert_eq!(shell["counts"]["members"], 2);
    let home = c.get_json("/api/home").await;
    assert!(home["attention"].is_array());
    assert!(home["readiness"]["graph"]["status"].is_string());
}

#[tokio::test]
async fn settings_logging_and_onboarding() {
    let hub = Hub::new();
    let c = hub.client().await;
    let v = c.get_json("/api/settings/logging").await;
    assert_eq!(v["mode"], "significant");
    assert_eq!(v["source"], "default");
    let v = c.post_json("/api/settings/logging", r#"{"mode":"checkpoints","expectedRevision":"none"}"#).await;
    assert_eq!(v["mode"], "checkpoints");
    // A stale expected revision conflicts.
    let (status, _) = c.post("/api/settings/logging", r#"{"mode":"manual","expectedRevision":"none"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(c.post("/api/settings/logging", r#"{"mode":"loud"}"#).await.0, StatusCode::BAD_REQUEST);

    assert_eq!(c.get_json("/api/settings/onboarding").await["completed"], false);
    assert_eq!(c.post_json("/api/settings/onboarding", r#"{"completed":true}"#).await["completed"], true);
    assert_eq!(c.get_json("/api/settings/onboarding").await["completed"], true);
    c.post_json("/api/settings/onboarding", r#"{"completed":false}"#).await;
    assert_eq!(c.get_json("/api/settings/onboarding").await["completed"], false);
}

#[cfg(unix)]
fn write_script(dir: &std::path::Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    fs::write(&p, body).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
const FAKE_CLAUDE_POPULATES: &str = r#"#!/bin/sh
echo '{"type":"system","subtype":"init"}'
for f in $(grep -rl 'knobyte:populate' .knobyte --include='*.md' --exclude-dir=local); do
  sed -i.bak 's/<!-- knobyte:populate -->/Populated by the fake agent./' "$f" && rm -f "$f.bak"
done
echo '{"type":"assistant","message":{"content":[{"type":"text","text":"Populating the scaffold"},{"type":"tool_use","id":"t1","name":"Edit","input":{"file_path":".knobyte/context/stack.md"}}]}}'
echo '{"type":"result","subtype":"success","is_error":false}'
"#;

#[cfg(unix)]
const FAKE_CLAUDE_SLOW: &str = "#!/bin/sh\necho '{\"type\":\"system\",\"subtype\":\"init\"}'\nsleep 30\n";

#[cfg(unix)]
#[tokio::test]
async fn setup_wizard_stages_population_with_fake_agent_and_reviewed_commit() {
    let hub = Hub::bare();
    let root = hub.root();
    write_sources(&root);
    let fake = hub.dir.path().join("fakebin");
    write_script(&fake, "claude", FAKE_CLAUDE_SLOW);
    let mut opts = hub.options();
    opts.agent_path = Some(format!("{}:/usr/bin:/bin", fake.display()).into());
    let c = Client::login(build_hub_app(hub.config.clone(), opts).router).await;

    // needs_git → git init (confirmed) → needs_setup.
    let st = c.get_json("/api/setup").await;
    assert_eq!(st["stage"], "needs_git");
    assert_eq!(st["hasScaffold"], false);
    let claude = st["tools"].as_array().unwrap().iter().find(|t| t["id"] == "claude").unwrap();
    assert_eq!(claude["cliAvailable"], true);
    assert_eq!(c.post("/api/setup/git-init", "{}").await.0, StatusCode::BAD_REQUEST);
    c.post_json("/api/setup/git-init", r#"{"confirm":true}"#).await;
    git_init(&root);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-qm", "init"]);
    assert_eq!(c.get_json("/api/setup").await["stage"], "needs_setup");

    // Scaffold, anchors and skills (no agent launched here).
    assert_eq!(c.post("/api/setup", r#"{"tools":["vim"]}"#).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, body) = c.post("/api/setup", r#"{"tools":["claude"],"skipGraph":true}"#).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{}", body);
    let run = c.poll("/api/setup/run", |v| v["status"] != "running").await;
    assert_eq!(run["status"], "succeeded", "{}", run);
    let st = c.get_json("/api/setup").await;
    assert_eq!(st["stage"], "needs_population");
    assert_eq!(st["configuredTools"], j!(["claude"]));
    assert!(root.join("CLAUDE.md").exists());

    // Population preview names the exact command and launches nothing.
    let pv = c.post_json("/api/setup/population/preview", "{}").await;
    assert_eq!(pv["tool"], "claude");
    assert!(pv["command"].as_str().unwrap().starts_with("claude -p"));
    assert!(pv["prompt"].as_str().unwrap().len() > 100);
    assert_eq!(c.post("/api/setup/population", r#"{"tool":"claude"}"#).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(c.post("/api/setup/population", r#"{"tool":"codex","confirm":true}"#).await.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(c.post("/api/setup/cancel", "{}").await.0, StatusCode::CONFLICT);

    // A slow agent session is cancelled; its process tree is stopped.
    let run = c.post_json("/api/setup/population", r#"{"tool":"claude","confirm":true}"#).await;
    assert_eq!(run["status"], "running");
    let first_transcript = run["transcriptId"].as_str().unwrap().to_string();
    c.poll(&format!("/api/setup/transcript?run={}", first_transcript), |v| v["entries"].as_array().is_some_and(|e| e.iter().any(|x| x["text"] == "Agent session started"))).await;
    let (status, body) = c.post("/api/setup/population", r#"{"tool":"claude","confirm":true}"#).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", body);
    c.post_json("/api/setup/cancel", "{}").await;
    let run = c.poll("/api/setup/run", |v| v["status"] != "running").await;
    assert_eq!(run["status"], "cancelled", "{}", run);
    assert_eq!(c.get_json("/api/setup").await["stage"], "needs_population");

    // A populating agent: streamed transcript, then needs_finalize.
    write_script(&fake, "claude", FAKE_CLAUDE_POPULATES);
    let run = c.post_json("/api/setup/population", r#"{"tool":"claude","confirm":true}"#).await;
    let tid = run["transcriptId"].as_str().unwrap().to_string();
    assert_ne!(tid, first_transcript);
    let run = c.poll("/api/setup/run", |v| v["status"] != "running").await;
    assert_eq!(run["status"], "succeeded", "{}", run);
    let tx = c.get_json(&format!("/api/setup/transcript?run={}", tid)).await;
    assert_eq!(tx["done"], true);
    let entries = tx["entries"].as_array().unwrap();
    assert!(entries.iter().any(|e| e["kind"] == "assistant" && e["text"] == "Populating the scaffold"));
    assert!(entries.iter().any(|e| e["kind"] == "tool" && e["text"].as_str().unwrap().contains("stack.md")));
    // SSE replay from a cursor, ending with `done`.
    let mut req = c.get_req(&format!("/api/setup/transcript/events?run={}", tid));
    req.headers_mut().insert("last-event-id", "1".parse().unwrap());
    let (status, _, body) = send(&c.router, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("event: entry") && body.contains("event: done"), "{}", body);
    assert!(!body.contains("\nid: 1\n"));
    assert_eq!(c.get("/api/setup/transcript?run=00000000-0000-0000-0000-000000000000").await.0, StatusCode::NOT_FOUND);
    assert_eq!(c.get_json("/api/setup").await["stage"], "needs_finalize");
    // The private prompt file was removed.
    assert_eq!(fs::read_dir(hub.config.local_dir().join("agent-sessions")).map(|r| r.count()).unwrap_or(0), 0);

    // Finalize → needs_commit.
    c.post_json("/api/setup/finalize", "{}").await;
    assert_eq!(c.get_json("/api/setup").await["stage"], "needs_commit");

    // Commit review: per-file diffs; a change after review is refused.
    let review = c.post_json("/api/setup/commit/preview", "{}").await;
    assert_eq!(review["canCommit"], true, "{}", review);
    let files = review["files"].as_array().unwrap();
    assert!(files.iter().any(|f| f["path"] == "CLAUDE.md"));
    assert!(files.iter().all(|f| !f["path"].as_str().unwrap().contains("/local/")));
    let rev = review["revision"].as_str().unwrap().to_string();
    let d = c.post_json("/api/setup/commit/diff", &j!({ "revision": rev, "path": "CLAUDE.md" }).to_string()).await;
    assert!(d["diff"].as_str().unwrap().contains("+"));
    assert_eq!(c.post("/api/setup/commit/diff", &j!({ "revision": rev, "path": "src/lib.rs" }).to_string()).await.0, StatusCode::NOT_FOUND);
    fs::write(root.join("CLAUDE.md"), format!("{}\nedited after review\n", fs::read_to_string(root.join("CLAUDE.md")).unwrap())).unwrap();
    let (status, body) = c.post("/api/setup/commit", &j!({ "revision": rev, "message": "chore: knobyte" }).to_string()).await;
    assert_eq!(status, StatusCode::CONFLICT, "{}", body);

    let review = c.post_json("/api/setup/commit/preview", "{}").await;
    let rev = review["revision"].as_str().unwrap().to_string();
    assert_eq!(c.post("/api/setup/commit", &j!({ "revision": rev, "message": "  " }).to_string()).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    let done = c.post_json("/api/setup/commit", &j!({ "revision": rev, "message": "chore: initialize knobyte" }).to_string()).await;
    assert_eq!(done["commit"].as_str().unwrap().len(), 40);
    let log = std::process::Command::new("git").args(["log", "-1", "--format=%s"]).current_dir(&root).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "chore: initialize knobyte");
    // The reviewed revision cannot be reused.
    assert_eq!(c.post("/api/setup/commit", &j!({ "revision": rev, "message": "again" }).to_string()).await.0, StatusCode::CONFLICT);
    assert_eq!(c.get_json("/api/setup").await["stage"], "ready");
    // Later scaffold edits are ordinary changes, not an unfinished setup.
    fs::write(hub.config.scaffold_root.join("context/later.md"), "# Later\n").unwrap();
    assert_eq!(c.get_json("/api/setup").await["stage"], "ready");
}

#[tokio::test]
async fn embedded_ui_smoke() {
    let hub = Hub::new();
    let r = hub.router();
    let (status, headers, page) = send(&r, raw_get("/")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap().starts_with("text/html"));
    assert!(page.contains("id=\"main\"") && page.contains("skip-link"));
    // Every module referenced by main.js (and transitively) is served.
    let mut queue = vec!["js/main.js".to_string()];
    let mut seen = std::collections::HashSet::new();
    while let Some(path) = queue.pop() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let (status, headers, body) = send(&r, raw_get(&format!("/assets/{}", path))).await;
        assert_eq!(status, StatusCode::OK, "{}", path);
        assert!(headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap().starts_with("text/javascript"));
        assert!(!body.contains(".innerHTML") && !body.contains("eval("), "{} uses unsafe DOM APIs", path);
        assert!(!body.contains("https://cdn"), "{} loads from a CDN", path);
        let dir = std::path::Path::new(&path).parent().unwrap().to_path_buf();
        for line in body.lines() {
            if let Some(rest) = line.trim().strip_prefix("import ").and_then(|l| l.split(" from '").nth(1)) {
                let rel = rest.trim_end_matches("';");
                let joined = dir.join(rel);
                let mut parts: Vec<String> = Vec::new();
                for c in joined.components() {
                    match c {
                        std::path::Component::ParentDir => { parts.pop(); }
                        std::path::Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
                        _ => {}
                    }
                }
                queue.push(parts.join("/"));
            }
        }
    }
    assert!(seen.len() >= 20, "only {} modules reachable", seen.len());
    let (status, headers, css) = send(&r, raw_get("/assets/app.css")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get(header::CONTENT_TYPE).unwrap().to_str().unwrap().starts_with("text/css"));
    assert!(css.contains("prefers-reduced-motion"));
    assert!(!css.contains("@import") && !css.contains("url(http"));
}
