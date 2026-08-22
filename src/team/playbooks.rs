//! Team playbooks: reusable, step-by-step procedures (`.knobyte/playbooks/<id>.json`)
//! and their runs (`.knobyte/playbooks/runs/<run-id>.json`).
//!
//! Every mutation goes through the team workflow engine (signed preview ->
//! apply, team lock, intent/complete journal, activity record):
//!
//! * `playbook.create` / `playbook.update` / `playbook.archive` (active members only),
//! * `playbook.run.start` (snapshots the playbook's steps; optional workstream link),
//! * `playbook.run.complete-step` (exactly one pending step, with evidence),
//! * `playbook.run.abandon` (with a reason).
//!
//! Lifecycles: playbook `draft -> active -> archived` (archived is immutable and
//! only reachable through `playbook.archive`); run `active -> completed`
//! (automatically once every step is complete) or `active -> abandoned`.
//! Completed and abandoned runs, and completed run steps, are immutable.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::KnobyteConfig;
use crate::team::activity::ActivitySubject;
use crate::team::envelope::{paginate, Diagnostic, Page, TeamError};
use crate::team::refs::EvidenceRef;
use crate::team::workflow::{parse_action, read_json_file, Ctx, Plan};

pub const PLAYBOOK_SCHEMA_VERSION: u32 = 1;
pub const PLAYBOOK_STATES: &[&str] = &["draft", "active", "archived"];
pub const RUN_STATES: &[&str] = &["active", "completed", "abandoned"];
pub const RUN_STEP_STATES: &[&str] = &["pending", "completed"];
/// Upper bound of steps per playbook.
pub const MAX_STEPS: usize = 64;
const MAX_LIST: usize = 64;

pub fn playbooks_dir(config: &KnobyteConfig) -> PathBuf {
    config.scaffold_root.join("playbooks")
}

pub fn runs_dir(config: &KnobyteConfig) -> PathBuf {
    playbooks_dir(config).join("runs")
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// One step of a playbook definition.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlaybookStep {
    pub id: String,
    pub title: String,
    /// Instructions for whoever performs the step.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Checks that must pass before the step counts as done.
    #[serde(default, rename = "requiredChecks", skip_serializing_if = "Vec::is_empty")]
    pub required_checks: Vec<String>,
    /// Evidence the step is expected to produce (test output, commit, document, ...).
    #[serde(default, rename = "expectedEvidence", skip_serializing_if = "Vec::is_empty")]
    pub expected_evidence: Vec<String>,
}

/// A reusable team procedure.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Playbook {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    /// When to use the playbook.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub trigger: String,
    /// `draft`, `active` or `archived`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owners: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prerequisites: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<String>,
    pub steps: Vec<PlaybookStep>,
    /// Semantic revision, incremented on every change.
    #[serde(rename = "entityRevision")]
    pub entity_revision: u64,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "createdBy")]
    pub created_by: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(rename = "updatedBy")]
    pub updated_by: String,
    #[serde(default, rename = "archivedAt", skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
    #[serde(default, rename = "archivedBy", skip_serializing_if = "Option::is_none")]
    pub archived_by: Option<String>,
}

/// A step of a run: a snapshot of the definition plus its completion.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct PlaybookRunStep {
    #[serde(rename = "stepId")]
    pub step_id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, rename = "requiredChecks", skip_serializing_if = "Vec::is_empty")]
    pub required_checks: Vec<String>,
    #[serde(default, rename = "expectedEvidence", skip_serializing_if = "Vec::is_empty")]
    pub expected_evidence: Vec<String>,
    /// `pending` or `completed`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default, rename = "completedBy", skip_serializing_if = "Option::is_none")]
    pub completed_by: Option<String>,
    #[serde(default, rename = "completedAt", skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    /// Git HEAD observed when the step was completed.
    #[serde(default, rename = "completedAtCommit", skip_serializing_if = "Option::is_none")]
    pub completed_at_commit: Option<String>,
}

