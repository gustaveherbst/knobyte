use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::config::KnobyteConfig;
use crate::team::activity::ActivitySubject;
use crate::team::envelope::{paginate, Diagnostic, Page, TeamError};
use crate::team::identity::resolve_actor;
use crate::team::members::get_member;
use crate::team::refs::{CodeRef, EvidenceRef};
use crate::team::validate_entity_id;
use crate::team::workflow::{parse_action, read_json_file, run_action, ActorChoice, Ctx, Plan};
use crate::team::workstreams::get_git_state;

pub const RELAY_SCHEMA_VERSION: u32 = 4;
pub const RELAY_STATES: &[&str] = &["published", "acknowledged", "closed"];
pub const MAX_RECIPIENTS: usize = 32;
const MAX_ITEMS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ObservedRepoState {
    pub branch: Option<String>,
    #[serde(rename = "headCommit")]
    pub head_commit: Option<String>,
    #[serde(rename = "dirtyTree")]
    pub dirty_tree: bool,
    pub timestamp: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct RelayDraft {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub sender: String,
    #[serde(default, rename = "openToTeam")]
    pub open_to_team: bool,
    #[serde(default, rename = "namedRecipients")]
    pub named_recipients: Vec<String>,
    #[serde(default)]
    pub progress: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default, rename = "nextActions")]
    pub next_actions: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    /// `team` (claimable by any active member) or `members` (named recipients only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed: Vec<String>,
    #[serde(default, rename = "inProgress", skip_serializing_if = "Vec::is_empty")]
    pub in_progress: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
    #[serde(default, rename = "unresolvedQuestions", skip_serializing_if = "Vec::is_empty")]
    pub unresolved_questions: Vec<String>,
    #[serde(default, rename = "changedFiles", skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code: Vec<CodeRef>,
    #[serde(default, rename = "evidenceRefs", skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<EvidenceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workstream: Option<String>,
    #[serde(default, rename = "updatedAt", skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl RelayDraft {
    fn is_team(&self) -> bool {
        match self.audience.as_deref() {
            Some("team") => true,
            Some(_) => false,
            None => self.open_to_team || self.named_recipients.is_empty(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Relay {
    pub id: String,
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub title: String,
    pub summary: String,
    pub sender: String,
    #[serde(rename = "openToTeam")]
    pub open_to_team: bool,
    #[serde(rename = "namedRecipients")]
    pub named_recipients: Vec<String>,
    /// Member who acknowledged (claimed) the relay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimant: Option<String>,
    pub status: String,
    #[serde(default)]
    pub progress: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default, rename = "nextActions")]
    pub next_actions: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(rename = "observedState")]
    pub observed_state: ObservedRepoState,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(default, rename = "acknowledgedAt", skip_serializing_if = "Option::is_none")]
    pub acknowledged_at: Option<String>,
    #[serde(default, rename = "closedAt", skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed: Vec<String>,
    #[serde(default, rename = "inProgress", skip_serializing_if = "Vec::is_empty")]
    pub in_progress: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
    #[serde(default, rename = "unresolvedQuestions", skip_serializing_if = "Vec::is_empty")]
    pub unresolved_questions: Vec<String>,
    #[serde(default, rename = "changedFiles", skip_serializing_if = "Vec::is_empty")]
    pub changed_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code: Vec<CodeRef>,
    #[serde(default, rename = "evidenceRefs", skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<EvidenceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workstream: Option<String>,
    #[serde(default, rename = "acknowledgedBy", skip_serializing_if = "Option::is_none")]
    pub acknowledged_by: Option<String>,
    #[serde(default, rename = "closedBy", skip_serializing_if = "Option::is_none")]
    pub closed_by: Option<String>,
}

impl Relay {
    pub fn is_team(&self) -> bool {
        match self.audience.as_deref() {
            Some("team") => true,
            Some(_) => false,
            None => self.open_to_team,
        }
    }
    pub fn claimed_by(&self) -> Option<&str> {
        self.acknowledged_by.as_deref().or(self.claimant.as_deref())
    }
}

pub fn relay_drafts_dir(config: &KnobyteConfig) -> PathBuf {
    config.local_dir().join("relay_drafts")
}

fn relay_draft_path(config: &KnobyteConfig, id: &str) -> Result<PathBuf, String> {
    validate_entity_id(id)?;
    Ok(relay_drafts_dir(config).join(format!("{}.json", id)))
}

