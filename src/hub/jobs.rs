//! Background index jobs for the Hub: graph refresh / rebuild, wiki refresh /
//! rebuild-index, Cozo sync and drift check.
//!
//! One job runs at a time per Hub. Each job moves through phases with bounded
//! progress, can be cancelled (queued jobs immediately, running jobs at the
//! next phase boundary, before anything is published), and is kept in a short
//! history persisted to `.knobyte/local/hub/jobs.json`. Jobs that were active
//! when a Hub stopped are reported as interrupted on the next start.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::{sse::Event, sse::KeepAlive, IntoResponse, Response, Sse},
    Json,
};
use super::problem::Query;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::problem::{ApiResult, Problem};
use super::HubState;
use crate::config::KnobyteConfig;

const HISTORY_LIMIT: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    GraphRefresh,
    GraphRebuild,
    WikiRefresh,
    WikiRebuild,
    CozoSync,
    DriftCheck,
}

impl JobKind {
    pub const ALL: [JobKind; 6] = [
        JobKind::GraphRefresh,
        JobKind::GraphRebuild,
        JobKind::WikiRefresh,
        JobKind::WikiRebuild,
        JobKind::CozoSync,
        JobKind::DriftCheck,
    ];
    pub fn label(&self) -> &'static str {
        match self {
            JobKind::GraphRefresh => "Graph refresh",
            JobKind::GraphRebuild => "Graph rebuild",
            JobKind::WikiRefresh => "Wiki refresh",
            JobKind::WikiRebuild => "Wiki rebuild-index",
            JobKind::CozoSync => "Cozo sync",
            JobKind::DriftCheck => "Drift check",
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            JobKind::GraphRefresh => "graph_refresh",
            JobKind::GraphRebuild => "graph_rebuild",
            JobKind::WikiRefresh => "wiki_refresh",
            JobKind::WikiRebuild => "wiki_rebuild",
            JobKind::CozoSync => "cozo_sync",
            JobKind::DriftCheck => "drift_check",
        }
    }
    /// Destructive kinds need an explicit confirmation from the client.
    pub fn requires_confirmation(&self) -> bool {
        matches!(self, JobKind::GraphRebuild | JobKind::WikiRebuild)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobProgress {
    pub completed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JobProblem {
    pub title: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JobSnapshot {
    pub id: String,
    pub kind: JobKind,
    pub label: String,
    /// `queued` | `running` | `succeeded` | `failed` | `interrupted`
    pub state: String,
    pub phase: String,
    pub phases: Vec<String>,
    pub progress: Option<JobProgress>,
    pub cancel_requested: bool,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// `user_cancelled` | `process_restart`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<JobProblem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    pub revision: u64,
}

impl JobSnapshot {
    pub fn is_terminal(&self) -> bool {
        matches!(self.state.as_str(), "succeeded" | "failed" | "interrupted")
    }
}

/// Outcome of an executor.
pub enum JobFailure {
    Cancelled,
    Failed(String),
}

pub struct JobOutput {
    pub summary: String,
    pub result: Value,
}

/// Handed to an executor: report phases / progress and observe cancellation.
pub struct JobContext {
    pub config: Arc<KnobyteConfig>,
    pub job_id: String,
    cancel: Arc<AtomicBool>,
    manager: Arc<JobManager>,
}

impl JobContext {
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
    /// Phase boundary: fails with [`JobFailure::Cancelled`] once cancel was requested.
    pub fn checkpoint(&self) -> Result<(), JobFailure> {
        if self.cancelled() {
            Err(JobFailure::Cancelled)
        } else {
            Ok(())
        }
    }
    pub fn phase(&self, phase: &str, completed: u64, total: Option<u64>, message: Option<String>) {
        self.manager.update(&self.job_id, |j| {
            j.phase = phase.to_string();
            j.progress = Some(JobProgress { completed, total, message: message.clone() });
        });
    }
}

pub type Executor = Arc<dyn Fn(&JobContext) -> Result<JobOutput, JobFailure> + Send + Sync>;

struct Inner {
    /// Newest first.
    jobs: VecDeque<JobSnapshot>,
    cancel_flags: HashMap<String, Arc<AtomicBool>>,
}

pub struct JobManager {
    inner: Mutex<Inner>,
    tx: broadcast::Sender<JobSnapshot>,
    executors: HashMap<JobKind, Executor>,
    persist_path: Option<PathBuf>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn phases_for(kind: JobKind) -> Vec<String> {
    let p: &[&str] = match kind {
        JobKind::GraphRefresh => &["discover", "refreshing", "sync", "finalizing"],
        JobKind::GraphRebuild => &["discover", "rebuilding", "sync", "finalizing"],
        JobKind::WikiRefresh => &["refreshing", "sync", "finalizing"],
        JobKind::WikiRebuild => &["rebuilding", "sync", "finalizing"],
        JobKind::CozoSync => &["graph", "wiki", "finalizing"],
        JobKind::DriftCheck => &["checking", "finalizing"],
    };
    p.iter().map(|s| s.to_string()).collect()
}

impl JobManager {
    /// Create a manager with the default executors, overridden by `overrides`.
    pub fn new(config: &KnobyteConfig, overrides: HashMap<JobKind, Executor>) -> Arc<Self> {
        let mut executors = default_executors();
        executors.extend(overrides);
        let persist_path = Some(config.local_dir().join("hub").join("jobs.json"));
        let (tx, _) = broadcast::channel(256);
        let mut jobs: VecDeque<JobSnapshot> = VecDeque::new();
        if let Some(p) = &persist_path {
            if let Ok(text) = std::fs::read_to_string(p) {
                if let Ok(list) = serde_json::from_str::<Vec<JobSnapshot>>(&text) {
                    for mut j in list.into_iter().take(HISTORY_LIMIT) {
                        if !j.is_terminal() {
                            j.state = "interrupted".into();
                            j.phase = "interrupted".into();
                            j.interrupted_reason = Some("process_restart".into());
                            j.finished_at = Some(now());
                            j.revision += 1;
                        }
                        jobs.push_back(j);
                    }
                }
            }
        }
        let m = Arc::new(JobManager {
            inner: Mutex::new(Inner { jobs, cancel_flags: HashMap::new() }),
            tx,
            executors,
            persist_path,
        });
        m.persist();
        m
    }

    fn persist(&self) {
        let Some(path) = &self.persist_path else { return };
        let Ok(inner) = self.inner.lock() else { return };
        let list: Vec<&JobSnapshot> = inner.jobs.iter().take(HISTORY_LIMIT).collect();
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                // Only persist inside an existing scaffold.
                if parent.parent().is_none_or(|p| !p.exists()) {
                    return;
                }
                let _ = std::fs::create_dir_all(parent);
            }
        }
        if let Ok(text) = serde_json::to_string_pretty(&list) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<JobSnapshot> {
        self.tx.subscribe()
    }

    pub fn list(&self) -> Vec<JobSnapshot> {
        self.inner.lock().map(|i| i.jobs.iter().cloned().collect()).unwrap_or_default()
    }

    pub fn get(&self, id: &str) -> Option<JobSnapshot> {
        self.inner.lock().ok()?.jobs.iter().find(|j| j.id == id).cloned()
    }

    pub fn active(&self) -> Option<JobSnapshot> {
        self.inner.lock().ok()?.jobs.iter().find(|j| !j.is_terminal()).cloned()
    }

    fn update(&self, id: &str, f: impl FnOnce(&mut JobSnapshot)) -> Option<JobSnapshot> {
        let snap = {
            let mut inner = self.inner.lock().ok()?;
            let j = inner.jobs.iter_mut().find(|j| j.id == id)?;
            f(j);
            j.revision += 1;
            j.clone()
        };
        let _ = self.tx.send(snap.clone());
        if snap.is_terminal() {
            if let Ok(mut inner) = self.inner.lock() {
                inner.cancel_flags.remove(id);
            }
            self.persist();
        }
        Some(snap)
    }

    /// Move a queued job to running; false when it was cancelled meanwhile.
    fn begin(&self, id: &str) -> bool {
        let snap = {
            let Ok(mut inner) = self.inner.lock() else { return false };
            let Some(j) = inner.jobs.iter_mut().find(|j| j.id == id) else { return false };
            if j.state != "queued" {
                return false;
            }
            j.state = "running".into();
            j.phase = j.phases.first().cloned().unwrap_or_else(|| "running".into());
            j.started_at = Some(now());
            j.revision += 1;
            j.clone()
        };
        let _ = self.tx.send(snap);
        true
    }

    /// Queue and start a job. Refuses while another job is active.
    pub fn start(self: &Arc<Self>, config: Arc<KnobyteConfig>, kind: JobKind) -> ApiResult<JobSnapshot> {
        let executor = self
            .executors
            .get(&kind)
            .cloned()
            .ok_or_else(|| Problem::unavailable(format!("{} is unavailable in this Hub", kind.label())))?;
        let cancel = Arc::new(AtomicBool::new(false));
        let snap = {
            let mut inner = self.inner.lock().map_err(|_| Problem::internal("job state poisoned"))?;
            if let Some(active) = inner.jobs.iter().find(|j| !j.is_terminal()) {
                return Err(Problem::job_running(&active.id));
            }
            let snap = JobSnapshot {
                id: format!("job_{}", uuid::Uuid::new_v4().simple()),
                kind,
                label: kind.label().to_string(),
                state: "queued".into(),
                phase: "queued".into(),
                phases: phases_for(kind),
                progress: None,
                cancel_requested: false,
                created_at: now(),
                started_at: None,
                finished_at: None,
                interrupted_reason: None,
                problem: None,
                summary: None,
                result: None,
                revision: 1,
            };
            inner.jobs.push_front(snap.clone());
            while inner.jobs.len() > HISTORY_LIMIT {
                inner.jobs.pop_back();
            }
            inner.cancel_flags.insert(snap.id.clone(), cancel.clone());
            snap
        };
        let _ = self.tx.send(snap.clone());
        self.persist();

        let manager = self.clone();
        let id = snap.id.clone();
        std::thread::Builder::new()
            .name(format!("knobyte-hub-{}", kind.as_str()))
            .spawn(move || {
                // A job cancelled while queued never runs.
                if !manager.begin(&id) {
                    return;
                }
                let ctx = JobContext { config, job_id: id.clone(), cancel, manager: manager.clone() };
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| executor(&ctx)))
                    .unwrap_or_else(|_| Err(JobFailure::Failed("The job panicked".into())));
                // Once cancellation was requested the job ends `interrupted`
                // (user_cancelled), never `succeeded`/`failed` with a dangling request.
                let cancel_won = |j: &mut JobSnapshot| {
                    if !j.cancel_requested {
                        return false;
                    }
                    j.state = "interrupted".into();
                    j.phase = "interrupted".into();
                    j.interrupted_reason = Some("user_cancelled".into());
                    j.finished_at = Some(now());
                    j.summary = Some("Cancelled while finishing; work it completed before the cancellation may already be applied.".into());
                    true
                };
                match outcome {
                    Ok(out) => {
                        manager.update(&id, |j| {
                            if cancel_won(j) {
                                return;
                            }
                            j.state = "succeeded".into();
                            j.phase = "complete".into();
                            j.finished_at = Some(now());
                            j.summary = Some(out.summary.clone());
                            j.result = Some(out.result.clone());
                        });
                    }
                    Err(JobFailure::Cancelled) => {
                        manager.update(&id, |j| {
                            j.state = "interrupted".into();
                            j.phase = "interrupted".into();
                            j.interrupted_reason = Some("user_cancelled".into());
                            j.finished_at = Some(now());
                            j.summary = Some("Cancelled before anything was published.".into());
                        });
                    }
                    Err(JobFailure::Failed(msg)) => {
                        manager.update(&id, |j| {
                            if cancel_won(j) {
                                return;
                            }
                            j.state = "failed".into();
                            j.phase = "failed".into();
                            j.finished_at = Some(now());
                            j.problem = Some(JobProblem { title: format!("{} failed", j.label), detail: msg.clone() });
                        });
                    }
                }
            })
            .map_err(|e| Problem::internal(format!("could not start job: {}", e)))?;
        Ok(snap)
    }

    /// Request cancellation. Queued jobs stop at once; running jobs stop at the
    /// next phase boundary.
    pub fn cancel(&self, id: &str) -> ApiResult<JobSnapshot> {
        let current = self.get(id).ok_or_else(|| Problem::not_found("The requested Hub job does not exist."))?;
        if current.is_terminal() {
            return Ok(current);
        }
        if let Ok(inner) = self.inner.lock() {
            if let Some(flag) = inner.cancel_flags.get(id) {
                flag.store(true, Ordering::SeqCst);
            }
        }
        let snap = self.update(id, |j| {
            // The job may have finished between the check above and this update.
            if j.is_terminal() {
                return;
            }
            j.cancel_requested = true;
            if j.state == "queued" {
                j.state = "interrupted".into();
                j.phase = "interrupted".into();
                j.interrupted_reason = Some("user_cancelled".into());
                j.finished_at = Some(now());
            }
        });
        snap.ok_or_else(|| Problem::not_found("The requested Hub job does not exist."))
    }
}

// ---------------------------------------------------------------------------
// Executors
// ---------------------------------------------------------------------------

fn failed(e: impl std::fmt::Display) -> JobFailure {
    JobFailure::Failed(e.to_string())
}

fn sync_cozo(config: &KnobyteConfig, graph: bool, wiki: bool) -> Result<Value, String> {
    let engine = crate::cozo::CozoEngine::open_configured(config).map_err(|e| e.to_string())?;
    let mut v = json!({});
    if graph && config.graph_db_path().exists() {
        let conn = rusqlite::Connection::open(config.graph_db_path()).map_err(|e| e.to_string())?;
        let (n, e) = engine.sync_from_graph(&conn).map_err(|e| e.to_string())?;
        v["nodes"] = json!(n);
        v["edges"] = json!(e);
    }
    if wiki && config.wiki_db_path().exists() {
        let conn = rusqlite::Connection::open(config.wiki_db_path()).map_err(|e| e.to_string())?;
        v["wikiEntities"] = json!(engine.sync_from_wiki(&conn).map_err(|e| e.to_string())?);
    }
    Ok(v)
}

fn graph_job(ctx: &JobContext, rebuild: bool) -> Result<JobOutput, JobFailure> {
    use crate::graph::{inspect_status, rebuild_graph, refresh_graph, scan_corpus, CorpusPolicy};
    let config = &ctx.config;
    let root = &config.project_root;
    ctx.phase("discover", 0, None, Some("Scanning the repository".into()));
    let policy = CorpusPolicy::for_project(root);
    let scan = scan_corpus(root, &policy).map_err(failed)?;
    let total = scan.files.len() as u64;
    ctx.phase("discover", total, Some(total), Some(format!("{} source files", total)));
    ctx.checkpoint()?;
    let db = config.graph_db_path();
    let (summary, result, published) = if rebuild {
        ctx.phase("rebuilding", 0, Some(total), Some("Parsing and resolving every file".into()));
        let o = rebuild_graph(&db, root, &scan, None).map_err(failed)?;
        (
            format!(
                "Rebuilt the code graph: {} files, {} symbols, {} relationships in {}ms.",
                o.summary.files_indexed, o.summary.nodes_indexed, o.summary.edges_indexed, o.summary.duration_ms
            ),
            serde_json::to_value(&o).unwrap_or(Value::Null),
            true,
        )
    } else {
        ctx.phase("refreshing", 0, Some(total), Some("Re-extracting changed files".into()));
        let o = refresh_graph(&db, root, &scan, None).map_err(failed)?;
        let text = if o.mode == "noop" {
            format!("Code graph already up to date ({} files); nothing to publish.", total)
        } else {
            format!(
                "Refreshed the code graph ({}): {} added, {} modified, {} deleted; {} re-extracted, {} reused.",
                o.mode,
                o.changes.added.len(),
                o.changes.modified.len(),
                o.changes.deleted.len(),
                o.summary.files_extracted,
                o.summary.files_reused
            )
        };
        let published = o.published;
        (text, serde_json::to_value(&o).unwrap_or(Value::Null), published)
    };
    ctx.phase(if rebuild { "rebuilding" } else { "refreshing" }, total, Some(total), None);
    let mut result = result;
    if published {
        super::explore::record_index_snapshot(config);
        ctx.phase("sync", 0, None, Some("Synchronizing the graph to CozoDB".into()));
        match sync_cozo(config, true, false) {
            Ok(v) => result["cozo"] = v,
            Err(e) => result["cozoWarning"] = json!(format!("CozoDB not synchronized: {}", e)),
        }
    }
    ctx.phase("finalizing", 0, None, None);
    let health = inspect_status(&db, root);
    result["status"] = json!(health.status);
    Ok(JobOutput { summary: format!("{} Status: {}.", summary, health.status), result })
}

fn wiki_job(ctx: &JobContext, rebuild: bool) -> Result<JobOutput, JobFailure> {
    let config = &ctx.config;
    ctx.phase(if rebuild { "rebuilding" } else { "refreshing" }, 0, None, Some("Indexing scaffold Markdown".into()));
    // Explicit maintenance job: the only Hub path allowed to reset an older index.
    let mut index = crate::wiki::WikiIndex::open_for_rebuild(&config.wiki_db_path()).map_err(failed)?;
    let (count, stats) = if rebuild {
        (index.rebuild(&config.scaffold_root).map_err(failed)?, None)
    } else {
        let s = index.refresh(&config.scaffold_root).map_err(failed)?;
        (s.entities, Some(s))
    };
    drop(index);
    let mut result = json!({ "indexedEntities": count, "refresh": stats.as_ref().map(|s| json!({
        "filesAdded": s.files_added, "filesUpdated": s.files_updated,
        "filesRemoved": s.files_removed, "filesUnchanged": s.files_unchanged,
    })) });
    ctx.checkpoint()?;
    ctx.phase("sync", 0, None, Some("Synchronizing the wiki to CozoDB".into()));
    match sync_cozo(config, false, true) {
        Ok(v) => result["cozo"] = v,
        Err(e) => result["cozoWarning"] = json!(format!("CozoDB not synchronized: {}", e)),
    }
    ctx.phase("finalizing", 0, None, None);
    Ok(JobOutput {
        summary: format!("{} the wiki index ({} entities).", if rebuild { "Rebuilt" } else { "Refreshed" }, count),
        result,
    })
}

fn cozo_job(ctx: &JobContext) -> Result<JobOutput, JobFailure> {
    let config = &ctx.config;
    ctx.phase("graph", 0, None, Some("Synchronizing graph.db".into()));
    let g = sync_cozo(config, true, false).map_err(failed)?;
    ctx.checkpoint()?;
    ctx.phase("wiki", 0, None, Some("Synchronizing wiki.db".into()));
    let w = sync_cozo(config, false, true).map_err(failed)?;
    ctx.phase("finalizing", 0, None, None);
    let status = crate::cozo::model2vec::embedding_status(&config.embedding);
    Ok(JobOutput {
        summary: format!(
            "Synchronized CozoDB: {} nodes, {} edges, {} wiki entities ({} embeddings).",
            g["nodes"].as_u64().unwrap_or(0),
            g["edges"].as_u64().unwrap_or(0),
            w["wikiEntities"].as_u64().unwrap_or(0),
            status.backend
        ),
        result: json!({ "graph": g, "wiki": w, "embedding": status }),
    })
}

fn drift_job(ctx: &JobContext) -> Result<JobOutput, JobFailure> {
    ctx.phase("checking", 0, None, Some("Checking scaffold claims against code".into()));
    let report = crate::drift::checker::run_drift_check_with(&ctx.config, &crate::drift::checker::DriftCheckOptions::default());
    ctx.phase("finalizing", 0, None, None);
    Ok(JobOutput {
        summary: format!("Drift score {:.1} ({}), {} issue(s).", report.score, report.status, report.issue_count),
        result: json!({ "score": report.score, "status": report.status, "issueCount": report.issue_count, "fileCount": report.file_count }),
    })
}

fn default_executors() -> HashMap<JobKind, Executor> {
    let mut m: HashMap<JobKind, Executor> = HashMap::new();
    m.insert(JobKind::GraphRefresh, Arc::new(|c: &JobContext| graph_job(c, false)));
    m.insert(JobKind::GraphRebuild, Arc::new(|c: &JobContext| graph_job(c, true)));
    m.insert(JobKind::WikiRefresh, Arc::new(|c: &JobContext| wiki_job(c, false)));
    m.insert(JobKind::WikiRebuild, Arc::new(|c: &JobContext| wiki_job(c, true)));
    m.insert(JobKind::CozoSync, Arc::new(cozo_job));
    m.insert(JobKind::DriftCheck, Arc::new(drift_job));
    m
}

// ---------------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct JobListQuery {
    limit: Option<usize>,
}

