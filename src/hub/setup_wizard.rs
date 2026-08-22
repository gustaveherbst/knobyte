//! Browser setup wizard: readiness stages, scaffold setup with tool
//! selection, agent-driven population with explicit confirmation and a
//! streamed transcript, finalize, and a reviewed, user-confirmed commit.
//!
//! Stages: `needs_git` → `needs_setup` → `needs_population` → `needs_finalize`
//! → `needs_commit` → `ready` (`complete` for agent-memory workspaces).

use std::collections::VecDeque;
use std::convert::Infallible;
use std::ffi::OsString;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{sse::Event, sse::KeepAlive, IntoResponse, Response, Sse},
    Json,
};
use super::problem::Query;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use super::git::{changed_paths, commit_paths, diff_path, has_git, repo_state, ChangedPath};
use super::problem::Problem;
use super::HubState;
use crate::agent::{find_on_path, preview_command, request_cancel, run_agent, AgentEvent, AgentTool, LaunchOptions};
use crate::config::{load_ai_tools, KnobyteConfig, AI_TOOLS};
use crate::setup::flow::{commit_checkpoint_paths, finalize_setup, parse_tool_list, run_setup_flow, SetupFlowOptions, SETUP_AGENT_TIMEOUT};
use crate::setup::prompts::build_population_prompt;
use crate::setup::{detect_project_state, is_scaffold_populated, resolve_setup_mode, unpopulated_files};

