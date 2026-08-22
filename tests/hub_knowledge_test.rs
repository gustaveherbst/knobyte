//! Hub knowledge-page panels (drift old/new, supersession timeline, evidence),
//! the app-wide job lifecycle stream behind cross-tab updates, and setup
//! progress over SSE.

mod hub_support;

use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use futures_util::StreamExt;
use serde_json::Value;
use tower::ServiceExt;

use hub_support::*;
use knobyte::hub::{build_hub_app, Executor, JobContext, JobKind, JobOutput};
use knobyte::wiki::WikiIndex;

fn build_wiki(hub: &Hub) {
    let mut idx = WikiIndex::open_for_rebuild(&hub.config.wiki_db_path()).unwrap();
    idx.rebuild(&hub.config.scaffold_root).unwrap();
}

async fn run_job(c: &Client, body: &str) -> Value {
    let job = c.post_json("/api/jobs", body).await;
    let id = job["id"].as_str().unwrap().to_string();
    let done = c
        .poll(&format!("/api/jobs/{}", id), |v| !["queued", "running"].contains(&v["state"].as_str().unwrap()))
        .await;
    assert_eq!(done["state"], "succeeded", "{}", done);
    done
}

/// An open SSE response read frame by frame.
struct Sse {
    body: axum::body::BodyDataStream,
    buf: String,
}

impl Sse {
    async fn open(router: &Router, req: Request<Body>) -> Sse {
        let resp = router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get(header::CONTENT_TYPE).unwrap().to_str().unwrap().starts_with("text/event-stream"));
        Sse { body: resp.into_body().into_data_stream(), buf: String::new() }
    }

    /// Next `(event, data)` frame (keep-alive comments skipped).
    async fn next(&mut self) -> (String, Value) {
        loop {
            if let Some(i) = self.buf.find("\n\n") {
                let frame: String = self.buf.drain(..i + 2).collect();
                let mut event = String::from("message");
                let mut data = String::new();
                for line in frame.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        event = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.trim_start());
                    }
                }
                if data.is_empty() && event == "message" {
                    continue;
                }
                return (event, serde_json::from_str(&data).unwrap_or(Value::Null));
            }
            let chunk = tokio::time::timeout(Duration::from_secs(60), self.body.next())
                .await
                .expect("SSE frame in time")
                .expect("stream open")
                .unwrap();
            self.buf.push_str(&String::from_utf8_lossy(&chunk));
        }
    }
}