/// One execution of a playbook.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct PlaybookRun {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub id: String,
    #[serde(rename = "playbookId")]
    pub playbook_id: String,
    #[serde(rename = "playbookTitle")]
    pub playbook_title: String,
    /// Playbook `entityRevision` the steps were snapshotted from.
    #[serde(rename = "playbookRevision")]
    pub playbook_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workstream: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// `active`, `completed` or `abandoned`.
    pub state: String,
    pub steps: Vec<PlaybookRunStep>,
    #[serde(rename = "entityRevision")]
    pub entity_revision: u64,
    #[serde(rename = "startedBy")]
    pub started_by: String,
    #[serde(rename = "startedAt")]
    pub started_at: String,
    #[serde(default, rename = "startedAtCommit", skip_serializing_if = "Option::is_none")]
    pub started_at_commit: Option<String>,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(default, rename = "completedAt", skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default, rename = "completedBy", skip_serializing_if = "Option::is_none")]
    pub completed_by: Option<String>,
    #[serde(default, rename = "abandonedAt", skip_serializing_if = "Option::is_none")]
    pub abandoned_at: Option<String>,
    #[serde(default, rename = "abandonedBy", skip_serializing_if = "Option::is_none")]
    pub abandoned_by: Option<String>,
    #[serde(default, rename = "abandonReason", skip_serializing_if = "Option::is_none")]
    pub abandon_reason: Option<String>,
    #[serde(default, rename = "stepsCompleted", skip_serializing_if = "is_zero")]
    pub steps_completed: u64,
}

impl PlaybookRun {
    pub fn pending_steps(&self) -> usize {
        self.steps.iter().filter(|s| s.state != "completed").count()
    }
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

fn read_dir_json<T: for<'de> Deserialize<'de>>(dir: &std::path::Path) -> Vec<T> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(v) = serde_json::from_str::<T>(&content) {
                        out.push(v);
                    }
                }
            }
        }
    }
    out
}

/// Every playbook, most recently updated first.
pub fn list_playbooks(config: &KnobyteConfig) -> Vec<Playbook> {
    let mut list: Vec<Playbook> = read_dir_json(&playbooks_dir(config));
    list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
    list
}

pub fn get_playbook(config: &KnobyteConfig, id: &str) -> Option<Playbook> {
    crate::team::validate_entity_id(id).ok()?;
    read_json_file(&playbooks_dir(config).join(format!("{}.json", id)))
}

fn check_states(states: &[String], allowed: &[&str], flag: &str) -> Result<(), TeamError> {
    for s in states {
        if !allowed.contains(&s.as_str()) {
            return Err(TeamError::usage(format!("{} must be one of: {}", flag, allowed.join(", "))));
        }
    }
    Ok(())
}