const TRANSCRIPT_RETAINED: usize = 2048;
const TRANSCRIPT_BATCH: usize = 64;
const REVIEW_TTL: Duration = Duration::from_secs(15 * 60);
const MAX_FILE_DIFF_CHARS: usize = 131_072;
const MAX_REVIEW_FILES: usize = 200;
pub const DEFAULT_COMMIT_MESSAGE: &str = "chore: initialize Knobyte project memory";

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SetupRun {
    /// `idle` | `running` | `succeeded` | `failed` | `paused` | `cancelled`
    pub status: String,
    /// `setup` | `population` | `finalize` | `commit`
    pub action: Option<String>,
    pub message: String,
    pub error: Option<String>,
    pub selected_tools: Vec<String>,
    pub population_tool: Option<String>,
    pub transcript_id: Option<String>,
    pub anchor_notes: Vec<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub last_commit: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptEntry {
    pub id: u64,
    pub at: String,
    /// `assistant` | `tool` | `notice` | `error`
    pub kind: String,
    pub text: String,
}

#[derive(Default)]
struct Transcript {
    run_id: Option<String>,
    entries: VecDeque<TranscriptEntry>,
    next_id: u64,
    done: bool,
}

struct ReviewedFile {
    change: ChangedPath,
    fingerprint: String,
}

struct CommitReview {
    revision: String,
    created: Instant,
    files: Vec<ReviewedFile>,
}

pub struct SetupService {
    run: Mutex<SetupRun>,
    transcript: Mutex<Transcript>,
    tx: broadcast::Sender<u64>,
    /// Revision of the run state, bumped on every change (drives `/api/setup/events`).
    run_rev: tokio::sync::watch::Sender<u64>,
    review: Mutex<Option<CommitReview>>,
    /// PATH used to find and run agent CLIs (tests point it at fake scripts).
    pub agent_path: Option<OsString>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

impl SetupService {
    pub fn new(agent_path: Option<OsString>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(512);
        Arc::new(SetupService {
            run: Mutex::new(SetupRun { status: "idle".into(), message: "No setup action has run in this Hub session.".into(), ..Default::default() }),
            transcript: Mutex::new(Transcript::default()),
            tx,
            run_rev: tokio::sync::watch::channel(0).0,
            review: Mutex::new(None),
            agent_path,
        })
    }

    pub fn run_snapshot(&self) -> SetupRun {
        self.run.lock().map(|r| r.clone()).unwrap_or_default()
    }

    fn set_run(&self, f: impl FnOnce(&mut SetupRun)) {
        if let Ok(mut r) = self.run.lock() {
            f(&mut r);
        }
        self.bump_run();
    }

    fn bump_run(&self) {
        self.run_rev.send_modify(|v| *v += 1);
    }

    /// Current run-state revision.
    pub fn run_revision(&self) -> u64 {
        *self.run_rev.borrow()
    }

    fn busy(&self) -> bool {
        self.run.lock().map(|r| r.status == "running").unwrap_or(true)
    }

    fn begin(&self, action: &str, message: &str) -> Result<(), Problem> {
        let mut r = self.run.lock().map_err(|_| Problem::internal("setup state poisoned"))?;
        if r.status == "running" {
            return Err(Problem::conflict(format!(
                "A setup {} is already running; wait for it or cancel it.",
                r.action.as_deref().unwrap_or("action")
            )));
        }
        r.status = "running".into();
        r.action = Some(action.into());
        r.message = message.into();
        r.error = None;
        r.started_at = Some(now());
        r.finished_at = None;
        drop(r);
        self.bump_run();
        Ok(())
    }

    fn push(&self, kind: &str, text: String) {
        let id = {
            let Ok(mut t) = self.transcript.lock() else { return };
            t.next_id += 1;
            let id = t.next_id;
            t.entries.push_back(TranscriptEntry { id, at: now(), kind: kind.into(), text: crate::agent::stream::sanitize(&text) });
            while t.entries.len() > TRANSCRIPT_RETAINED {
                t.entries.pop_front();
            }
            id
        };
        let _ = self.tx.send(id);
    }

    fn start_transcript(&self) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        if let Ok(mut t) = self.transcript.lock() {
            *t = Transcript { run_id: Some(id.clone()), entries: VecDeque::new(), next_id: 0, done: false };
        }
        id
    }

    fn finish_transcript(&self) {
        if let Ok(mut t) = self.transcript.lock() {
            t.done = true;
        }
        let _ = self.tx.send(u64::MAX);
    }

    /// Entries after `after` for transcript `run`, plus whether the run ended.
    fn read_transcript(&self, run: &str, after: u64) -> Result<(Vec<TranscriptEntry>, bool, u64), Problem> {
        let t = self.transcript.lock().map_err(|_| Problem::internal("setup state poisoned"))?;
        if t.run_id.as_deref() != Some(run) {
            return Err(Problem::not_found("The requested setup transcript does not exist in this Hub session."));
        }
        let entries: Vec<TranscriptEntry> =
            t.entries.iter().filter(|e| e.id > after).take(TRANSCRIPT_BATCH).cloned().collect();
        let cursor = entries.last().map(|e| e.id).unwrap_or(after);
        let more = t.entries.back().is_some_and(|e| e.id > cursor);
        Ok((entries, t.done && !more, cursor))
    }

    fn launchable(&self, tool: AgentTool) -> bool {
        find_on_path(tool.program(), self.agent_path.as_deref()).is_some()
    }
}

/// Re-read the scaffold config (setup changes it while the Hub runs).
fn fresh_config(config: &KnobyteConfig) -> KnobyteConfig {
    KnobyteConfig::new(config.project_root.clone(), config.scaffold_root.clone())
}

fn has_scaffold(config: &KnobyteConfig) -> bool {
    config.scaffold_root.is_dir() && config.config_file_path().exists()
}

/// Paths the setup commit checkpoint covers.
fn checkpoint_paths(config: &KnobyteConfig) -> Vec<String> {
    let tools = load_ai_tools(&config.scaffold_root).unwrap_or_default();
    commit_checkpoint_paths(config, &tools)
}

/// Uncommitted changes the setup checkpoint would commit (excluding checkout-local state).
fn pending_checkpoint_changes(config: &KnobyteConfig) -> Vec<ChangedPath> {
    let scaffold_rel = config
        .scaffold_root
        .strip_prefix(&config.project_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| ".knobyte".into());
    let local = format!("{}/local/", scaffold_rel);
    changed_paths(&config.project_root, &checkpoint_paths(config))
        .into_iter()
        .filter(|c| !c.path.starts_with(&local) && !c.path.ends_with(".db") && !c.path.contains(".db-"))
        .collect()
}

pub fn setup_status(config: &KnobyteConfig, svc: &SetupService) -> Value {
    let config = fresh_config(config);
    let mode = resolve_setup_mode(&config, None).unwrap_or_else(|_| "code-repo".into());
    let git = has_git(&config.project_root);
    let scaffold = has_scaffold(&config);
    let populated = scaffold && is_scaffold_populated(&config.scaffold_root);
    let graph_ready = config.graph_db_path().exists();
    let wiki_ready = config.wiki_db_path().exists();
    let configured = load_ai_tools(&config.scaffold_root).unwrap_or_default();
    let tools: Vec<Value> = AI_TOOLS
        .iter()
        .map(|t| {
            let agent = AgentTool::parse(t);
            json!({
                "id": t,
                "name": crate::setup::anchor::tool_display_name(t),
                "selected": configured.iter().any(|c| c == t),
                "launchable": agent.is_some(),
                "cliAvailable": agent.map(|a| svc.launchable(a)).unwrap_or(false),
            })
        })
        .collect();
    // Setup is committed once the scaffold config is in HEAD; later team
    // records are ordinary changes, not an unfinished setup.
    let config_rel = config
        .config_file_path()
        .strip_prefix(&config.project_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| ".knobyte/config.json".into());
    let committed = git && super::git::is_committed(&config.project_root, &config_rel);
    let pending = if git && scaffold && mode == "code-repo" && !committed { pending_checkpoint_changes(&config) } else { Vec::new() };
    let stage = if mode == "code-repo" && !git {
        "needs_git"
    } else if !scaffold {
        "needs_setup"
    } else if !populated {
        "needs_population"
    } else if !wiki_ready {
        "needs_finalize"
    } else if mode != "code-repo" {
        "complete"
    } else if !committed && !pending.is_empty() {
        "needs_commit"
    } else {
        "ready"
    };
    let paths = checkpoint_paths(&config);
    json!({
        "mode": mode,
        "projectName": config.project_name(),
        "projectRoot": config.project_root,
        "hasGit": git,
        "hasScaffold": scaffold,
        "populated": populated,
        "unpopulatedFiles": if scaffold { unpopulated_files(&config.scaffold_root) } else { Vec::new() },
        "graphReady": graph_ready,
        "wikiReady": wiki_ready,
        "state": detect_project_state(&config.project_root, &config.scaffold_root).as_str(),
        "stage": stage,
        "ready": matches!(stage, "ready" | "complete"),
        "configuredTools": configured,
        "tools": tools,
        "pendingCommitFiles": pending.len(),
        "commitPaths": paths,
        "commitCommands": [
            "git status --short".to_string(),
            format!("git add -- {}", paths.join(" ")),
            format!("git commit -m \"{}\"", DEFAULT_COMMIT_MESSAGE),
        ],
        "run": svc.run_snapshot(),
    })
}

fn parse_body<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, Problem> {
    let slice: &[u8] = if body.iter().all(|b| b.is_ascii_whitespace()) { b"{}" } else { body };
    serde_json::from_slice(slice).map_err(|e| Problem::bad_request(format!("Invalid request body: {}", e)))
}

async fn blocking<F>(f: F) -> Response
where
    F: FnOnce() -> Response + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|_| Problem::internal("Hub worker failed").into_response())
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

