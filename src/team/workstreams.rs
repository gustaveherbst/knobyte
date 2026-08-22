use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::KnobyteConfig;
use crate::team::activity::ActivitySubject;
use crate::team::envelope::{paginate, Page, TeamError};
use crate::team::workflow::{parse_action, run_action, ActorChoice, Ctx, Plan};

pub const WORKSTREAM_STATES: &[&str] = &["planned", "active", "blocked", "done", "archived"];
/// Workstream step statuses.
pub const STEP_STATUSES: &[&str] = &["pending", "in_progress", "done", "blocked", "committed"];
/// Upper bound of checkpoints kept per workstream (oldest dropped first).
const MAX_CHECKPOINTS: usize = 200;
const MAX_LIST: usize = 64;
/// Upper bound of dirty paths recorded per checkpoint; the remainder is counted in
/// `dirtyFilesOmitted` so a large working tree cannot bloat the workstream file.
pub const MAX_CHECKPOINT_DIRTY_FILES: usize = 50;

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct WorkstreamStep {
    pub id: String,
    pub title: String,
    pub status: String,
    #[serde(default, rename = "filesTouched")]
    pub files_touched: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct WorkstreamCheckpoint {
    #[serde(rename = "stepId")]
    pub step_id: String,
    pub status: String,
    #[serde(rename = "gitHead")]
    pub git_head: String,
    #[serde(rename = "dirtyFiles")]
    pub dirty_files: Vec<String>,
    /// Dirty paths left out of `dirty_files` once the per-checkpoint bound was reached.
    #[serde(default, rename = "dirtyFilesOmitted", skip_serializing_if = "is_zero")]
    pub dirty_files_omitted: usize,
    pub timestamp: String,
}

/// Team workstream. `status` holds the lifecycle state
/// (`planned`, `active`, `blocked`, `done`, `archived`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Workstream {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default)]
    pub steps: Vec<WorkstreamStep>,
    #[serde(default)]
    pub checkpoints: Vec<WorkstreamCheckpoint>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub goal: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owners: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributors: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
    #[serde(default, rename = "currentState", skip_serializing_if = "String::is_empty")]
    pub current_state: String,
    #[serde(default, rename = "nextMilestone", skip_serializing_if = "String::is_empty")]
    pub next_milestone: String,
    #[serde(default, rename = "createdBy", skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(default, rename = "updatedBy", skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
}

pub fn get_git_state(project_root: &Path) -> (String, Vec<String>) {
    let head = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let dirty_files = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| l.len() > 3)
                .map(|l| l[3..].trim().to_string())
                .collect()
        })
        .unwrap_or_default();

    (head, dirty_files)
}

pub fn list_workstreams(config: &KnobyteConfig) -> Vec<Workstream> {
    let mut list = Vec::new();
    if let Ok(entries) = fs::read_dir(config.workstreams_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(w) = serde_json::from_str::<Workstream>(&content) {
                        list.push(w);
                    }
                }
            }
        }
    }
    list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
    list
}