/// Paged playbook listing; archived playbooks are hidden unless requested
/// (`include_archived` or an explicit `archived` state filter).
pub fn list_playbooks_page(
    config: &KnobyteConfig,
    states: &[String],
    topic: Option<&str>,
    include_archived: bool,
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<Page<Playbook>, TeamError> {
    check_states(states, PLAYBOOK_STATES, "--state")?;
    let mut all = list_playbooks(config);
    if !states.is_empty() {
        all.retain(|p| states.contains(&p.state));
    } else if !include_archived {
        all.retain(|p| p.state != "archived");
    }
    if let Some(t) = topic.map(str::trim).filter(|t| !t.is_empty()) {
        all.retain(|p| p.topics.iter().any(|x| x.eq_ignore_ascii_case(t)));
    }
    paginate(
        all,
        |p| format!("{}@{}", p.id, p.entity_revision),
        &format!("states={:?};topic={};archived={}", states, topic.unwrap_or(""), include_archived),
        cursor,
        limit,
    )
}

/// Every run, most recently started first.
pub fn list_runs(config: &KnobyteConfig) -> Vec<PlaybookRun> {
    let mut list: Vec<PlaybookRun> = read_dir_json(&runs_dir(config));
    list.sort_by(|a, b| b.started_at.cmp(&a.started_at).then_with(|| b.id.cmp(&a.id)));
    list
}

pub fn get_run(config: &KnobyteConfig, id: &str) -> Option<PlaybookRun> {
    crate::team::validate_entity_id(id).ok()?;
    read_json_file(&runs_dir(config).join(format!("{}.json", id)))
}

/// Filters of [`list_runs_page`].
#[derive(Debug, Clone, Default)]
pub struct RunFilter {
    pub playbook: Option<String>,
    pub workstream: Option<String>,
    pub states: Vec<String>,
}

pub fn list_runs_page(config: &KnobyteConfig, f: &RunFilter, cursor: Option<&str>, limit: Option<usize>) -> Result<Page<PlaybookRun>, TeamError> {
    check_states(&f.states, RUN_STATES, "--state")?;
    let mut all = list_runs(config);
    if let Some(p) = f.playbook.as_deref().filter(|p| !p.is_empty()) {
        all.retain(|r| r.playbook_id == p);
    }
    if let Some(w) = f.workstream.as_deref().filter(|w| !w.is_empty()) {
        all.retain(|r| r.workstream.as_deref() == Some(w));
    }
    if !f.states.is_empty() {
        all.retain(|r| f.states.contains(&r.state));
    }
    paginate(
        all,
        |r| format!("{}@{}", r.id, r.entity_revision),
        &format!("playbook={};workstream={};states={:?}", f.playbook.as_deref().unwrap_or(""), f.workstream.as_deref().unwrap_or(""), f.states),
        cursor,
        limit,
    )
}

/// Playbook detail with its most recent runs.
pub fn playbook_detail(config: &KnobyteConfig, id: &str) -> Result<Value, TeamError> {
    let p = get_playbook(config, id).ok_or_else(|| TeamError::not_found("Playbook", id))?;
    let runs: Vec<Value> = list_runs(config)
        .into_iter()
        .filter(|r| r.playbook_id == p.id)
        .take(25)
        .map(|r| run_summary(&r))
        .collect();
    Ok(json!({ "playbook": p, "runs": runs }))
}

/// Compact run projection used in listings.
pub fn run_summary(r: &PlaybookRun) -> Value {
    json!({
        "id": r.id,
        "playbookId": r.playbook_id,
        "playbookTitle": r.playbook_title,
        "title": r.title,
        "state": r.state,
        "workstream": r.workstream,
        "startedBy": r.started_by,
        "startedAt": r.started_at,
        "updatedAt": r.updated_at,
        "stepsTotal": r.steps.len(),
        "stepsCompleted": r.steps.len() - r.pending_steps(),
    })
}

// ---------------------------------------------------------------------------
// Workflow planners
// ---------------------------------------------------------------------------

/// Caller input for one step.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlaybookStepInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, rename = "requiredChecks", skip_serializing_if = "Vec::is_empty")]
    pub required_checks: Vec<String>,
    #[serde(default, rename = "expectedEvidence", skip_serializing_if = "Vec::is_empty")]
    pub expected_evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PlaybookInput {
    #[serde(default)]
    pub id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub trigger: Option<String>,
    /// `draft` (default) or `active`.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub owners: Vec<String>,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub prerequisites: Vec<String>,
    #[serde(default)]
    pub related: Vec<String>,
    #[serde(default)]
    pub steps: Vec<PlaybookStepInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlaybookPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    /// `draft` or `active` (archive with `playbook.archive`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owners: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topics: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerequisites: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub related: Option<Vec<String>>,
    /// Replaces the whole step list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<PlaybookStepInput>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateAction {
    playbook: PlaybookInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateAction {
    #[serde(rename = "playbookId")]
    playbook_id: String,
    patch: PlaybookPatch,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveAction {
    #[serde(rename = "playbookId")]
    playbook_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunStartAction {
    #[serde(rename = "playbookId")]
    playbook_id: String,
    #[serde(default)]
    workstream: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteStepAction {
    #[serde(rename = "runId")]
    run_id: String,
    /// A step id, or its 1-based number in the run (a string or a JSON integer).
    #[serde(rename = "stepId", deserialize_with = "step_ref")]
    step_id: String,
    #[serde(default)]
    evidence: Vec<EvidenceRef>,
    #[serde(default)]
    note: Option<String>,
}

fn step_ref<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    match Value::deserialize(d)? {
        Value::String(s) => Ok(s),
        Value::Number(n) if n.is_u64() => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!("stepId must be a step id or a 1-based step number, got {}", other))),
    }
}

/// Index of the run step `step_ref` names: an exact step id first, else a 1-based number.
fn resolve_run_step(run: &PlaybookRun, step_ref: &str) -> Result<usize, TeamError> {
    let r = step_ref.trim();
    if let Some(i) = run.steps.iter().position(|s| s.step_id == r) {
        return Ok(i);
    }
    match r.trim_start_matches('#').parse::<usize>() {
        Ok(n) if (1..=run.steps.len()).contains(&n) => Ok(n - 1),
        Ok(n) => Err(TeamError::validation(format!(
            "Playbook run '{}' has {} step{}; step number {} is out of range",
            run.id,
            run.steps.len(),
            if run.steps.len() == 1 { "" } else { "s" },
            n
        ))),
        Err(_) => Err(TeamError::validation(format!(
            "Playbook run '{}' has no step '{}' (use a step id or a 1-based step number)",
            run.id, r
        ))),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AbandonAction {
    #[serde(rename = "runId")]
    run_id: String,
    reason: String,
}

fn bounded_text(label: &str, v: &str, max: usize) -> Result<String, TeamError> {
    if v.len() > max {
        return Err(TeamError::validation(format!("{} exceeds {} bytes", label, max)));
    }
    Ok(v.trim().to_string())
}

fn bounded_list(label: &str, v: Vec<String>, max_len: usize) -> Result<Vec<String>, TeamError> {
    let mut out: Vec<String> = Vec::new();
    for item in v {
        let t = item.trim().to_string();
        if t.is_empty() {
            continue;
        }
        if t.len() > max_len {
            return Err(TeamError::validation(format!("{} entries must be at most {} bytes", label, max_len)));
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

fn slugify(title: &str, fallback: &str) -> String {
    let mut s = String::new();
    for c in title.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-').chars().take(48).collect::<String>();
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { fallback.to_string() } else { s }
}

fn normalize_steps(input: Vec<PlaybookStepInput>) -> Result<Vec<PlaybookStep>, TeamError> {
    if input.len() > MAX_STEPS {
        return Err(TeamError::validation(format!("A playbook has at most {} steps", MAX_STEPS)));
    }
    let mut out: Vec<PlaybookStep> = Vec::new();
    for (i, s) in input.into_iter().enumerate() {
        let title = bounded_text("step title", &s.title, 512)?;
        if title.is_empty() {
            return Err(TeamError::validation(format!("Step {} has an empty title", i + 1)));
        }
        let id = match s.id.map(|x| x.trim().to_string()).filter(|x| !x.is_empty()) {
            Some(id) => {
                crate::team::validate_entity_id(&id).map_err(|e| TeamError::validation(format!("Invalid step id: {}", e)))?;
                id
            }
            None => {
                let base = slugify(&title, &format!("step-{}", i + 1));
                if out.iter().any(|x| x.id == base) { format!("{}-{}", base, i + 1) } else { base }
            }
        };
        if out.iter().any(|x| x.id == id) {
            return Err(TeamError::validation(format!("Duplicate step id '{}'", id)));
        }
        out.push(PlaybookStep {
            id,
            title,
            description: bounded_text("step description", s.description.as_deref().unwrap_or(""), 8192)?,
            required_checks: bounded_list("requiredChecks", s.required_checks, 1024)?,
            expected_evidence: bounded_list("expectedEvidence", s.expected_evidence, 1024)?,
        });
    }
    Ok(out)
}

fn require_known_members(ctx: &Ctx, label: &str, ids: &[String]) -> Result<(), TeamError> {
    for id in ids {
        if crate::team::members::get_member(ctx.config, id).is_none() {
            return Err(TeamError::validation(format!("Unknown {} '{}' (see `knobyte member list`)", label, id)));
        }
    }
    Ok(())
}

fn require_known_actor(ctx: &Ctx, purpose: &str) -> Result<String, TeamError> {
    let id = ctx.actor_id();
    if id == "unknown" {
        return Err(TeamError::unauthorized(format!(
            "Select a member (`knobyte member select <id>`) or configure Git, to {}.",
            purpose
        )));
    }
    if let Some(m) = ctx.authority.actor.member_id() {
        match crate::team::members::get_member(ctx.config, m) {
            Some(member) if member.is_active() => {}
            _ => return Err(TeamError::unauthorized(format!("The current actor '{}' is missing or inactive.", m))),
        }
    }
    Ok(id)
}

fn playbook_rel(ctx: &Ctx, id: &str) -> String {
    ctx.rel(&playbooks_dir(ctx.config).join(format!("{}.json", id)))
}

fn run_rel(ctx: &Ctx, id: &str) -> String {
    ctx.rel(&runs_dir(ctx.config).join(format!("{}.json", id)))
}

pub(crate) fn plan(ctx: &mut Ctx, kind: &str, action: &Value) -> Result<Plan, TeamError> {
    match kind {
        "playbook.create" | "playbook.update" | "playbook.archive" => plan_playbook(ctx, kind, action),
        "playbook.run.start" | "playbook.run.complete-step" | "playbook.run.abandon" => plan_run(ctx, kind, action),
        other => Err(TeamError::usage(format!("Unsupported playbook action '{}'", other))),
    }
}

fn plan_playbook(ctx: &mut Ctx, kind: &str, action: &Value) -> Result<Plan, TeamError> {
    let mut plan = Plan::default();
    let actor = ctx.require_active_member("change playbooks")?;
    let now = ctx.now();
    let (pb, verb) = match kind {
        "playbook.create" => {
            let CreateAction { playbook: input } = parse_action(action)?;
            let title = bounded_text("title", &input.title, 512)?;
            if title.is_empty() {
                return Err(TeamError::validation("Playbook title must not be empty"));
            }
            let id = match input.id.map(|x| x.trim().to_string()).filter(|x| !x.is_empty()) {
                Some(id) => id,
                None => {
                    let base = slugify(&title, "playbook");
                    let dir = playbooks_dir(ctx.config);
                    ctx.ids.id("playbook", || {
                        if !dir.join(format!("{}.json", base)).exists() {
                            base.clone()
                        } else {
                            format!("{}-{}", base, &uuid::Uuid::new_v4().simple().to_string()[..6])
                        }
                    })?
                }
            };
            crate::team::validate_entity_id(&id).map_err(TeamError::validation)?;
            let path = playbooks_dir(ctx.config).join(format!("{}.json", id));
            if path.exists() {
                return Err(TeamError::conflict(format!("Playbook '{}' already exists", id)));
            }
            let state = input.state.unwrap_or_else(|| "draft".to_string());
            if state != "draft" && state != "active" {
                return Err(TeamError::validation("A new playbook state must be draft or active"));
            }
            let steps = normalize_steps(input.steps)?;
            if state == "active" && steps.is_empty() {
                return Err(TeamError::validation("An active playbook needs at least one step"));
            }
            let mut owners = bounded_list("owners", input.owners, 128)?;
            if owners.is_empty() {
                owners.push(actor.clone());
            }
            require_known_members(ctx, "owner", &owners)?;
            let pb = Playbook {
                schema_version: PLAYBOOK_SCHEMA_VERSION,
                id,
                title,
                summary: bounded_text("summary", input.summary.as_deref().unwrap_or(""), 4096)?,
                trigger: bounded_text("trigger", input.trigger.as_deref().unwrap_or(""), 2048)?,
                state,
                owners,
                topics: bounded_list("topics", input.topics, 128)?,
                prerequisites: bounded_list("prerequisites", input.prerequisites, 1024)?,
                related: bounded_list("related", input.related, 512)?,
                steps,
                entity_revision: 1,
                created_at: now.clone(),
                created_by: actor.clone(),
                updated_at: now.clone(),
                updated_by: actor.clone(),
                archived_at: None,
                archived_by: None,
            };
            (pb, "Created")
        }
        _ => {
            let (id, patch) = if kind == "playbook.update" {
                let UpdateAction { playbook_id, patch } = parse_action(action)?;
                if patch == PlaybookPatch::default() {
                    return Err(TeamError::validation("A playbook update must change at least one field"));
                }
                (playbook_id, Some(patch))
            } else {
                let ArchiveAction { playbook_id } = parse_action(action)?;
                (playbook_id, None)
            };
            crate::team::validate_entity_id(&id).map_err(TeamError::usage)?;
            let current = get_playbook(ctx.config, &id).ok_or_else(|| TeamError::not_found("Playbook", &id))?;
            if current.state == "archived" {
                return Err(TeamError::validation(format!("Playbook '{}' is archived; archived playbooks are immutable", id)));
            }
            let mut pb = current.clone();
            match patch {
                Some(p) => {
                    if let Some(v) = p.title {
                        pb.title = bounded_text("title", &v, 512)?;
                        if pb.title.is_empty() {
                            return Err(TeamError::validation("Playbook title must not be empty"));
                        }
                    }
                    if let Some(v) = p.summary {
                        pb.summary = bounded_text("summary", &v, 4096)?;
                    }
                    if let Some(v) = p.trigger {
                        pb.trigger = bounded_text("trigger", &v, 2048)?;
                    }
                    if let Some(v) = p.state {
                        match v.as_str() {
                            "draft" | "active" => pb.state = v,
                            "archived" => return Err(TeamError::validation("Use `knobyte playbook archive` to archive a playbook")),
                            _ => return Err(TeamError::validation("state must be draft or active")),
                        }
                    }
                    if let Some(v) = p.owners {
                        pb.owners = bounded_list("owners", v, 128)?;
                        if pb.owners.is_empty() {
                            return Err(TeamError::validation("A playbook needs at least one owner"));
                        }
                        require_known_members(ctx, "owner", &pb.owners)?;
                    }
                    if let Some(v) = p.topics {
                        pb.topics = bounded_list("topics", v, 128)?;
                    }
                    if let Some(v) = p.prerequisites {
                        pb.prerequisites = bounded_list("prerequisites", v, 1024)?;
                    }
                    if let Some(v) = p.related {
                        pb.related = bounded_list("related", v, 512)?;
                    }
                    if let Some(v) = p.steps {
                        pb.steps = normalize_steps(v)?;
                    }
                    if pb.state == "active" && pb.steps.is_empty() {
                        return Err(TeamError::validation("An active playbook needs at least one step"));
                    }
                    if pb == current {
                        return Err(TeamError::validation("A playbook update must change at least one field"));
                    }
                }
                None => {
                    pb.state = "archived".to_string();
                    pb.archived_at = Some(now.clone());
                    pb.archived_by = Some(actor.clone());
                }
            }
            pb.entity_revision = current.entity_revision + 1;
            pb.updated_at = now.clone();
            pb.updated_by = actor.clone();
            let verb = if kind == "playbook.archive" {
                "Archived"
            } else if current.state == "draft" && pb.state == "active" {
                "Published"
            } else {
                "Updated"
            };
            (pb, verb)
        }
    };
    plan.write_json(playbook_rel(ctx, &pb.id), "canonical", &pb, format!("{} playbook", verb))?;
    ctx.activity(
        &mut plan,
        None,
        kind,
        "playbook",
        &pb.id,
        &pb.title,
        format!("{} playbook '{}'", verb, pb.title),
        None,
        vec![ActivitySubject::entity("playbook", &pb.id, Some(&pb.title))],
        None,
    )?;
    plan.summary = format!("{} playbook '{}'", verb, pb.id);
    plan.result = json!(pb);
    Ok(plan)
}

fn plan_run(ctx: &mut Ctx, kind: &str, action: &Value) -> Result<Plan, TeamError> {
    let mut plan = Plan::default();
    let now = ctx.now();
    let head = ctx.authority.repo_state.head_commit.clone();
    let (run, summary, subjects) = match kind {
        "playbook.run.start" => {
            let actor = require_known_actor(ctx, "start a playbook run")?;
            let RunStartAction { playbook_id, workstream, title } = parse_action(action)?;
            crate::team::validate_entity_id(&playbook_id).map_err(TeamError::usage)?;
            let pb = get_playbook(ctx.config, &playbook_id).ok_or_else(|| TeamError::not_found("Playbook", &playbook_id))?;
            if pb.state != "active" {
                return Err(TeamError::validation(format!(
                    "Playbook '{}' is {}; only active playbooks can be run{}",
                    pb.id,
                    pb.state,
                    if pb.state == "draft" { " (publish it with `knobyte playbook update <id> --state active`)" } else { "" }
                )));
            }
            plan.read(playbook_rel(ctx, &pb.id));
            let workstream = match workstream.map(|w| w.trim().to_string()).filter(|w| !w.is_empty()) {
                Some(w) => {
                    crate::team::validate_entity_id(&w).map_err(TeamError::usage)?;
                    let ws = crate::team::workstreams::get_workstream(ctx.config, &w).ok_or_else(|| TeamError::not_found("Workstream", &w))?;
                    if ws.status == "archived" {
                        return Err(TeamError::validation(format!("Workstream '{}' is archived", w)));
                    }
                    plan.read(ctx.rel(&ctx.config.workstreams_dir().join(format!("{}.json", w))));
                    Some(w)
                }
                None => None,
            };
            let id = ctx.ids.id("run", || format!("run-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]))?;
            crate::team::validate_entity_id(&id).map_err(TeamError::usage)?;
            if runs_dir(ctx.config).join(format!("{}.json", id)).exists() {
                return Err(TeamError::conflict(format!("Playbook run '{}' already exists", id)));
            }
            let run = PlaybookRun {
                schema_version: PLAYBOOK_SCHEMA_VERSION,
                id,
                playbook_id: pb.id.clone(),
                playbook_title: pb.title.clone(),
                playbook_revision: pb.entity_revision,
                workstream: workstream.clone(),
                title: bounded_text("title", title.as_deref().unwrap_or(""), 512)?,
                state: "active".to_string(),
                steps: pb
                    .steps
                    .iter()
                    .map(|s| PlaybookRunStep {
                        step_id: s.id.clone(),
                        title: s.title.clone(),
                        description: s.description.clone(),
                        required_checks: s.required_checks.clone(),
                        expected_evidence: s.expected_evidence.clone(),
                        state: "pending".to_string(),
                        ..Default::default()
                    })
                    .collect(),
                entity_revision: 1,
                started_by: actor,
                started_at: now.clone(),
                started_at_commit: head,
                updated_at: now.clone(),
                ..Default::default()
            };
            let mut subjects = vec![
                ActivitySubject::entity("playbook_run", &run.id, Some(&run.playbook_title)),
                ActivitySubject::entity("playbook", &pb.id, Some(&pb.title)),
            ];
            if let Some(w) = &workstream {
                subjects.push(ActivitySubject::entity("workstream", w, None));
            }
            let summary = format!("Started playbook '{}' (run {})", pb.title, run.id);
            (run, summary, subjects)
        }
        "playbook.run.complete-step" => {
            let actor = require_known_actor(ctx, "complete a playbook step")?;
            let CompleteStepAction { run_id, step_id, evidence, note } = parse_action(action)?;
            crate::team::validate_entity_id(&run_id).map_err(TeamError::usage)?;
            let mut run = get_run(ctx.config, &run_id).ok_or_else(|| TeamError::not_found("Playbook run", &run_id))?;
            if run.state != "active" {
                return Err(TeamError::validation(format!("Playbook run '{}' is {}; it is immutable", run.id, run.state)));
            }
            if evidence.len() > 64 {
                return Err(TeamError::validation("At most 64 evidence references are allowed"));
            }
            for e in &evidence {
                let text = serde_json::to_string(e).unwrap_or_default();
                if text.len() > 4096 {
                    return Err(TeamError::validation("Evidence entries must be at most 4 KiB"));
                }
                if let EvidenceRef::File { path } = e {
                    if path.starts_with('/') || path.split('/').any(|p| p == "..") {
                        return Err(TeamError::validation(format!("Evidence file '{}' must be repository-relative", path)));
                    }
                }
            }
            let note = bounded_text("note", note.as_deref().unwrap_or(""), 4096)?;
            let index = resolve_run_step(&run, &step_id)?;
            let step = &mut run.steps[index];
            let step_id = step.step_id.clone();
            if step.state == "completed" {
                return Err(TeamError::validation(format!("Step '{}' is already complete; completed steps are immutable", step_id)));
            }
            if !step.expected_evidence.is_empty() && evidence.is_empty() && note.is_empty() {
                plan.diagnostics.push(Diagnostic::warning(
                    "EVIDENCE_MISSING",
                    format!("Step '{}' expects evidence ({}) but none was recorded", step_id, step.expected_evidence.join("; ")),
                ));
            }
            step.state = "completed".to_string();
            step.evidence = evidence;
            step.note = note;
            step.completed_by = Some(actor.clone());
            step.completed_at = Some(now.clone());
            step.completed_at_commit = head;
            let step_title = step.title.clone();
            run.steps_completed = (run.steps.len() - run.pending_steps()) as u64;
            run.entity_revision += 1;
            run.updated_at = now.clone();
            let finished = run.pending_steps() == 0;
            if finished {
                run.state = "completed".to_string();
                run.completed_at = Some(now.clone());
                run.completed_by = Some(actor);
            }
            let summary = if finished {
                format!("Completed step '{}' and finished playbook '{}' (run {})", step_title, run.playbook_title, run.id)
            } else {
                format!("Completed step '{}' of playbook '{}' (run {})", step_title, run.playbook_title, run.id)
            };
            let subjects = vec![
                ActivitySubject::entity("playbook_run", &run.id, Some(&run.playbook_title)),
                ActivitySubject::entity("playbook", &run.playbook_id, Some(&run.playbook_title)),
            ];
            (run, summary, subjects)
        }
        "playbook.run.abandon" => {
            let actor = require_known_actor(ctx, "abandon a playbook run")?;
            let AbandonAction { run_id, reason } = parse_action(action)?;
            crate::team::validate_entity_id(&run_id).map_err(TeamError::usage)?;
            let reason = bounded_text("reason", &reason, 4096)?;
            if reason.is_empty() {
                return Err(TeamError::validation("A reason is required to abandon a playbook run"));
            }
            let mut run = get_run(ctx.config, &run_id).ok_or_else(|| TeamError::not_found("Playbook run", &run_id))?;
            if run.state != "active" {
                return Err(TeamError::validation(format!("Playbook run '{}' is {}; it is immutable", run.id, run.state)));
            }
            run.state = "abandoned".to_string();
            run.abandoned_at = Some(now.clone());
            run.abandoned_by = Some(actor);
            run.abandon_reason = Some(reason);
            run.entity_revision += 1;
            run.updated_at = now.clone();
            let summary = format!("Abandoned playbook '{}' (run {})", run.playbook_title, run.id);
            let subjects = vec![
                ActivitySubject::entity("playbook_run", &run.id, Some(&run.playbook_title)),
                ActivitySubject::entity("playbook", &run.playbook_id, Some(&run.playbook_title)),
            ];
            (run, summary, subjects)
        }
        other => return Err(TeamError::usage(format!("Unsupported playbook action '{}'", other))),
    };
    plan.write_json(run_rel(ctx, &run.id), "canonical", &run, summary.clone())?;
    let label = if run.title.is_empty() { run.playbook_title.clone() } else { run.title.clone() };
    ctx.activity(&mut plan, None, kind, "playbook_run", &run.id, &label, summary.clone(), None, subjects, run.workstream.clone())?;
    plan.summary = summary;
    plan.result = json!(run);
    Ok(plan)
}