pub async fn get_setup(State(state): State<HubState>) -> Response {
    blocking(move || Json(setup_status(&state.config, &state.setup)).into_response()).await
}

pub async fn get_run(State(state): State<HubState>) -> Response {
    Json(state.setup.run_snapshot()).into_response()
}

/// `GET /api/setup/events`: SSE of the setup run state. Sends a `run` event
/// with the current snapshot at once, then one per change (the event id is the
/// run revision). The page falls back to polling `/api/setup/run` when the
/// stream cannot be opened.
pub async fn run_events(State(state): State<HubState>, auth: Option<axum::Extension<super::security::Auth>>) -> Response {
    let Some(slot) = state.security.acquire_stream() else {
        return super::security::too_many_streams();
    };
    let svc = state.setup.clone();
    let mut rx = svc.run_rev.subscribe();
    let stream = async_stream::stream! {
        loop {
            let rev = *rx.borrow_and_update();
            let run = svc.run_snapshot();
            yield Ok::<Event, Infallible>(Event::default().event("run").id(rev.to_string()).json_data(&run).unwrap_or_default());
            if rx.changed().await.is_err() {
                break;
            }
        }
    };
    let stream = super::security::session_bound_stream(stream, state.security.clone(), auth.map(|a| a.0), slot);
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitInitBody {
    #[serde(default)]
    confirm: bool,
}

/// `POST /api/setup/git-init` (`{"confirm": true}`): `git init` in the project root.
pub async fn git_init(State(state): State<HubState>, body: Bytes) -> Response {
    let b: GitInitBody = match parse_body(&body) {
        Ok(b) => b,
        Err(p) => return p.into_response(),
    };
    if !b.confirm {
        return Problem::bad_request("Confirm the git init explicitly (\"confirm\": true).").into_response();
    }
    blocking(move || {
        let root = &state.config.project_root;
        if has_git(root) {
            return Problem::conflict("This project already has a git repository.").into_response();
        }
        match std::process::Command::new("git").arg("init").current_dir(root).output() {
            Ok(o) if o.status.success() => Json(setup_status(&state.config, &state.setup)).into_response(),
            Ok(o) => Problem::internal(String::from_utf8_lossy(&o.stderr).trim().to_string()).into_response(),
            Err(e) => Problem::internal(format!("git could not start: {}", e)).into_response(),
        }
    })
    .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetupBody {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    tools: Vec<String>,
    #[serde(default, rename = "skipGraph")]
    skip_graph: bool,
}

/// `POST /api/setup`: scaffold, tool anchors, skills, scan and code graph
/// (never launches an agent; population is a separate, confirmed step).
pub async fn start_setup(State(state): State<HubState>, body: Bytes) -> Response {
    let b: SetupBody = match parse_body(&body) {
        Ok(b) => b,
        Err(p) => return p.into_response(),
    };
    let tools = match parse_tool_list(&b.tools.join(",")) {
        Ok(t) => t,
        Err(e) => return Problem::validation(e).into_response(),
    };
    if let Some(m) = &b.mode {
        if let Err(e) = crate::setup::validate_setup_mode(m) {
            return Problem::validation(e).into_response();
        }
    }
    if let Err(p) = state.setup.begin("setup", "Creating the scaffold, tool anchors, skills and code graph...") {
        return p.into_response();
    }
    state.setup.set_run(|r| r.selected_tools = tools.clone());
    let svc = state.setup.clone();
    let config = fresh_config(&state.config);
    std::thread::spawn(move || {
        let opts = SetupFlowOptions {
            mode: b.mode.clone(),
            dry_run: false,
            tools: Some(tools),
            interactive: false,
            launch_agent: false,
            no_agent: true,
            agent: None,
            skip_graph: b.skip_graph,
            commit: false,
            backup_skills: false,
            capture_baselines: false,
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_setup_flow(&config, &opts)))
            .unwrap_or_else(|_| Err("setup panicked".into()));
        svc.set_run(|r| {
            r.finished_at = Some(now());
            match result {
                Ok(res) => {
                    r.status = "succeeded".into();
                    r.anchor_notes = res.anchor_notes;
                    r.message = match res.stage {
                        crate::setup::flow::SetupStage::NeedsPopulation => {
                            "Scaffold ready. Next: populate it with an agent (or paste the prompt).".into()
                        }
                        _ => "Setup finished.".into(),
                    };
                }
                Err(e) => {
                    r.status = "failed".into();
                    r.message = "Setup failed.".into();
                    r.error = Some(e);
                }
            }
        });
    });
    (StatusCode::ACCEPTED, Json(state.setup.run_snapshot())).into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PopulationBody {
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    confirm: bool,
}

fn population_prompt(config: &KnobyteConfig) -> String {
    let mode = resolve_setup_mode(config, None).unwrap_or_else(|_| "code-repo".into());
    let state = detect_project_state(&config.project_root, &config.scaffold_root);
    let brief = if mode != "agent-memory" && state != crate::setup::ProjectState::Fresh {
        serde_json::to_string_pretty(&crate::scanner::scan(&config.project_root)).ok()
    } else {
        None
    };
    build_population_prompt(&mode, state, brief.as_deref())
}

fn choose_tool(config: &KnobyteConfig, svc: &SetupService, requested: Option<&str>) -> Result<AgentTool, Problem> {
    match requested {
        Some(t) => {
            let tool = AgentTool::parse(t).ok_or_else(|| Problem::validation(format!("Unknown agent '{}'. Use claude or codex.", t)))?;
            if !svc.launchable(tool) {
                return Err(Problem::unavailable(format!("{} is not installed on PATH.", tool.display_name())));
            }
            Ok(tool)
        }
        None => {
            let selected = load_ai_tools(&config.scaffold_root).unwrap_or_default();
            selected
                .iter()
                .filter_map(|t| AgentTool::parse(t))
                .chain([AgentTool::Claude, AgentTool::Codex])
                .find(|t| svc.launchable(*t))
                .ok_or_else(|| Problem::unavailable("Neither the Claude Code nor the Codex CLI is installed on PATH. Paste the prompt into your AI tool instead."))
        }
    }
}

fn launch_options(config: &KnobyteConfig, svc: &SetupService) -> LaunchOptions {
    LaunchOptions {
        cwd: config.project_root.clone(),
        private_dir: config.local_dir(),
        timeout: Some(SETUP_AGENT_TIMEOUT),
        path_env: svc.agent_path.clone(),
        allow_non_git: !has_git(&config.project_root),
    }
}

/// `POST /api/setup/population/preview`: exactly what would be launched, plus
/// the prompt for the paste fallback. Launches nothing.
pub async fn population_preview(State(state): State<HubState>, body: Bytes) -> Response {
    let b: PopulationBody = match parse_body(&body) {
        Ok(b) => b,
        Err(p) => return p.into_response(),
    };
    blocking(move || {
        let config = fresh_config(&state.config);
        if !has_scaffold(&config) {
            return Problem::conflict("Create the scaffold first.").into_response();
        }
        let prompt = population_prompt(&config);
        let installed: Vec<&str> =
            [AgentTool::Claude, AgentTool::Codex].into_iter().filter(|t| state.setup.launchable(*t)).map(|t| t.id()).collect();
        let tool = choose_tool(&config, &state.setup, b.tool.as_deref()).ok();
        let opts = launch_options(&config, &state.setup);
        Json(json!({
            "tool": tool.map(|t| t.id()),
            "toolName": tool.map(|t| t.display_name()),
            "command": tool.map(|t| preview_command(t, &opts).display()),
            "cwd": config.project_root,
            "timeoutMinutes": SETUP_AGENT_TIMEOUT.as_secs() / 60,
            "installedAgents": installed,
            "prompt": prompt,
            "promptChars": prompt.chars().count(),
            "unpopulatedFiles": unpopulated_files(&config.scaffold_root),
        }))
        .into_response()
    })
    .await
}

fn describe_event(ev: &AgentEvent) -> (&'static str, String) {
    match ev {
        AgentEvent::Started => ("notice", "Agent session started".into()),
        AgentEvent::Assistant { text } => ("assistant", text.clone()),
        AgentEvent::Tool { kind, detail } => ("tool", format!("[{}] {}", kind.label(), detail)),
        AgentEvent::ToolFailed { detail } => ("error", format!("Tool failed: {}", detail)),
        AgentEvent::Completed => ("notice", "Agent reported completion".into()),
        AgentEvent::Failed { detail } => ("error", format!("Agent failed: {}", detail)),
    }
}

/// `POST /api/setup/population` (`{"tool": "claude", "confirm": true}`):
/// launch the population agent headless; its transcript streams over SSE.
pub async fn start_population(State(state): State<HubState>, body: Bytes) -> Response {
    let b: PopulationBody = match parse_body(&body) {
        Ok(b) => b,
        Err(p) => return p.into_response(),
    };
    if !b.confirm {
        return Problem::bad_request("Launching an agent requires explicit confirmation (\"confirm\": true).").into_response();
    }
    let config = fresh_config(&state.config);
    if !has_scaffold(&config) {
        return Problem::conflict("Create the scaffold first.").into_response();
    }
    let tool = match choose_tool(&config, &state.setup, b.tool.as_deref()) {
        Ok(t) => t,
        Err(p) => return p.into_response(),
    };
    if let Err(p) = state.setup.begin("population", &format!("{} is populating the scaffold...", tool.display_name())) {
        return p.into_response();
    }
    let run_id = state.setup.start_transcript();
    state.setup.set_run(|r| {
        r.population_tool = Some(tool.id().into());
        r.transcript_id = Some(run_id.clone());
    });
    let svc = state.setup.clone();
    std::thread::spawn(move || {
        svc.push("notice", format!("Launching {} in {}", tool.display_name(), config.project_root.display()));
        let prompt = population_prompt(&config);
        let opts = launch_options(&config, &svc);
        let outcome = run_agent(tool, &prompt, &opts, &mut |ev| {
            let (kind, text) = describe_event(ev);
            svc.push(kind, text);
        });
        let populated = is_scaffold_populated(&config.scaffold_root);
        let (status, message, error) = match outcome.failure {
            None if populated => ("succeeded", format!("{} finished; the scaffold is populated. Next: finalize.", tool.display_name()), None),
            None => (
                "paused",
                format!(
                    "{} finished, but these files still need population: {}",
                    tool.display_name(),
                    unpopulated_files(&config.scaffold_root).join(", ")
                ),
                None,
            ),
            Some(crate::agent::LaunchFailure::Cancelled) => ("cancelled", "The agent session was cancelled; its process tree was stopped.".to_string(), None),
            Some(f) => ("failed", "Population did not complete.".to_string(), Some(f.message(tool))),
        };
        svc.push(if error.is_some() { "error" } else { "notice" }, error.clone().unwrap_or_else(|| message.clone()));
        svc.set_run(|r| {
            r.status = status.into();
            r.message = message;
            r.error = error;
            r.finished_at = Some(now());
        });
        svc.finish_transcript();
    });
    (StatusCode::ACCEPTED, Json(state.setup.run_snapshot())).into_response()
}

/// `POST /api/setup/cancel`: stop a running population agent.
pub async fn cancel(State(state): State<HubState>) -> Response {
    let run = state.setup.run_snapshot();
    if run.status != "running" || run.action.as_deref() != Some("population") {
        return Problem::conflict("No agent session is running.").into_response();
    }
    request_cancel();
    Json(json!({ "cancelRequested": true, "run": run })).into_response()
}

/// `POST /api/setup/finalize`: capture grounding baselines and refresh the wiki index.
pub async fn finalize(State(state): State<HubState>) -> Response {
    if let Err(p) = state.setup.begin("finalize", "Capturing grounding baselines and indexing the wiki...") {
        return p.into_response();
    }
    blocking(move || {
        let config = fresh_config(&state.config);
        let result = if !has_scaffold(&config) {
            Err("Create the scaffold first.".to_string())
        } else if !is_scaffold_populated(&config.scaffold_root) {
            Err(format!("These files still need population: {}", unpopulated_files(&config.scaffold_root).join(", ")))
        } else {
            finalize_setup(&config)
        };
        let ok = result.is_ok();
        state.setup.set_run(|r| {
            r.finished_at = Some(now());
            match &result {
                Ok((captured, entities)) => {
                    r.status = "succeeded".into();
                    r.message = format!("Finalized: {} grounding baseline(s), {} wiki entities.", captured, entities);
                }
                Err(e) => {
                    r.status = "failed".into();
                    r.message = "Finalize failed.".into();
                    r.error = Some(e.clone());
                }
            }
        });
        let body = json!({ "run": state.setup.run_snapshot(), "status": setup_status(&state.config, &state.setup) });
        if ok {
            Json(body).into_response()
        } else {
            Problem::conflict(state.setup.run_snapshot().error.unwrap_or_default()).with_extra(body).into_response()
        }
    })
    .await
}

#[derive(Debug, Deserialize)]
pub struct TranscriptQuery {
    run: String,
    #[serde(default)]
    after: Option<u64>,
}

/// `GET /api/setup/transcript?run=&after=`: one bounded batch (polling fallback).
pub async fn transcript(State(state): State<HubState>, Query(q): Query<TranscriptQuery>) -> Response {
    match state.setup.read_transcript(&q.run, q.after.unwrap_or(0)) {
        Ok((entries, done, cursor)) => Json(json!({ "runId": q.run, "entries": entries, "cursor": cursor, "done": done })).into_response(),
        Err(p) => p.into_response(),
    }
}

/// `GET /api/setup/transcript/events?run=`: SSE of transcript entries
/// (resumable with `Last-Event-ID`); ends when the session ends.
pub async fn transcript_events(
    State(state): State<HubState>,
    headers: HeaderMap,
    Query(q): Query<TranscriptQuery>,
    auth: Option<axum::Extension<super::security::Auth>>,
) -> Response {
    let after = match headers.get("last-event-id").and_then(|v| v.to_str().ok()) {
        Some(v) => match v.parse::<u64>() {
            Ok(n) => n,
            Err(_) => return Problem::bad_request("The transcript cursor is invalid.").into_response(),
        },
        None => q.after.unwrap_or(0),
    };
    let svc = state.setup.clone();
    let run = q.run.clone();
    if let Err(p) = svc.read_transcript(&run, after) {
        return p.into_response();
    }
    let Some(slot) = state.security.acquire_stream() else {
        return super::security::too_many_streams();
    };
    let mut rx = svc.tx.subscribe();
    let stream = async_stream::stream! {
        let mut cursor = after;
        while let Ok((entries, done, next)) = svc.read_transcript(&run, cursor) {
            let had = !entries.is_empty();
            for e in entries {
                yield Ok::<Event, Infallible>(Event::default().event("entry").id(e.id.to_string()).json_data(&e).unwrap_or_default());
            }
            cursor = next;
            if done {
                yield Ok(Event::default().event("done").data(json!({ "runId": run }).to_string()));
                break;
            }
            if had { continue; }
            if let Ok(Err(broadcast::error::RecvError::Closed)) = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
                break;
            }
        }
    };
    let stream = super::security::session_bound_stream(stream, state.security.clone(), auth.map(|a| a.0), slot);
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))).into_response()
}