/// Paged listing filtered by state; archived workstreams are hidden unless
/// requested explicitly (`include_archived` or a `--state archived` filter).
pub fn list_workstreams_page(
    config: &KnobyteConfig,
    states: &[String],
    include_archived: bool,
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<Page<Workstream>, TeamError> {
    for s in states {
        if !WORKSTREAM_STATES.contains(&s.as_str()) {
            return Err(TeamError::usage(format!("--state must be one of: {}", WORKSTREAM_STATES.join(", "))));
        }
    }
    let mut all = list_workstreams(config);
    if !states.is_empty() {
        all.retain(|w| states.contains(&w.status));
    } else if !include_archived {
        all.retain(|w| w.status != "archived");
    }
    paginate(all, |w| w.id.clone(), &format!("states={:?};archived={}", states, include_archived), cursor, limit)
}

pub fn get_workstream(config: &KnobyteConfig, id: &str) -> Option<Workstream> {
    crate::team::validate_entity_id(id).ok()?;
    crate::team::workflow::read_json_file(&config.workstreams_dir().join(format!("{}.json", id)))
}

/// Write a workstream record directly (atomic).
pub fn save_workstream(config: &KnobyteConfig, workstream: &Workstream) -> Result<(), String> {
    crate::team::validate_entity_id(&workstream.id)?;
    crate::team::store::write_json_atomic(&config.workstreams_dir().join(format!("{}.json", workstream.id)), workstream)
}

/// Set a step's status (creating the step if needed) through the workflow
/// engine, as the resolved actor: locked, journaled, and recorded as activity.
pub fn update_workstream_step(
    config: &KnobyteConfig,
    workstream_id: &str,
    step_id: &str,
    status: &str,
    evidence: Option<&str>,
    files_touched: Option<&[String]>,
) -> Result<Workstream, String> {
    let mut action = json!({ "kind": "workstream.step.update", "workstreamId": workstream_id, "stepId": step_id, "status": status });
    if let Some(ev) = evidence {
        action["evidence"] = json!(ev);
    }
    if let Some(files) = files_touched {
        action["filesTouched"] = json!(files);
    }
    Ok(result_ws(run_action(config, action, &ActorChoice::resolved())?)?)
}

/// Workstreams an agent may implicitly target: those in the `active` state.
pub fn active_workstreams(config: &KnobyteConfig) -> Vec<Workstream> {
    list_workstreams(config).into_iter().filter(|w| w.status == "active").collect()
}

// ---------------------------------------------------------------------------
// Workflow planners
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct WorkstreamInput {
    #[serde(default)]
    pub id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub goal: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub owners: Vec<String>,
    #[serde(default)]
    pub contributors: Vec<String>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub code: Vec<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub components: Vec<String>,
    #[serde(default)]
    pub related: Vec<String>,
    #[serde(default, rename = "nextMilestone")]
    pub next_milestone: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkstreamPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owners: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contributors: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topics: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub components: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blockers: Option<Vec<String>>,
    #[serde(default, rename = "currentState", skip_serializing_if = "Option::is_none")]
    pub current_state: Option<String>,
    #[serde(default, rename = "nextMilestone", skip_serializing_if = "Option::is_none")]
    pub next_milestone: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateAction {
    workstream: WorkstreamInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateAction {
    #[serde(rename = "workstreamId")]
    workstream_id: String,
    patch: WorkstreamPatch,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveAction {
    #[serde(rename = "workstreamId")]
    workstream_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepUpdateAction {
    #[serde(rename = "workstreamId")]
    workstream_id: String,
    #[serde(rename = "stepId")]
    step_id: String,
    status: String,
    #[serde(default)]
    evidence: Option<String>,
    #[serde(default, rename = "filesTouched")]
    files_touched: Vec<String>,
}

fn bounded_text(label: &str, v: &str, max: usize) -> Result<String, TeamError> {
    if v.len() > max {
        return Err(TeamError::validation(format!("{} exceeds {} bytes", label, max)));
    }
    Ok(v.trim().to_string())
}

fn bounded_list(label: &str, v: Vec<String>) -> Result<Vec<String>, TeamError> {
    let mut out: Vec<String> = Vec::new();
    for item in v {
        let t = item.trim().to_string();
        if t.is_empty() {
            continue;
        }
        if t.len() > 512 {
            return Err(TeamError::validation(format!("{} entries must be at most 512 bytes", label)));
        }
        if !out.contains(&t) {
            out.push(t);
        }
    }
    if out.len() > MAX_LIST {
        return Err(TeamError::validation(format!("At most {} {} are allowed", MAX_LIST, label)));
    }
    Ok(out)
}

fn slugify(title: &str) -> String {
    let mut s = String::new();
    for c in title.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-').chars().take(48).collect::<String>();
    if s.is_empty() { "workstream".to_string() } else { s }
}

fn ws_rel(ctx: &Ctx, id: &str) -> String {
    ctx.rel(&ctx.config.workstreams_dir().join(format!("{}.json", id)))
}

pub(crate) fn plan(ctx: &mut Ctx, kind: &str, action: &Value) -> Result<Plan, TeamError> {
    let mut plan = Plan::default();
    let mut step_summary: Option<String> = None;
    let (ws, verb) = match kind {
        "workstream.step.update" => {
            let StepUpdateAction { workstream_id, step_id, status, evidence, files_touched } = parse_action(action)?;
            crate::team::validate_entity_id(&workstream_id).map_err(TeamError::usage)?;
            crate::team::validate_entity_id(&step_id).map_err(|e| TeamError::usage(format!("Invalid step id: {}", e)))?;
            if !STEP_STATUSES.contains(&status.as_str()) {
                return Err(TeamError::validation(format!("Step status must be one of: {}", STEP_STATUSES.join(", "))));
            }
            let evidence = match evidence {
                Some(e) => Some(bounded_text("evidence", &e, 4096)?).filter(|e| !e.is_empty()),
                None => None,
            };
            let files = bounded_list("filesTouched", files_touched)?;
            let mut ws = get_workstream(ctx.config, &workstream_id).ok_or_else(|| TeamError::not_found("Workstream", &workstream_id))?;
            if ws.status == "archived" || ws.status == "done" {
                return Err(TeamError::validation(format!(
                    "Workstream '{}' is {}; reopen it before updating its steps",
                    workstream_id, ws.status
                )));
            }
            let now = ctx.now();
            match ws.steps.iter_mut().find(|s| s.id == step_id) {
                Some(step) => {
                    step.status = status.clone();
                    if evidence.is_some() {
                        step.evidence = evidence;
                    }
                    for f in files {
                        if !step.files_touched.contains(&f) {
                            step.files_touched.push(f);
                        }
                    }
                    if step.files_touched.len() > MAX_LIST {
                        return Err(TeamError::validation(format!("At most {} filesTouched are allowed per step", MAX_LIST)));
                    }
                    step.updated_at = now.clone();
                }
                None => {
                    if ws.steps.len() >= MAX_LIST {
                        return Err(TeamError::validation(format!("A workstream has at most {} steps", MAX_LIST)));
                    }
                    ws.steps.push(WorkstreamStep {
                        id: step_id.clone(),
                        title: format!("Step {}", step_id),
                        status: status.clone(),
                        files_touched: files,
                        evidence,
                        updated_at: now.clone(),
                    });
                }
            }
            let (_, mut dirty_files) = get_git_state(&ctx.config.project_root);
            let dirty_files_omitted = dirty_files.len().saturating_sub(MAX_CHECKPOINT_DIRTY_FILES);
            dirty_files.truncate(MAX_CHECKPOINT_DIRTY_FILES);
            ws.checkpoints.push(WorkstreamCheckpoint {
                step_id: step_id.clone(),
                status: status.clone(),
                git_head: ctx.authority.repo_state.head_commit.clone().unwrap_or_else(|| "unknown".to_string()),
                dirty_files,
                dirty_files_omitted,
                timestamp: now.clone(),
            });
            if ws.checkpoints.len() > MAX_CHECKPOINTS {
                let excess = ws.checkpoints.len() - MAX_CHECKPOINTS;
                ws.checkpoints.drain(..excess);
            }
            ws.updated_at = now;
            ws.updated_by = Some(ctx.actor_id());
            step_summary = Some(format!("Set step '{}' to {} in workstream '{}'", step_id, status, ws.title));
            (ws, "Updated")
        }
        "workstream.create" => {
            let CreateAction { workstream: input } = parse_action(action)?;
            let title = bounded_text("title", &input.title, 512)?;
            if title.is_empty() {
                return Err(TeamError::validation("Workstream title must not be empty"));
            }
            let id = match input.id {
                Some(id) => id,
                None => {
                    let base = slugify(&title);
                    let exists = |id: &str| ctx.config.workstreams_dir().join(format!("{}.json", id)).exists();
                    ctx.ids.id("workstream", || {
                        if !exists(&base) {
                            base.clone()
                        } else {
                            format!("{}-{}", base, &uuid::Uuid::new_v4().simple().to_string()[..6])
                        }
                    })?
                }
            };
            crate::team::validate_entity_id(&id).map_err(TeamError::validation)?;
            if ctx.config.workstreams_dir().join(format!("{}.json", id)).exists() {
                return Err(TeamError::conflict(format!("Workstream '{}' already exists", id)));
            }
            let state = input.state.unwrap_or_else(|| "active".to_string());
            if !WORKSTREAM_STATES.contains(&state.as_str()) || state == "archived" {
                return Err(TeamError::validation("A new workstream state must be planned, active, blocked or done"));
            }
            let mut owners = bounded_list("owners", input.owners)?;
            if owners.is_empty() {
                owners.push(ctx.actor_id());
            }
            let ws = Workstream {
                id: id.clone(),
                title,
                description: input.description.filter(|d| !d.trim().is_empty()),
                status: state,
                owner: owners.first().cloned(),
                steps: Vec::new(),
                checkpoints: Vec::new(),
                created_at: ctx.now(),
                updated_at: ctx.now(),
                goal: bounded_text("goal", input.goal.as_deref().unwrap_or(""), 2048)?,
                summary: bounded_text("summary", input.summary.as_deref().unwrap_or(""), 2048)?,
                owners,
                contributors: bounded_list("contributors", input.contributors)?,
                paths: bounded_list("paths", input.paths)?,
                code: bounded_list("code", input.code)?,
                topics: bounded_list("topics", input.topics)?,
                components: bounded_list("components", input.components)?,
                related: bounded_list("related", input.related)?,
                blockers: Vec::new(),
                current_state: "Planned".to_string(),
                next_milestone: bounded_text("nextMilestone", input.next_milestone.as_deref().unwrap_or(""), 2048)?,
                created_by: Some(ctx.actor_id()),
                updated_by: Some(ctx.actor_id()),
            };
            (ws, "Created")
        }
        "workstream.update" | "workstream.archive" => {
            let (id, patch) = if kind == "workstream.update" {
                let UpdateAction { workstream_id, patch } = parse_action(action)?;
                if patch == WorkstreamPatch::default() {
                    return Err(TeamError::validation("A workstream update must change at least one field"));
                }
                if patch.state.as_deref() == Some("archived") {
                    return Err(TeamError::validation("Use `knobyte workstream archive` to archive a workstream"));
                }
                (workstream_id, Some(patch))
            } else {
                let ArchiveAction { workstream_id } = parse_action(action)?;
                (workstream_id, None)
            };
            crate::team::validate_entity_id(&id).map_err(TeamError::usage)?;
            let current = get_workstream(ctx.config, &id).ok_or_else(|| TeamError::not_found("Workstream", &id))?;
            let mut ws = current.clone();
            match patch {
                Some(p) => {
                    if let Some(v) = p.title {
                        ws.title = bounded_text("title", &v, 512)?;
                        if ws.title.is_empty() {
                            return Err(TeamError::validation("Workstream title must not be empty"));
                        }
                    }
                    if let Some(v) = p.description {
                        ws.description = Some(v).filter(|d| !d.trim().is_empty());
                    }
                    if let Some(v) = p.goal {
                        ws.goal = bounded_text("goal", &v, 2048)?;
                    }
                    if let Some(v) = p.summary {
                        ws.summary = bounded_text("summary", &v, 2048)?;
                    }
                    if let Some(v) = p.state {
                        if !WORKSTREAM_STATES.contains(&v.as_str()) {
                            return Err(TeamError::validation(format!("state must be one of: {}", WORKSTREAM_STATES.join(", "))));
                        }
                        ws.status = v;
                    }
                    if let Some(v) = p.owners {
                        ws.owners = bounded_list("owners", v)?;
                        ws.owner = ws.owners.first().cloned();
                    }
                    if let Some(v) = p.contributors {
                        ws.contributors = bounded_list("contributors", v)?;
                    }
                    if let Some(v) = p.paths {
                        ws.paths = bounded_list("paths", v)?;
                    }
                    if let Some(v) = p.code {
                        ws.code = bounded_list("code", v)?;
                    }
                    if let Some(v) = p.topics {
                        ws.topics = bounded_list("topics", v)?;
                    }
                    if let Some(v) = p.components {
                        ws.components = bounded_list("components", v)?;
                    }
                    if let Some(v) = p.related {
                        ws.related = bounded_list("related", v)?;
                    }
                    if let Some(v) = p.blockers {
                        ws.blockers = bounded_list("blockers", v)?;
                    }
                    if let Some(v) = p.current_state {
                        ws.current_state = bounded_text("currentState", &v, 2048)?;
                    }
                    if let Some(v) = p.next_milestone {
                        ws.next_milestone = bounded_text("nextMilestone", &v, 2048)?;
                    }
                    if ws == current {
                        return Err(TeamError::validation("A workstream update must change at least one field"));
                    }
                }
                None => {
                    if ws.status == "archived" {
                        return Err(TeamError::validation(format!("Workstream {} is already archived", id)));
                    }
                    ws.status = "archived".to_string();
                    ws.blockers.clear();
                }
            }
            ws.updated_at = ctx.now();
            ws.updated_by = Some(ctx.actor_id());
            (ws, if kind == "workstream.archive" { "Archived" } else { "Updated" })
        }
        other => return Err(TeamError::usage(format!("Unsupported workstream action '{}'", other))),
    };
    plan.write_json(ws_rel(ctx, &ws.id), "canonical", &ws, format!("{} workstream", verb))?;
    ctx.activity(
        &mut plan,
        None,
        kind,
        "workstream",
        &ws.id,
        &ws.title,
        step_summary.clone().unwrap_or_else(|| format!("{} workstream '{}'", verb, ws.title)),
        None,
        vec![ActivitySubject::entity("workstream", &ws.id, Some(&ws.title))],
        Some(ws.id.clone()),
    )?;
    plan.summary = format!("{} workstream '{}'", verb, ws.id);
    plan.result = json!(ws);
    Ok(plan)
}

fn result_ws(r: crate::team::workflow::ApplyResult) -> Result<Workstream, TeamError> {
    serde_json::from_value(r.result).map_err(|e| TeamError::internal(e.to_string()))
}

/// Create a workstream through the workflow engine.
pub fn create_workstream(config: &KnobyteConfig, input: &WorkstreamInput) -> Result<Workstream, TeamError> {
    result_ws(run_action(config, json!({ "kind": "workstream.create", "workstream": input }), &ActorChoice::resolved())?)
}

/// Patch a workstream through the workflow engine.
pub fn update_workstream(config: &KnobyteConfig, id: &str, patch: &WorkstreamPatch) -> Result<Workstream, TeamError> {
    result_ws(run_action(config, json!({ "kind": "workstream.update", "workstreamId": id, "patch": patch }), &ActorChoice::resolved())?)
}

/// Archive a workstream through the workflow engine.
pub fn archive_workstream(config: &KnobyteConfig, id: &str) -> Result<Workstream, TeamError> {
    result_ws(run_action(config, json!({ "kind": "workstream.archive", "workstreamId": id }), &ActorChoice::resolved())?)
}