pub async fn list_jobs(State(state): State<HubState>, Query(q): Query<JobListQuery>) -> Response {
    let limit = q.limit.unwrap_or(25).clamp(1, HISTORY_LIMIT);
    let jobs = state.jobs.list();
    let active = jobs.iter().find(|j| !j.is_terminal()).cloned();
    Json(json!({
        "items": jobs.into_iter().take(limit).collect::<Vec<_>>(),
        "active": active,
        "kinds": JobKind::ALL.iter().map(|k| json!({
            "kind": k, "label": k.label(), "requiresConfirmation": k.requires_confirmation(),
        })).collect::<Vec<_>>(),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartBody {
    kind: JobKind,
    #[serde(default)]
    confirm: bool,
}

pub async fn start_job(State(state): State<HubState>, body: Bytes) -> Response {
    let b: StartBody = match serde_json::from_slice(&body) {
        Ok(b) => b,
        Err(e) => return Problem::bad_request(format!("Invalid job request: {}", e)).into_response(),
    };
    if b.kind.requires_confirmation() && !b.confirm {
        return Problem::bad_request(format!(
            "{} replaces the index from scratch; confirm it explicitly (\"confirm\": true).",
            b.kind.label()
        ))
        .into_response();
    }
    if matches!(b.kind, JobKind::GraphRefresh | JobKind::GraphRebuild | JobKind::DriftCheck | JobKind::WikiRefresh | JobKind::WikiRebuild)
        && !state.config.scaffold_root.exists()
    {
        return Problem::conflict("Set up Knobyte for this project first (Setup page).").into_response();
    }
    match state.jobs.start(state.config.clone(), b.kind) {
        Ok(s) => (StatusCode::ACCEPTED, Json(s)).into_response(),
        Err(p) => p.into_response(),
    }
}

fn valid_job_id(id: &str) -> bool {
    id.len() <= 64 && id.starts_with("job_") && id[4..].chars().all(|c| c.is_ascii_hexdigit())
}

pub async fn get_job(State(state): State<HubState>, AxumPath(id): AxumPath<String>) -> Response {
    if !valid_job_id(&id) {
        return Problem::bad_request("Invalid job id").into_response();
    }
    match state.jobs.get(&id) {
        Some(j) => Json(j).into_response(),
        None => Problem::not_found("The requested Hub job does not exist.").into_response(),
    }
}

pub async fn cancel_job(State(state): State<HubState>, AxumPath(id): AxumPath<String>) -> Response {
    if !valid_job_id(&id) {
        return Problem::bad_request("Invalid job id").into_response();
    }
    match state.jobs.cancel(&id) {
        Ok(j) => Json(j).into_response(),
        Err(p) => p.into_response(),
    }
}

/// `GET /api/jobs/{id}/events`: SSE of job snapshots (`snapshot`, `progress`,
/// `terminal`); the stream ends after the terminal event.
pub async fn job_events(
    State(state): State<HubState>,
    AxumPath(id): AxumPath<String>,
    auth: Option<axum::Extension<super::security::Auth>>,
) -> Response {
    if !valid_job_id(&id) {
        return Problem::bad_request("Invalid job id").into_response();
    }
    let Some(slot) = state.security.acquire_stream() else {
        return super::security::too_many_streams();
    };
    let mut rx = state.jobs.subscribe();
    let Some(first) = state.jobs.get(&id) else {
        return Problem::not_found("The requested Hub job does not exist.").into_response();
    };
    let stream = async_stream::stream! {
        let terminal = first.is_terminal();
        let mut last = first.revision;
        let name = if terminal { "terminal" } else { "snapshot" };
        yield Ok::<Event, Infallible>(Event::default().event(name).id(last.to_string()).json_data(&first).unwrap_or_default());
        if !terminal {
            loop {
                match rx.recv().await {
                    Ok(s) if s.id == id && s.revision > last => {
                        last = s.revision;
                        let done = s.is_terminal();
                        let name = if done { "terminal" } else { "progress" };
                        yield Ok(Event::default().event(name).id(last.to_string()).json_data(&s).unwrap_or_default());
                        if done { break; }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    };
    let stream = super::security::session_bound_stream(stream, state.security.clone(), auth.map(|a| a.0), slot);
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response()
}

/// Most job ids a lifecycle stream remembers (older ones are forgotten).
const LIFECYCLE_MEMORY: usize = 256;

/// `GET /api/jobs/events`: app-wide job lifecycle stream. Starts with a `jobs`
/// event (the active job, if any, and the newest job ids with their states),
/// then one `job` event per state transition of any job (queued, running,
/// succeeded, failed, interrupted); progress within a state is not repeated.
/// Every open Hub tab subscribes so all of them learn when a job starts or
/// finishes; tabs also relay snapshots to each other (BroadcastChannel).
pub async fn lifecycle_events(State(state): State<HubState>, auth: Option<axum::Extension<super::security::Auth>>) -> Response {
    let Some(slot) = state.security.acquire_stream() else {
        return super::security::too_many_streams();
    };
    let mut rx = state.jobs.subscribe();
    let jobs = state.jobs.list();
    let stream = async_stream::stream! {
        let mut known: VecDeque<(String, String)> = jobs.iter().take(LIFECYCLE_MEMORY).map(|j| (j.id.clone(), j.state.clone())).collect();
        let hello = json!({
            "active": jobs.iter().find(|j| !j.is_terminal()),
            "states": jobs.iter().take(LIFECYCLE_MEMORY).map(|j| json!({ "id": j.id, "state": j.state, "revision": j.revision })).collect::<Vec<_>>(),
        });
        yield Ok::<Event, Infallible>(Event::default().event("jobs").data(hello.to_string()));
        loop {
            match rx.recv().await {
                Ok(s) => {
                    if let Some(i) = known.iter().position(|(id, _)| *id == s.id) {
                        if known[i].1 == s.state { continue; }
                        known[i].1 = s.state.clone();
                    } else {
                        known.push_front((s.id.clone(), s.state.clone()));
                        known.truncate(LIFECYCLE_MEMORY);
                    }
                    yield Ok(Event::default().event("job").id(format!("{}:{}", s.id, s.revision)).json_data(&s).unwrap_or_default());
                }
                // Missed transitions: the client re-lists.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    yield Ok(Event::default().event("resync").data("{}"));
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    let stream = super::security::session_bound_stream(stream, state.security.clone(), auth.map(|a| a.0), slot);
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response()
}