// ---------------------------------------------------------------------------
// Commit review
// ---------------------------------------------------------------------------

fn fingerprint(root: &std::path::Path, c: &ChangedPath) -> String {
    let mut h = Sha256::new();
    h.update(c.status.as_bytes());
    h.update([0]);
    if let Ok(bytes) = std::fs::read(root.join(&c.path)) {
        h.update(&bytes);
    }
    hex::encode(h.finalize())
}

/// `POST /api/setup/commit/preview`: the exact files the checkpoint would
/// commit, with per-file line counts. Diffs are fetched per file.
pub async fn commit_preview(State(state): State<HubState>) -> Response {
    blocking(move || {
        let config = fresh_config(&state.config);
        let root = &config.project_root;
        let repo = repo_state(root);
        let mut blocked: Option<String> = None;
        if !has_git(root) {
            blocked = Some("This project has no git repository.".into());
        } else if !has_scaffold(&config) {
            blocked = Some("Create the scaffold first.".into());
        } else if !is_scaffold_populated(&config.scaffold_root) {
            blocked = Some("Populate the scaffold before committing it.".into());
        }
        let changes = if blocked.is_none() { pending_checkpoint_changes(&config) } else { Vec::new() };
        if blocked.is_none() && changes.is_empty() {
            blocked = Some("Nothing to commit: the setup files are already committed.".into());
        }
        if changes.len() > MAX_REVIEW_FILES {
            blocked = Some(format!("{} files changed; review and commit them with git directly.", changes.len()));
        }
        let files: Vec<ReviewedFile> =
            changes.into_iter().take(MAX_REVIEW_FILES).map(|c| ReviewedFile { fingerprint: fingerprint(root, &c), change: c }).collect();
        let revision = uuid::Uuid::new_v4().to_string();
        let expires = chrono::Utc::now() + chrono::Duration::from_std(REVIEW_TTL).unwrap_or_default();
        let listed: Vec<&ChangedPath> = files.iter().map(|f| &f.change).collect();
        let body = json!({
            "revision": revision,
            "expiresAt": expires.to_rfc3339(),
            "branch": repo.branch,
            "head": repo.head,
            "defaultMessage": DEFAULT_COMMIT_MESSAGE,
            "files": listed,
            "canCommit": blocked.is_none(),
            "blockedReason": blocked,
        });
        if let Ok(mut r) = state.setup.review.lock() {
            *r = Some(CommitReview { revision, created: Instant::now(), files });
        }
        Json(body).into_response()
    })
    .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffBody {
    revision: String,
    path: String,
}

/// `POST /api/setup/commit/diff`: one reviewed file's diff.
pub async fn commit_diff(State(state): State<HubState>, body: Bytes) -> Response {
    let b: DiffBody = match parse_body(&body) {
        Ok(b) => b,
        Err(p) => return p.into_response(),
    };
    blocking(move || {
        let status = {
            let guard = match state.setup.review.lock() {
                Ok(g) => g,
                Err(_) => return Problem::internal("setup state poisoned").into_response(),
            };
            let Some(review) = guard.as_ref().filter(|r| r.revision == b.revision && r.created.elapsed() < REVIEW_TTL) else {
                return Problem::conflict("This review expired or was replaced; review the files again.").into_response();
            };
            match review.files.iter().find(|f| f.change.path == b.path) {
                Some(f) => f.change.status.clone(),
                None => return Problem::not_found("That path is not part of this review.").into_response(),
            }
        };
        let (diff, truncated) = diff_path(&state.config.project_root, &b.path, &status, MAX_FILE_DIFF_CHARS);
        Json(json!({ "revision": b.revision, "path": b.path, "diff": diff, "truncated": truncated })).into_response()
    })
    .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitBody {
    revision: String,
    message: String,
}

/// `POST /api/setup/commit`: commit exactly the reviewed files, refusing if
/// anything changed since the review.
pub async fn commit(State(state): State<HubState>, body: Bytes) -> Response {
    let b: CommitBody = match parse_body(&body) {
        Ok(b) => b,
        Err(p) => return p.into_response(),
    };
    let message = b.message.trim().to_string();
    if message.is_empty() || message.len() > 2000 || message.contains('\0') {
        return Problem::validation("The commit message must be 1-2000 characters.").into_response();
    }
    if state.setup.busy() {
        return Problem::conflict("Wait for the running setup action to finish.").into_response();
    }
    blocking(move || {
        let config = fresh_config(&state.config);
        let root = config.project_root.clone();
        let reviewed: Vec<(String, String)> = {
            let mut guard = match state.setup.review.lock() {
                Ok(g) => g,
                Err(_) => return Problem::internal("setup state poisoned").into_response(),
            };
            let Some(review) = guard.as_ref().filter(|r| r.revision == b.revision && r.created.elapsed() < REVIEW_TTL) else {
                return Problem::conflict("This review expired or was replaced; review the files again.").into_response();
            };
            let current = pending_checkpoint_changes(&config);
            let now_fp: Vec<(String, String)> = current.iter().map(|c| (c.path.clone(), fingerprint(&root, c))).collect();
            let then_fp: Vec<(String, String)> = review.files.iter().map(|f| (f.change.path.clone(), f.fingerprint.clone())).collect();
            if review.files.is_empty() || now_fp != then_fp {
                *guard = None;
                return Problem::conflict("The setup files changed since you reviewed them; review them again.").into_response();
            }
            *guard = None;
            then_fp
        };
        let paths: Vec<String> = reviewed.into_iter().map(|(p, _)| p).collect();
        match commit_paths(&root, &paths, &message) {
            Ok(sha) => {
                state.setup.set_run(|r| {
                    r.action = Some("commit".into());
                    r.status = "succeeded".into();
                    r.message = format!("Committed {} file(s) as {}.", paths.len(), &sha[..sha.len().min(10)]);
                    r.last_commit = Some(sha.clone());
                    r.finished_at = Some(now());
                });
                Json(json!({ "commit": sha, "files": paths, "message": message, "run": state.setup.run_snapshot() })).into_response()
            }
            Err(e) => Problem::new(StatusCode::CONFLICT, "COMMIT_FAILED", "Commit failed", e).into_response(),
        }
    })
    .await
}

// ---------------------------------------------------------------------------
// Settings: logging cadence and onboarding
// ---------------------------------------------------------------------------

fn settings_path(config: &KnobyteConfig) -> std::path::PathBuf {
    config.local_dir().join("hub").join("settings.json")
}

fn read_settings(config: &KnobyteConfig) -> Value {
    std::fs::read_to_string(settings_path(config))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| json!({}))
}

fn write_settings(config: &KnobyteConfig, v: &Value) -> Result<(), Problem> {
    if !config.scaffold_root.is_dir() {
        return Err(Problem::conflict("Set up Knobyte for this project first."));
    }
    let p = settings_path(config);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Problem::internal(e.to_string()))?;
    }
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(v).unwrap_or_default()).map_err(|e| Problem::internal(e.to_string()))?;
    std::fs::rename(&tmp, &p).map_err(|e| Problem::internal(e.to_string()))
}

