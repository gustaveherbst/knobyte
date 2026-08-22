use std::fs;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::KnobyteConfig;
use crate::team::activity::ActivitySubject;
use crate::team::envelope::{paginate, Diagnostic, Page, TeamError};
use crate::team::identity::{read_selection, resolve_actor, selection_path, ActorRef};
use crate::team::validate_entity_id;
use crate::team::workflow::{parse_action, read_json_file, run_action, ActorChoice, Ctx, Plan};

pub const MEMBER_ACTIVE: &str = "active";
pub const MEMBER_INACTIVE: &str = "inactive";
const MAX_ALIASES: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct GitAlias {
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Member {
    pub id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(default, rename = "gitAliases")]
    pub git_aliases: Vec<GitAlias>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub status: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
}

impl Member {
    pub fn is_active(&self) -> bool {
        self.status == MEMBER_ACTIVE
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CurrentMemberSelection {
    #[serde(rename = "memberId")]
    pub member_id: String,
    #[serde(rename = "selectedAt")]
    pub selected_at: String,
}

pub fn list_members(config: &KnobyteConfig) -> Vec<Member> {
    let mut members = Vec::new();
    if let Ok(entries) = fs::read_dir(config.members_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("json") {
                if let Ok(content) = fs::read_to_string(&path) {
                    if let Ok(m) = serde_json::from_str::<Member>(&content) {
                        members.push(m);
                    }
                }
            }
        }
    }
    members.sort_by(|a, b| a.display_name.cmp(&b.display_name).then_with(|| a.id.cmp(&b.id)));
    members
}

/// Paged member listing. `active`: `Some(true)` only active, `Some(false)` only inactive.
pub fn list_members_page(
    config: &KnobyteConfig,
    active: Option<bool>,
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<Page<Member>, TeamError> {
    let mut all = list_members(config);
    if let Some(a) = active {
        all.retain(|m| m.is_active() == a);
    }
    paginate(all, |m| m.id.clone(), &format!("active={:?}", active), cursor, limit)
}

pub fn get_member(config: &KnobyteConfig, id: &str) -> Option<Member> {
    validate_entity_id(id).ok()?;
    read_json_file(&config.members_dir().join(format!("{}.json", id)))
}

/// Resolve the effective current member (pure read): the local selection when it
/// names an active member, else the one active member matching the Git identity.
pub fn get_current_member(config: &KnobyteConfig) -> Option<Member> {
    match resolve_actor(config).actor {
        ActorRef::Member { member_id, .. } => get_member(config, &member_id),
        _ => None,
    }
}

pub fn detect_git_user(project_root: &std::path::Path) -> (Option<String>, Option<String>) {
    let read = |key: &str| {
        std::process::Command::new("git")
            .args(["config", key])
            .current_dir(project_root)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    (read("user.name"), read("user.email"))
}

/// Validate a member id slug: lowercase ASCII letters, digits, `_` and `-`,
/// starting with a letter or digit, at most 64 characters.
pub fn validate_member_id(id: &str) -> Result<(), String> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id.chars().next().map(|c| c.is_ascii_lowercase() || c.is_ascii_digit()).unwrap_or(false)
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(format!("Invalid member id '{}': use a lowercase slug (a-z, 0-9, '_' or '-', max 64 chars)", id))
    }
}

fn normalize_email(email: Option<String>) -> Result<Option<String>, TeamError> {
    let email = email.map(|e| e.trim().to_string()).filter(|e| !e.is_empty());
    if let Some(ref e) = email {
        if !e.contains('@') || e.chars().any(char::is_whitespace) || e.len() > 320 {
            return Err(TeamError::validation(format!("Invalid email '{}'", e)));
        }
    }
    Ok(email)
}

fn normalize_aliases(aliases: Vec<GitAlias>) -> Result<Vec<GitAlias>, TeamError> {
    let mut out: Vec<GitAlias> = Vec::new();
    for a in aliases {
        let a = GitAlias {
            name: a.name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
            email: normalize_email(a.email)?,
        };
        if a.name.is_none() && a.email.is_none() {
            continue;
        }
        if a.name.as_ref().map(|n| n.len() > 200 || n.chars().any(char::is_control)).unwrap_or(false) {
            return Err(TeamError::validation("Git alias names must be at most 200 bytes without control characters"));
        }
        if !out.contains(&a) {
            out.push(a);
        }
    }
    if out.len() > MAX_ALIASES {
        return Err(TeamError::validation(format!("At most {} Git aliases are allowed", MAX_ALIASES)));
    }
    Ok(out)
}

fn display_name(name: &str) -> Result<String, TeamError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(TeamError::validation("Member display name must not be empty"));
    }
    if name.len() > 200 || name.chars().any(char::is_control) {
        return Err(TeamError::validation("Member display name must be at most 200 bytes without control characters"));
    }
    Ok(name.to_string())
}