#[tokio::test]
async fn drift_panel_shows_committed_baseline_vs_current_source_with_codes() {
    let hub = Hub::new();
    let root = hub.root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n").unwrap();
    fs::write(root.join("src/lib.rs"), "/// Adds.\npub fn add_numbers(a: i32, b: i32) -> i32 {\n    a + b\n}\n").unwrap();
    let doc = hub.config.scaffold_root.join("context/adder.md");
    fs::write(&doc, "---\nid: kb_adder\ntitle: Adder\ngrounds_to: [function:src/lib.rs:add_numbers]\n---\n# Adder\n").unwrap();
    let c = hub.client().await;

    // Without a wiki index the panels are 404 problems, never empty "no drift".
    let (status, body) = c.get("/api/wiki/entity/drift?id=kb_adder").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{}", body);

    run_job(&c, r#"{"kind":"graph_rebuild","confirm":true}"#).await;
    {
        let conn = rusqlite::Connection::open(hub.config.graph_db_path()).unwrap();
        let n = knobyte::graph::grounding::ground_documents(&conn, &root, &hub.config.scaffold_root).unwrap();
        assert_eq!(n, 1);
    }
    assert!(fs::read_to_string(&doc).unwrap().contains("body_hash"), "baseline committed");
    run_job(&c, r#"{"kind":"wiki_refresh"}"#).await;

    // Fresh: one pane, not drifted.
    let d = c.get_json("/api/wiki/entity/drift?id=kb_adder").await;
    assert_eq!(d["unavailable"], false, "{}", d);
    assert_eq!(d["panes"].as_array().unwrap().len(), 1);
    assert_eq!(d["panes"][0]["drifted"], false, "{}", d);
    assert_eq!(d["drifted"], 0);

    // Change the body: old (baseline) vs new (current) with GROUNDING_DRIFT.
    fs::write(root.join("src/lib.rs"), "/// Adds.\npub fn add_numbers(a: i32, b: i32) -> i32 {\n    a + b + 0\n}\n").unwrap();
    run_job(&c, r#"{"kind":"graph_refresh"}"#).await;
    run_job(&c, r#"{"kind":"wiki_rebuild","confirm":true}"#).await;
    let d = c.get_json("/api/wiki/entity/drift?id=kb_adder").await;
    let p = &d["panes"][0];
    assert_eq!(p["ref"], "function:src/lib.rs:add_numbers");
    assert_eq!(p["drifted"], true, "{}", d);
    assert_eq!(p["resolved"], true);
    assert_eq!(p["symbol"]["name"], "add_numbers");
    assert_eq!(p["baseline"]["committed"], true);
    let old = p["baseline"]["source"].as_str().unwrap();
    let new = p["current"]["source"].as_str().unwrap();
    assert!(old.contains("a + b") && !old.contains("a + b + 0"), "{}", old);
    assert!(new.contains("a + b + 0"), "{}", new);
    assert_ne!(p["baseline"]["bodyHash"], p["current"]["bodyHash"]);
    assert!(p["diff"].as_array().unwrap().iter().any(|l| l["op"] == "+" && l["text"].as_str().unwrap().contains("a + b + 0")));
    assert_eq!(p["health"], "changed", "{}", p);
    let codes: Vec<&str> = d["codes"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(codes.contains(&"GROUNDING_DRIFT"), "{}", d);
    assert_eq!(p["issues"][0]["code"], "GROUNDING_DRIFT");
    assert_eq!(d["actions"]["syncPreview"]["path"], "/api/drift/sync");
    assert_eq!(d["drifted"], 1);

    // The sync preview the panel offers is a dry run and writes nothing.
    let before = fs::read_to_string(&doc).unwrap();
    let pv = c.post_json("/api/drift/sync", r#"{"dryRun":true}"#).await;
    assert_eq!(pv["result"]["dry_run"], true, "{}", pv);
    assert_eq!(fs::read_to_string(&doc).unwrap(), before);

    // The symbol disappears: missing, still drifted, GROUNDING_GONE.
    fs::write(root.join("src/lib.rs"), "pub fn other() {}\n").unwrap();
    run_job(&c, r#"{"kind":"graph_refresh"}"#).await;
    let d = c.get_json("/api/wiki/entity/drift?id=kb_adder").await;
    let p = &d["panes"][0];
    assert_eq!(p["resolved"], false, "{}", d);
    assert_eq!(p["drifted"], true);
    assert!(p["current"]["source"].is_null());
    assert!(p["baseline"]["source"].as_str().unwrap().contains("a + b"));
    assert!(d["codes"].as_array().unwrap().iter().any(|c| c == "GROUNDING_GONE"), "{}", d);

    assert_eq!(c.get("/api/wiki/entity/drift?id=kb_nope").await.0, StatusCode::NOT_FOUND);
    // The panel is wired into the knowledge page.
    let js = c.get("/assets/js/pages/knowledge.js").await.1;
    for needle in ["/api/wiki/entity/drift", "Committed baseline (old)", "Current source (new)", "Run sync preview", "Open symbol", "dryRun: true"] {
        assert!(js.contains(needle), "knowledge.js lacks {}", needle);
    }
}

/// A teammate re-baselined and the pulled Markdown carries a new committed hash, while this
/// checkout's graph still caches the old baseline source: the panel shows the committed hash
/// and says the old source is not available locally instead of passing the stale cache off
/// as the baseline.
#[tokio::test]
async fn drift_panel_prefers_the_committed_baseline_over_a_stale_cache() {
    let hub = Hub::new();
    let root = hub.root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n").unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn add_numbers(a: i32, b: i32) -> i32 {\n    a + b\n}\n").unwrap();
    let doc = hub.config.scaffold_root.join("context/adder.md");
    fs::write(&doc, "---\nid: kb_adder\ntitle: Adder\ngrounds_to: [function:src/lib.rs:add_numbers]\n---\n# Adder\n").unwrap();
    let c = hub.client().await;
    run_job(&c, r#"{"kind":"graph_rebuild","confirm":true}"#).await;
    {
        let conn = rusqlite::Connection::open(hub.config.graph_db_path()).unwrap();
        knobyte::graph::grounding::ground_documents(&conn, &root, &hub.config.scaffold_root).unwrap();
    }
    run_job(&c, r#"{"kind":"wiki_refresh"}"#).await;
    let old_hash = c.get_json("/api/wiki/entity/drift?id=kb_adder").await["panes"][0]["baseline"]["bodyHash"]
        .as_str()
        .unwrap()
        .to_string();

    // The code changes; the teammate's re-baseline lands in the Markdown only.
    fs::write(root.join("src/lib.rs"), "pub fn add_numbers(a: i32, b: i32) -> i32 {\n    b + a\n}\n").unwrap();
    run_job(&c, r#"{"kind":"graph_refresh"}"#).await;
    let new_hash = c.get_json("/api/wiki/entity/drift?id=kb_adder").await["panes"][0]["current"]["bodyHash"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(old_hash, new_hash);
    let text = fs::read_to_string(&doc).unwrap();
    fs::write(&doc, text.replace(&old_hash, &new_hash)).unwrap();

    let d = c.get_json("/api/wiki/entity/drift?id=kb_adder").await;
    let p = &d["panes"][0];
    assert_eq!(p["baseline"]["bodyHash"], new_hash.as_str(), "{}", d);
    assert!(p["baseline"]["source"].is_null(), "stale cached source shown: {}", d);
    assert_eq!(p["baseline"]["sourceAvailable"], false);
    assert!(p["baseline"]["sourceNote"].as_str().unwrap().contains("not available locally"), "{}", d);
    assert!(p["current"]["source"].as_str().unwrap().contains("b + a"));
    assert!(p["diff"].is_null());
    assert_eq!(p["drifted"], false, "{}", d);
}

fn decision(hub: &Hub, id: &str, status: &str, supersedes: Option<&str>, date: &str) {
    let rel = supersedes
        .map(|t| format!("relations:\n  - type: supersedes\n    target: {}\n", t))
        .unwrap_or_default();
    let dir = hub.config.scaffold_root.join("decisions");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(format!("{}.md", id)),
        format!(
            "---\nid: {id}\ntype: decision\ntitle: Decision {id}\nstatus: {status}\n{rel}provenance:\n  created_by: {{ kind: human, id: ada }}\n  created_at: \"{date}\"\n---\n# Decision {id}\n"
        ),
    )
    .unwrap();
}

#[tokio::test]
async fn supersession_timeline_orders_chain_oldest_first_and_reports_cycles() {
    let hub = Hub::new();
    decision(&hub, "kb_d1", "archived", None, "2023-01-01T00:00:00Z");
    decision(&hub, "kb_d2", "deprecated", Some("kb_d1"), "2024-01-01T00:00:00Z");
    decision(&hub, "kb_d3", "promoted", Some("kb_d2"), "2025-01-01T00:00:00Z");
    decision(&hub, "kb_solo", "promoted", None, "2025-01-01T00:00:00Z");
    build_wiki(&hub);
    let c = hub.client().await;

    // From the middle of the chain: both directions are walked.
    let t = c.get_json("/api/wiki/entity/timeline?id=kb_d2").await;
    let ids: Vec<&str> = t["entries"].as_array().unwrap().iter().map(|e| e["entity"]["id"].as_str().unwrap()).collect();
    assert_eq!(ids, vec!["kb_d1", "kb_d2", "kb_d3"], "{}", t);
    let e = &t["entries"];
    assert_eq!(e[0]["lifecycle"], "archived");
    assert_eq!(e[1]["lifecycle"], "deprecated");
    assert_eq!(e[2]["lifecycle"], "promoted");
    assert_eq!(e[0]["supersededBy"], "kb_d2");
    assert_eq!(e[1]["supersedes"], "kb_d1");
    assert_eq!(e[2]["current"], true);
    assert_eq!(e[1]["current"], false);
    assert_eq!(e[1]["origin"], true);
    assert_eq!(e[0]["date"], "2023-01-01T00:00:00Z", "{}", e[0]);
    assert!(t["cycles"].as_array().unwrap().is_empty());
    // From either end the same chain comes back.
    let from_old = c.get_json("/api/wiki/entity/timeline?id=kb_d1").await;
    assert_eq!(from_old["entries"].as_array().unwrap().len(), 3);
    // An entity outside any chain is its own one-entry timeline.
    let solo = c.get_json("/api/wiki/entity/timeline?id=kb_solo").await;
    assert_eq!(solo["entries"].as_array().unwrap().len(), 1);
    assert_eq!(solo["entries"][0]["current"], true);

    // A cycle is reported, never walked forever.
    decision(&hub, "kb_c1", "promoted", Some("kb_c2"), "2025-01-01T00:00:00Z");
    decision(&hub, "kb_c2", "promoted", Some("kb_c1"), "2025-01-01T00:00:00Z");
    build_wiki(&hub);
    let cyc = c.get_json("/api/wiki/entity/timeline?id=kb_c1").await;
    assert!(cyc["entries"].as_array().unwrap().is_empty(), "{}", cyc);
    assert_eq!(cyc["cycles"].as_array().unwrap().len(), 1, "{}", cyc);

    let js = c.get("/assets/js/pages/knowledge.js").await.1;
    assert!(js.contains("/api/wiki/entity/timeline") && js.contains("Supersession timeline"));
}

#[tokio::test]
async fn evidence_panel_lists_sources_provenance_health_and_traceability() {
    let hub = Hub::new();
    let specs = hub.config.scaffold_root.join("specs");
    fs::create_dir_all(&specs).unwrap();
    fs::write(
        specs.join("kb_req.md"),
        "---\nid: kb_req\ntype: requirement\ntitle: Requirement\nstatus: promoted\n\
grounds_to: [function:src/lib.rs:missing_fn]\n\
sources:\n  - type: url\n    ref: https://example.com/design\n    note: Design doc\n  - type: manual\n    note: Interview\n\
provenance:\n  created_by: { kind: human, id: ada }\n  created_at: \"2024-05-01T00:00:00Z\"\n---\n# Requirement\n",
    )
    .unwrap();
    build_wiki(&hub);
    let c = hub.client().await;
    let ev = c.get_json("/api/wiki/entity/evidence?id=kb_req").await;
    assert_eq!(ev["entity"]["id"], "kb_req");
    assert_eq!(ev["sources"].as_array().unwrap().len(), 2, "{}", ev);
    assert_eq!(ev["sources"][0]["type"], "url");
    assert_eq!(ev["sources"][0]["ref"], "https://example.com/design");
    assert_eq!(ev["provenance"]["created_by"]["id"], "ada");
    let g = &ev["groundings"][0];
    assert_eq!(g["ref"], "function:src/lib.rs:missing_fn");
    assert_eq!(g["committedBaseline"], false);
    // No graph: nothing was checked, so overall health is null (not "unverified").
    assert!(ev["health"].is_null(), "{}", ev);
    assert_eq!(ev["traceability"]["origin"]["id"], "kb_req");
    assert!(ev["traceability"]["gaps"].is_array());
    assert_eq!(c.get("/api/wiki/entity/evidence?id=kb_nope").await.0, StatusCode::NOT_FOUND);
    let js = c.get("/assets/js/pages/knowledge.js").await.1;
    assert!(js.contains("/api/wiki/entity/evidence") && js.contains("Traceability") && js.contains("Provenance"));
}

#[tokio::test]
async fn job_lifecycle_stream_reports_start_and_finish_once_per_state() {
    let hub = Hub::new();
    let mut opts = hub.options();
    let mut ex: HashMap<JobKind, Executor> = HashMap::new();
    ex.insert(
        JobKind::DriftCheck,
        Arc::new(|ctx: &JobContext| {
            for i in 0..5 {
                ctx.phase("checking", i, Some(5), None);
                std::thread::sleep(Duration::from_millis(40));
            }
            Ok(JobOutput { summary: "done".into(), result: Value::Null })
        }) as Executor,
    );
    opts.job_executors = ex;
    let app = build_hub_app(hub.config.clone(), opts);
    let security = app.state.security.clone();
    let c = Client::login(app.router).await;

    // Unauthenticated: refused like every API route.
    assert_eq!(send(&c.router, raw_get("/api/jobs/events")).await.0, StatusCode::UNAUTHORIZED);

    let mut sse = Sse::open(&c.router, c.get_req("/api/jobs/events")).await;
    let (ev, hello) = sse.next().await;
    assert_eq!(ev, "jobs");
    assert!(hello["active"].is_null() && hello["states"].is_array());
    assert_eq!(security.open_streams(), 1);

    let job = c.post_json("/api/jobs", r#"{"kind":"drift_check"}"#).await;
    let id = job["id"].as_str().unwrap().to_string();
    let mut seen = Vec::new();
    loop {
        let (ev, data) = sse.next().await;
        assert_eq!(ev, "job", "{}", data);
        assert_eq!(data["id"], id.as_str());
        seen.push(data["state"].as_str().unwrap().to_string());
        if data["state"] == "succeeded" {
            break;
        }
    }
    // One event per state, progress within a state is not repeated.
    let mut dedup = seen.clone();
    dedup.dedup();
    assert_eq!(dedup, seen, "{:?}", seen);
    assert!(seen.contains(&"running".to_string()), "{:?}", seen);
    assert_eq!(seen.last().unwrap(), "succeeded");
    drop(sse);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(security.open_streams(), 0);

    // The shared observer: BroadcastChannel with a storage fallback, app-wide refresh.
    let (status, headers, js) = send(&c.router, raw_get("/assets/js/lifecycle.js")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get(header::CONTENT_SECURITY_POLICY).is_some());
    for needle in ["BroadcastChannel", "localStorage", "/api/jobs/events", "hub:data-refresh", "startPolling"] {
        assert!(js.contains(needle), "lifecycle.js lacks {}", needle);
    }
    let main = send(&c.router, raw_get("/assets/js/main.js")).await.2;
    assert!(main.contains("startJobLifecycle") && main.contains("hub:data-refresh"));
}

#[tokio::test]
async fn setup_progress_streams_over_sse_with_polling_fallback() {
    let hub = Hub::bare();
    git_init(&hub.root());
    let c = hub.client().await;
    assert_eq!(send(&c.router, raw_get("/api/setup/events")).await.0, StatusCode::UNAUTHORIZED);

    let mut sse = Sse::open(&c.router, c.get_req("/api/setup/events")).await;
    let (ev, first) = sse.next().await;
    assert_eq!(ev, "run");
    assert_eq!(first["status"], "idle");

    let (status, body) = c.post("/api/setup", r#"{"tools":["claude"],"skipGraph":true}"#).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{}", body);
    let mut statuses = Vec::new();
    loop {
        let (ev, run) = sse.next().await;
        assert_eq!(ev, "run");
        statuses.push(run["status"].as_str().unwrap().to_string());
        if run["status"] != "running" && run["status"] != "idle" {
            assert_eq!(run["status"], "succeeded", "{}", run);
            break;
        }
    }
    assert!(statuses.contains(&"running".to_string()), "{:?}", statuses);
    // The polling fallback still answers.
    assert_eq!(c.get_json("/api/setup/run").await["status"], "succeeded");

    let js = c.get("/assets/js/pages/setup.js").await.1;
    assert!(js.contains("/api/setup/events") && js.contains("/api/setup/run"), "SSE with polling fallback");
}