fn logging_problem(e: crate::agent_logging::LoggingError) -> Problem {
    let status = match e.exit_code() {
        3 => StatusCode::NOT_FOUND,
        4 => StatusCode::CONFLICT,
        2 => StatusCode::BAD_REQUEST,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    Problem::from_status(status, e.detail().to_string())
}

fn logging_json(p: &crate::agent_logging::LoggingPolicy) -> Value {
    json!({ "mode": p.mode, "revision": p.revision, "source": p.source, "modes": crate::agent_logging::LOGGING_MODES })
}

pub async fn get_logging(State(state): State<HubState>) -> Response {
    match crate::agent_logging::read_policy(&state.config) {
        Ok(p) => Json(logging_json(&p)).into_response(),
        Err(e) => logging_problem(e).into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoggingBody {
    mode: String,
    #[serde(default, rename = "expectedRevision")]
    expected_revision: Option<String>,
}

pub async fn set_logging(State(state): State<HubState>, body: Bytes) -> Response {
    let b: LoggingBody = match parse_body(&body) {
        Ok(b) => b,
        Err(p) => return p.into_response(),
    };
    match crate::agent_logging::set_policy(&state.config, &b.mode, b.expected_revision.as_deref()) {
        Ok(p) => Json(logging_json(&p)).into_response(),
        Err(e) => logging_problem(e).into_response(),
    }
}

pub async fn get_onboarding(State(state): State<HubState>) -> Response {
    let s = read_settings(&state.config);
    Json(json!({
        "completed": s.get("onboardingCompleted").and_then(|v| v.as_bool()).unwrap_or(false),
        "completedAt": s.get("onboardingCompletedAt").cloned().unwrap_or(Value::Null),
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OnboardingBody {
    completed: bool,
}

pub async fn set_onboarding(State(state): State<HubState>, body: Bytes) -> Response {
    let b: OnboardingBody = match parse_body(&body) {
        Ok(b) => b,
        Err(p) => return p.into_response(),
    };
    let mut s = read_settings(&state.config);
    s["onboardingCompleted"] = json!(b.completed);
    s["onboardingCompletedAt"] = if b.completed { json!(now()) } else { Value::Null };
    match write_settings(&state.config, &s) {
        Ok(()) => Json(json!({ "completed": b.completed, "completedAt": s["onboardingCompletedAt"] })).into_response(),
        Err(p) => p.into_response(),
    }
}