fn relay_path(config: &KnobyteConfig, id: &str) -> Result<PathBuf, String> {
    validate_entity_id(id)?;
    Ok(config.relays_dir().join(format!("{}.json", id)))
}

fn git_output(project_root: &Path, args: &[&str]) -> Option<String> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Capture the observed repository state (branch, HEAD commit, dirty tree) via git.
/// Fields are `None`/`false` when the project is not a git repository.
pub fn observe_repo_state(project_root: &Path) -> ObservedRepoState {
    let head_commit = git_output(project_root, &["rev-parse", "HEAD"]);
    let branch = git_output(project_root, &["symbolic-ref", "--short", "-q", "HEAD"]);
    let dirty_tree = if head_commit.is_some() || branch.is_some() {
        let (_head, dirty_files) = get_git_state(project_root);
        !dirty_files.is_empty()
    } else {
        false
    };
    ObservedRepoState { branch, head_commit, dirty_tree, timestamp: chrono::Utc::now().to_rfc3339() }
}

/// Repository-relative files changed in the working tree and on the branch
/// relative to its upstream (falls back to the working tree only). Bounded.
pub fn detect_changed_files(project_root: &Path) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    let mut push = |f: &str| {
        let f = f.trim().trim_matches('"').to_string();
        if !f.is_empty() && !f.starts_with(".knobyte/") && !files.contains(&f) && files.len() < MAX_ITEMS {
            files.push(f);
        }
    };
    if let Some(out) = git_output(project_root, &["diff", "--name-only", "@{upstream}...HEAD"]) {
        for l in out.lines() {
            push(l);
        }
    }
    if let Some(out) = git_output(project_root, &["status", "--porcelain", "--untracked-files=normal"]) {
        for l in out.lines().filter(|l| l.len() > 3) {
            let path = &l[3..];
            push(path.rsplit(" -> ").next().unwrap_or(path));
        }
    }
    files
}

fn load_dir<T: for<'de> Deserialize<'de>>(dir: PathBuf) -> Vec<T> {
    let mut list = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(d) = serde_json::from_str::<T>(&content) {
                        list.push(d);
                    }
                }
            }
        }
    }
    list
}

pub fn list_relay_drafts(config: &KnobyteConfig) -> Vec<RelayDraft> {
    let mut list: Vec<RelayDraft> = load_dir(relay_drafts_dir(config));
    list.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id)));
    list
}

pub fn list_relay_drafts_page(config: &KnobyteConfig, cursor: Option<&str>, limit: Option<usize>) -> Result<Page<RelayDraft>, TeamError> {
    paginate(list_relay_drafts(config), |d| d.id.clone(), "relay-drafts", cursor, limit)
}

pub fn get_relay_draft(config: &KnobyteConfig, id: &str) -> Option<RelayDraft> {
    read_json_file(&relay_draft_path(config, id).ok()?)
}

fn validate_recipients(recipients: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for r in recipients {
        validate_entity_id(r).map_err(|e| format!("Invalid recipient: {}", e))?;
        if !out.contains(r) {
            out.push(r.clone());
        }
    }
    if out.len() > MAX_RECIPIENTS {
        return Err(format!("A relay may name at most {} unique recipients", MAX_RECIPIENTS));
    }
    Ok(out)
}

/// Write a relay draft directly (atomic). Validates ids and recipient bounds.
pub fn save_relay_draft(config: &KnobyteConfig, draft: &RelayDraft) -> Result<(), String> {
    let path = relay_draft_path(config, &draft.id)?;
    validate_recipients(&draft.named_recipients)?;
    crate::team::store::write_json_atomic(&path, draft)
}

pub fn delete_relay_draft(config: &KnobyteConfig, id: &str) -> Result<(), String> {
    let path = relay_draft_path(config, id)?;
    crate::team::store::remove_if_exists(&path).map_err(|e| e.to_string())
}

pub fn list_relays(config: &KnobyteConfig) -> Vec<Relay> {
    let mut list: Vec<Relay> = load_dir(config.relays_dir());
    list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
    list
}