// ---------------------------------------------------------------------------
// Workflow planners
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MemberInput {
    pub id: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default, rename = "gitAliases")]
    pub git_aliases: Option<Vec<GitAlias>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MemberPatch {
    #[serde(default, rename = "displayName", skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// `Some("")` clears the email.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// `Some("")` clears the role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, rename = "gitAliases", skip_serializing_if = "Option::is_none")]
    pub git_aliases: Option<Vec<GitAlias>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AddAction {
    member: MemberInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateAction {
    #[serde(rename = "memberId")]
    member_id: String,
    patch: MemberPatch,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdAction {
    #[serde(rename = "memberId")]
    member_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyAction {}

fn member_rel(ctx: &Ctx, id: &str) -> String {
    ctx.rel(&ctx.config.members_dir().join(format!("{}.json", id)))
}

fn require_member(ctx: &Ctx, id: &str) -> Result<Member, TeamError> {
    validate_entity_id(id).map_err(TeamError::usage)?;
    get_member(ctx.config, id).ok_or_else(|| TeamError::not_found("Member", id))
}

pub(crate) fn plan(ctx: &mut Ctx, kind: &str, action: &Value) -> Result<Plan, TeamError> {
    let mut plan = Plan::default();
    match kind {
        "member.add" => {
            let AddAction { member } = parse_action(action)?;
            validate_member_id(&member.id).map_err(TeamError::validation)?;
            let name = display_name(&member.display_name)?;
            if get_member(ctx.config, &member.id).is_some() || ctx.config.members_dir().join(format!("{}.json", member.id)).exists() {
                return Err(TeamError::conflict(format!("Member '{}' already exists", member.id)));
            }
            let email = normalize_email(member.email)?;
            let role = member.role.map(|r| r.trim().to_string()).filter(|r| !r.is_empty());
            let aliases = match member.git_aliases {
                Some(a) => normalize_aliases(a)?,
                None if email.is_some() => vec![GitAlias { name: Some(name.clone()), email: email.clone() }],
                None => Vec::new(),
            };
            let m = Member {
                id: member.id.clone(),
                display_name: name,
                git_aliases: aliases,
                email,
                role,
                status: MEMBER_ACTIVE.to_string(),
                created_at: ctx.now(),
                updated_at: ctx.now(),
            };
            plan.write_json(member_rel(ctx, &m.id), "canonical", &m, "Create member")?;
            // Only the very first member of an empty team registers themselves; any later
            // addition is attributed to the real actor (member, Git identity or unknown).
            let bootstrap = ctx.authority.actor.member_id().is_none() && list_members(ctx.config).is_empty();
            let actor = if bootstrap { Some(m.id.clone()) } else { None };
            ctx.activity(
                &mut plan,
                actor,
                "member.add",
                "member",
                &m.id,
                &m.display_name,
                format!("Added team member '{}' ({})", m.display_name, m.id),
                None,
                vec![ActivitySubject::entity("member", &m.id, Some(&m.display_name))],
                None,
            )?;
            plan.summary = format!("Add member '{}'", m.id);
            plan.result = json!(m);
        }
        "member.update" => {
            let UpdateAction { member_id, patch } = parse_action(action)?;
            let current = require_member(ctx, &member_id)?;
            let mut m = current.clone();
            if let Some(n) = patch.display_name {
                m.display_name = display_name(&n)?;
            }
            if let Some(e) = patch.email {
                m.email = normalize_email(Some(e))?;
            }
            if let Some(r) = patch.role {
                m.role = Some(r.trim().to_string()).filter(|r| !r.is_empty());
            }
            if let Some(a) = patch.git_aliases {
                m.git_aliases = normalize_aliases(a)?;
            }
            if m == current {
                return Err(TeamError::validation(format!("Member {} already has the proposed values", member_id)));
            }
            m.updated_at = ctx.now();
            plan.write_json(member_rel(ctx, &m.id), "canonical", &m, "Update member")?;
            ctx.activity(&mut plan, None, "member.update", "member", &m.id, &m.display_name,
                format!("Updated team member '{}'", m.id), None,
                vec![ActivitySubject::entity("member", &m.id, Some(&m.display_name))], None)?;
            plan.summary = format!("Update member '{}'", m.id);
            plan.result = json!(m);
        }
        "member.deactivate" | "member.reactivate" => {
            let IdAction { member_id } = parse_action(action)?;
            let mut m = require_member(ctx, &member_id)?;
            let deactivate = kind == "member.deactivate";
            if deactivate && !m.is_active() {
                return Err(TeamError::validation(format!("Member {} is already inactive", member_id)));
            }
            if !deactivate && m.is_active() {
                return Err(TeamError::validation(format!("Member {} is already active", member_id)));
            }
            if deactivate {
                if let Some(sel) = read_selection(ctx.config) {
                    if sel.member_id == member_id {
                        return Err(TeamError::validation(format!(
                            "Member {} is the current local selection; run `knobyte member clear` first",
                            member_id
                        )));
                    }
                }
                plan.read(ctx.rel(&selection_path(ctx.config)));
            }
            m.status = if deactivate { MEMBER_INACTIVE } else { MEMBER_ACTIVE }.to_string();
            m.updated_at = ctx.now();
            plan.write_json(member_rel(ctx, &m.id), "canonical", &m, if deactivate { "Deactivate member" } else { "Reactivate member" })?;
            ctx.activity(&mut plan, None, kind, "member", &m.id, &m.display_name,
                format!("{} team member '{}'", if deactivate { "Deactivated" } else { "Reactivated" }, m.id), None,
                vec![ActivitySubject::entity("member", &m.id, Some(&m.display_name))], None)?;
            plan.summary = format!("{} member '{}'", if deactivate { "Deactivate" } else { "Reactivate" }, m.id);
            plan.result = json!(m);
        }
        "member.select" => {
            let IdAction { member_id } = parse_action(action)?;
            let m = require_member(ctx, &member_id)?;
            if !m.is_active() {
                return Err(TeamError::validation(format!("Cannot select deactivated member '{}'", member_id)));
            }
            plan.read(member_rel(ctx, &m.id));
            let sel = CurrentMemberSelection { member_id: m.id.clone(), selected_at: ctx.now() };
            plan.write_json(ctx.rel(&selection_path(ctx.config)), "local", &sel, "Select current member")?;
            plan.summary = format!("Select member '{}'", m.id);
            plan.result = json!(sel);
        }
        "member.clear" => {
            let EmptyAction {} = parse_action(action)?;
            let path = selection_path(ctx.config);
            if path.exists() {
                plan.delete(ctx.rel(&path), "local", "Clear current member");
            }
            plan.summary = "Clear current member selection".to_string();
            plan.result = Value::Null;
        }
        other => return Err(TeamError::usage(format!("Unsupported member action '{}'", other))),
    }
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Library API (one-shot preview + apply)
// ---------------------------------------------------------------------------

fn result_member(r: crate::team::workflow::ApplyResult) -> Result<Member, TeamError> {
    serde_json::from_value(r.result).map_err(|e| TeamError::internal(e.to_string()))
}

/// Create and persist a new active member, recording an activity entry.
/// Refuses invalid id slugs, empty names, malformed emails and duplicate ids.
pub fn create_member(
    config: &KnobyteConfig,
    id: &str,
    name: &str,
    email: Option<&str>,
    role: Option<&str>,
) -> Result<Member, String> {
    validate_member_id(id)?;
    let action = json!({
        "kind": "member.add",
        "member": { "id": id, "displayName": name, "email": email, "role": role }
    });
    Ok(result_member(run_action(config, action, &ActorChoice::resolved())?)?)
}

/// Update a member's display name, email, role or Git aliases.
pub fn update_member(config: &KnobyteConfig, id: &str, patch: &MemberPatch) -> Result<Member, TeamError> {
    let action = json!({ "kind": "member.update", "memberId": id, "patch": patch });
    result_member(run_action(config, action, &ActorChoice::resolved())?)
}

/// Mark a member inactive (refused while it is this checkout's selection).
pub fn deactivate_member(config: &KnobyteConfig, id: &str) -> Result<Member, TeamError> {
    result_member(run_action(config, json!({ "kind": "member.deactivate", "memberId": id }), &ActorChoice::resolved())?)
}

/// Restore an inactive member.
pub fn reactivate_member(config: &KnobyteConfig, id: &str) -> Result<Member, TeamError> {
    result_member(run_action(config, json!({ "kind": "member.reactivate", "memberId": id }), &ActorChoice::resolved())?)
}

pub fn select_current_member(config: &KnobyteConfig, member_id: &str) -> Result<(), String> {
    run_action(config, json!({ "kind": "member.select", "memberId": member_id }), &ActorChoice::resolved())?;
    Ok(())
}

pub fn clear_current_member(config: &KnobyteConfig) -> Result<(), String> {
    if !selection_path(config).exists() {
        return Ok(());
    }
    run_action(config, json!({ "kind": "member.clear" }), &ActorChoice::resolved())?;
    Ok(())
}

/// Write a member record directly (atomic). Used for imports and fixtures.
pub fn save_member(config: &KnobyteConfig, member: &Member) -> Result<(), String> {
    validate_entity_id(&member.id)?;
    crate::team::store::write_json_atomic(&config.members_dir().join(format!("{}.json", member.id)), member)
}

/// `member current` projection: actor, resolution source, selection, diagnostics.
pub fn current_actor_view(config: &KnobyteConfig) -> (Value, Vec<Diagnostic>) {
    let r = resolve_actor(config);
    let member = r.actor.member_id().and_then(|id| get_member(config, id));
    (
        json!({
            "actor": r.actor,
            "source": r.source,
            "selection": r.selection,
            "member": member,
        }),
        r.diagnostics,
    )
}