/// Filtered, paged relay listing.
///
/// * `perspective`: `all` (default), `mine` (published relays addressed to me or
///   the team, and relays I acknowledged), `sent` (relays I sent). `mine`/`sent`
///   need an active current member.
/// * `states`: published / acknowledged / closed (any of).
/// * `workstream`: only relays recorded against that workstream.
pub fn list_relays_page(
    config: &KnobyteConfig,
    perspective: Option<&str>,
    states: &[String],
    workstream: Option<&str>,
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<Page<Relay>, TeamError> {
    let perspective = perspective.unwrap_or("all");
    if !["all", "mine", "sent"].contains(&perspective) {
        return Err(TeamError::usage("--perspective must be one of: all, mine, sent"));
    }
    for s in states {
        if !RELAY_STATES.contains(&s.as_str()) {
            return Err(TeamError::usage(format!("--state must be one of: {}", RELAY_STATES.join(", "))));
        }
    }
    let me = if perspective == "all" {
        None
    } else {
        let actor = resolve_actor(config).actor;
        let id = actor.member_id().filter(|id| get_member(config, id).map(|m| m.is_active()).unwrap_or(false)).map(|s| s.to_string());
        Some(id.ok_or_else(|| {
            TeamError::unauthorized("Select an active member, or configure one unique active Git alias, to view personal relays.")
        })?)
    };
    let mut all = list_relays(config);
    all.retain(|r| {
        if !states.is_empty() && !states.contains(&r.status) {
            return false;
        }
        if let Some(ws) = workstream {
            if r.workstream.as_deref() != Some(ws) {
                return false;
            }
        }
        match (perspective, me.as_deref()) {
            ("sent", Some(me)) => r.sender == me,
            ("mine", Some(me)) => {
                if r.status == "published" {
                    r.is_team() || r.named_recipients.iter().any(|x| x == me)
                } else {
                    r.claimed_by() == Some(me)
                }
            }
            _ => true,
        }
    });
    paginate(
        all,
        |r| r.id.clone(),
        &format!("p={};s={:?};w={:?};me={:?}", perspective, states, workstream, me),
        cursor,
        limit,
    )
}

pub fn get_relay(config: &KnobyteConfig, id: &str) -> Option<Relay> {
    read_json_file(&relay_path(config, id).ok()?)
}

// ---------------------------------------------------------------------------
// Workflow planners
// ---------------------------------------------------------------------------

/// Caller-owned relay draft content (sparse: every field optional except `summary`).
#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RelayDraftInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    #[serde(default)]
    pub recipients: Vec<String>,
    #[serde(default)]
    pub completed: Vec<String>,
    #[serde(default, rename = "inProgress")]
    pub in_progress: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<String>,
    #[serde(default, rename = "unresolvedQuestions")]
    pub unresolved_questions: Vec<String>,
    #[serde(default, rename = "changedFiles")]
    pub changed_files: Vec<String>,
    #[serde(default)]
    pub code: Vec<CodeRef>,
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default, rename = "nextActions")]
    pub next_actions: Vec<String>,
    /// Legacy free-form progress notes.
    #[serde(default)]
    pub progress: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workstream: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftSaveAction {
    #[serde(default, rename = "draftId")]
    draft_id: Option<String>,
    draft: RelayDraftInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftIdAction {
    #[serde(rename = "draftId")]
    draft_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RelayIdAction {
    #[serde(rename = "relayId")]
    relay_id: String,
}

fn items(label: &str, v: &[String]) -> Result<Vec<String>, TeamError> {
    let mut out: Vec<String> = Vec::new();
    for s in v {
        let t = s.trim();
        if t.is_empty() {
            continue;
        }
        if t.len() > 4096 {
            return Err(TeamError::validation(format!("{} entries must be at most 4096 bytes", label)));
        }
        if !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    if out.len() > MAX_ITEMS {
        return Err(TeamError::validation(format!("At most {} {} entries are allowed", MAX_ITEMS, label)));
    }
    Ok(out)
}

fn normalize_input(input: RelayDraftInput) -> Result<RelayDraftInput, TeamError> {
    let summary = input.summary.trim().to_string();
    if summary.is_empty() || summary.len() > 8192 {
        return Err(TeamError::validation("Relay summary must be 1-8192 bytes"));
    }
    let audience = match input.audience.as_deref() {
        None => None,
        Some("team") | Some("members") => input.audience.clone(),
        Some(other) => return Err(TeamError::validation(format!("audience must be 'team' or 'members', got '{}'", other))),
    };
    let recipients = validate_recipients(&input.recipients).map_err(TeamError::validation)?;
    for f in &input.changed_files {
        if f.starts_with('/') || f.split('/').any(|p| p == "..") {
            return Err(TeamError::validation(format!("changedFiles entry '{}' must be repository-relative", f)));
        }
    }
    if input.code.len() > MAX_ITEMS || input.evidence.len() > MAX_ITEMS {
        return Err(TeamError::validation(format!("At most {} code/evidence references are allowed", MAX_ITEMS)));
    }
    if let Some(ws) = &input.workstream {
        validate_entity_id(ws).map_err(TeamError::validation)?;
    }
    let title = input
        .title
        .as_deref()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| summary.lines().next().unwrap_or("").chars().take(80).collect());
    if title.len() > 512 {
        return Err(TeamError::validation("Relay title must be at most 512 bytes"));
    }
    Ok(RelayDraftInput {
        title: Some(title),
        summary,
        audience,
        recipients,
        completed: items("completed", &input.completed)?,
        in_progress: items("inProgress", &input.in_progress)?,
        decisions: items("decisions", &input.decisions)?,
        blockers: items("blockers", &input.blockers)?,
        unresolved_questions: items("unresolvedQuestions", &input.unresolved_questions)?,
        changed_files: items("changedFiles", &input.changed_files)?,
        code: input.code,
        evidence: input.evidence,
        next_actions: items("nextActions", &input.next_actions)?,
        progress: items("progress", &input.progress)?,
        workstream: input.workstream,
    })
}

fn draft_rel(ctx: &Ctx, id: &str) -> Result<String, TeamError> {
    Ok(ctx.rel(&relay_draft_path(ctx.config, id).map_err(TeamError::usage)?))
}

fn relay_rel(ctx: &Ctx, id: &str) -> Result<String, TeamError> {
    Ok(ctx.rel(&relay_path(ctx.config, id).map_err(TeamError::usage)?))
}

fn require_draft(ctx: &Ctx, id: &str) -> Result<RelayDraft, TeamError> {
    validate_entity_id(id).map_err(TeamError::usage)?;
    get_relay_draft(ctx.config, id).ok_or_else(|| TeamError::not_found("Relay draft", id))
}

fn require_relay(ctx: &Ctx, id: &str) -> Result<Relay, TeamError> {
    validate_entity_id(id).map_err(TeamError::usage)?;
    get_relay(ctx.config, id).ok_or_else(|| TeamError::not_found("Relay", id))
}

pub(crate) fn plan(ctx: &mut Ctx, kind: &str, action: &Value) -> Result<Plan, TeamError> {
    let mut plan = Plan::default();
    match kind {
        "relay.draft.save" => {
            let DraftSaveAction { draft_id, draft } = parse_action(action)?;
            let input = normalize_input(draft)?;
            let existing = match &draft_id {
                Some(id) => Some(require_draft(ctx, id)?),
                None => None,
            };
            let id = match draft_id {
                Some(id) => id,
                None => ctx.ids.id("relay-draft", || format!("draft_{}", Uuid::new_v4()))?,
            };
            let team = input.audience.as_deref() == Some("team") || (input.audience.is_none() && input.recipients.is_empty());
            let d = RelayDraft {
                id: id.clone(),
                title: input.title.clone().unwrap_or_default(),
                summary: input.summary.clone(),
                sender: existing.as_ref().map(|e| e.sender.clone()).unwrap_or_else(|| ctx.actor_id()),
                open_to_team: team,
                named_recipients: input.recipients.clone(),
                progress: input.progress.clone(),
                blockers: input.blockers.clone(),
                next_actions: input.next_actions.clone(),
                evidence: input.evidence.iter().map(|e| e.display()).collect(),
                created_at: existing.as_ref().map(|e| e.created_at.clone()).unwrap_or_else(|| ctx.now()),
                audience: Some(if team { "team" } else { "members" }.to_string()),
                completed: input.completed.clone(),
                in_progress: input.in_progress.clone(),
                decisions: input.decisions.clone(),
                unresolved_questions: input.unresolved_questions.clone(),
                changed_files: input.changed_files.clone(),
                code: input.code.clone(),
                evidence_refs: input.evidence.clone(),
                workstream: input.workstream.clone(),
                updated_at: Some(ctx.now()),
            };
            // Same rule as publish and the MCP relay tool: a draft may only name
            // registered, active members, so it cannot be saved toward a dead end.
            let unusable: Vec<&str> = d
                .named_recipients
                .iter()
                .filter(|r| !get_member(ctx.config, r).map(|m| m.is_active()).unwrap_or(false))
                .map(|s| s.as_str())
                .collect();
            if !unusable.is_empty() {
                return Err(TeamError::validation(format!(
                    "Unknown or inactive recipient(s): {} (see `knobyte member list`)",
                    unusable.join(", ")
                )));
            }
            plan.write_json(draft_rel(ctx, &id)?, "local", &d, "Save local relay draft")?;
            plan.summary = format!("Save relay draft '{}'", id);
            plan.result = json!(d);
        }
        "relay.draft.delete" => {
            let DraftIdAction { draft_id } = parse_action(action)?;
            require_draft(ctx, &draft_id)?;
            plan.delete(draft_rel(ctx, &draft_id)?, "local", "Delete local relay draft");
            plan.summary = format!("Delete relay draft '{}'", draft_id);
            plan.result = json!({ "id": draft_id });
        }
        "relay.publish" => {
            let DraftIdAction { draft_id } = parse_action(action)?;
            let actor = ctx.require_active_member("publish a relay")?;
            let d = require_draft(ctx, &draft_id)?;
            let team = d.is_team();
            let recipients = validate_recipients(&d.named_recipients).map_err(TeamError::validation)?;
            if !team && recipients.is_empty() {
                return Err(TeamError::validation(
                    "Choose at least one active member or open the handoff to the team before publishing.",
                ));
            }
            for r in &recipients {
                let m = get_member(ctx.config, r).ok_or_else(|| TeamError::not_found("Member", r))?;
                if !m.is_active() {
                    return Err(TeamError::validation(format!("Relay recipient {} is inactive.", r)));
                }
                plan.read(ctx.rel(&ctx.config.members_dir().join(format!("{}.json", r))));
            }
            let id = ctx.ids.id("relay", || format!("relay_{}", Uuid::new_v4()))?;
            let mut observed = ctx.authority.repo_state.clone();
            observed.timestamp = ctx.now();
            let relay = Relay {
                id: id.clone(),
                schema_version: RELAY_SCHEMA_VERSION,
                title: d.title.clone(),
                summary: d.summary.clone(),
                sender: actor.clone(),
                open_to_team: team,
                named_recipients: recipients,
                claimant: None,
                status: "published".to_string(),
                progress: d.progress.clone(),
                blockers: d.blockers.clone(),
                next_actions: d.next_actions.clone(),
                evidence: d.evidence.clone(),
                observed_state: observed,
                created_at: ctx.now(),
                updated_at: ctx.now(),
                acknowledged_at: None,
                closed_at: None,
                audience: Some(if team { "team" } else { "members" }.to_string()),
                completed: d.completed.clone(),
                in_progress: d.in_progress.clone(),
                decisions: d.decisions.clone(),
                unresolved_questions: d.unresolved_questions.clone(),
                changed_files: d.changed_files.clone(),
                code: d.code.clone(),
                evidence_refs: d.evidence_refs.clone(),
                workstream: d.workstream.clone(),
                acknowledged_by: None,
                closed_by: None,
            };
            if relay.observed_state.dirty_tree {
                plan.diagnostics.push(Diagnostic::warning(
                    "RELAY_DIRTY_PUBLICATION",
                    "The working tree has uncommitted changes; the recipient may not see them.",
                ));
            }
            plan.write_json(relay_rel(ctx, &id)?, "canonical", &relay, "Publish relay")?;
            plan.delete(draft_rel(ctx, &draft_id)?, "local", "Remove published relay draft");
            ctx.activity(
                &mut plan,
                None,
                "relay.publish",
                "relay",
                &relay.id,
                &relay.title,
                format!("Published relay '{}'", relay.title),
                crate::team::workflow::meta(&[
                    ("openToTeam", json!(relay.open_to_team)),
                    ("namedRecipients", json!(relay.named_recipients)),
                    ("branch", json!(relay.observed_state.branch)),
                    ("headCommit", json!(relay.observed_state.head_commit)),
                ]),
                vec![ActivitySubject::entity("relay", &relay.id, Some(&relay.title))],
                relay.workstream.clone(),
            )?;
            plan.summary = format!("Publish relay '{}'", relay.title);
            plan.result = json!(relay);
        }
        "relay.acknowledge" | "relay.close" => {
            let RelayIdAction { relay_id } = parse_action(action)?;
            let actor = ctx.require_active_member(if kind == "relay.acknowledge" { "acknowledge a relay" } else { "close a relay" })?;
            let mut relay = require_relay(ctx, &relay_id)?;
            if kind == "relay.acknowledge" {
                if relay.status != "published" {
                    return Err(TeamError::validation(format!(
                        "Relay '{}' is already in status '{}'; only a published relay can be acknowledged",
                        relay_id, relay.status
                    )));
                }
                if !relay.is_team() && !relay.named_recipients.iter().any(|r| r == &actor) {
                    return Err(TeamError::unauthorized(format!(
                        "Member '{}' is not a named recipient of relay '{}' (recipients: {})",
                        actor,
                        relay_id,
                        relay.named_recipients.join(", ")
                    )));
                }
                relay.claimant = Some(actor.clone());
                relay.acknowledged_by = Some(actor.clone());
                relay.acknowledged_at = Some(ctx.now());
                relay.status = "acknowledged".to_string();
            } else {
                if relay.status == "closed" {
                    return Err(TeamError::validation(format!("Relay '{}' is already closed", relay_id)));
                }
                if relay.status != "acknowledged" {
                    return Err(TeamError::validation(format!(
                        "Relay '{}' has not been acknowledged yet; only an acknowledged relay can be closed",
                        relay_id
                    )));
                }
                let claimant = relay.claimed_by().unwrap_or("").to_string();
                for p in [&relay.sender, &claimant] {
                    if !get_member(ctx.config, p).map(|m| m.is_active()).unwrap_or(false) {
                        return Err(TeamError::unauthorized(format!(
                            "The recorded relay sender and claimant must both remain active members ('{}' is not)",
                            p
                        )));
                    }
                }
                if actor != relay.sender && actor != claimant {
                    return Err(TeamError::unauthorized(format!(
                        "Member '{}' is not the sender or claimant of relay '{}'",
                        actor, relay_id
                    )));
                }
                relay.status = "closed".to_string();
                relay.closed_by = Some(actor.clone());
                relay.closed_at = Some(ctx.now());
            }
            relay.updated_at = ctx.now();
            let verb = if kind == "relay.acknowledge" { "Acknowledged" } else { "Closed" };
            plan.write_json(relay_rel(ctx, &relay.id)?, "canonical", &relay, format!("{} relay", verb))?;
            ctx.activity(
                &mut plan,
                None,
                kind,
                "relay",
                &relay.id,
                &relay.title,
                format!("{} relay '{}'", verb, relay.title),
                None,
                vec![ActivitySubject::entity("relay", &relay.id, Some(&relay.title))],
                relay.workstream.clone(),
            )?;
            plan.summary = format!("{} relay '{}'", verb, relay.id);
            plan.result = json!(relay);
        }
        other => return Err(TeamError::usage(format!("Unsupported relay action '{}'", other))),
    }
    Ok(plan)
}

fn result_relay(r: crate::team::workflow::ApplyResult) -> Result<Relay, TeamError> {
    serde_json::from_value(r.result).map_err(|e| TeamError::internal(e.to_string()))
}

/// Publish a relay draft as the current actor. When the checkout has no active
/// member, the draft's recorded sender acts if it is an active member.
pub fn publish_relay_draft(config: &KnobyteConfig, draft_id: &str) -> Result<Relay, String> {
    let actor = match resolve_actor(config).actor.member_id() {
        Some(_) => ActorChoice::resolved(),
        None => match get_relay_draft(config, draft_id) {
            Some(d) if get_member(config, &d.sender).map(|m| m.is_active()).unwrap_or(false) => ActorChoice::trusted(&d.sender),
            _ => ActorChoice::resolved(),
        },
    };
    Ok(result_relay(run_action(config, json!({ "kind": "relay.publish", "draftId": draft_id }), &actor)?)?)
}

/// Claim a published relay as `member_id`. When the relay is not open to the
/// team, only a named recipient may acknowledge it.
pub fn acknowledge_relay(config: &KnobyteConfig, relay_id: &str, member_id: &str) -> Result<Relay, String> {
    validate_entity_id(member_id).map_err(|e| format!("Invalid member: {}", e))?;
    Ok(result_relay(run_action(config, json!({ "kind": "relay.acknowledge", "relayId": relay_id }), &ActorChoice::trusted(member_id))?)?)
}

/// Close an acknowledged relay as its sender or claimant.
pub fn close_relay(config: &KnobyteConfig, relay_id: &str, member_id: &str) -> Result<Relay, String> {
    validate_entity_id(member_id).map_err(|e| format!("Invalid member: {}", e))?;
    Ok(result_relay(run_action(config, json!({ "kind": "relay.close", "relayId": relay_id }), &ActorChoice::trusted(member_id))?)?)
}
