use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::config::KnobyteConfig;
use crate::events::EventEntry;
use crate::team::activity::ActivitySubject;
use crate::team::envelope::{paginate, Diagnostic, ErrorCode, Page, TeamError};
use crate::team::refs::EvidenceRef;
use crate::team::store::{file_revision, JournalAppend};
use crate::team::validate_entity_id;
use crate::team::workflow::{parse_action, read_json_file, run_action, ActorChoice, Ctx, Plan, RevisionExpectation};
use crate::wiki::models::WikiEntity;
use crate::wiki::parser::parse_markdown_entity;

pub const PROPOSAL_MODE_APPEND: &str = "append";
pub const PROPOSAL_MODE_REPLACE: &str = "replace";
pub const PROPOSAL_STATUS_PENDING: &str = "pending";
/// Machine code leading the error detail when an author approves their own
/// proposal without the self-approval acknowledgement. Every transport maps it
/// to its own confirmation (CLI `--self-approve`, Hub checkbox, `selfApprove`).
pub const SELF_APPROVAL_REQUIRED: &str = "SELF_APPROVAL_REQUIRED";
pub const PROPOSAL_STATUS_APPROVED: &str = "approved";
pub const PROPOSAL_STATUS_REJECTED: &str = "rejected";
pub const PROPOSAL_STATUS_WITHDRAWN: &str = "withdrawn";
pub const PROPOSAL_STATUS_STALE: &str = "stale";
pub const PROPOSAL_STATES: &[&str] = &["pending", "approved", "rejected", "withdrawn", "stale"];
pub const KNOWLEDGE_KINDS: &[&str] = &["architecture", "component", "convention", "decision", "pattern", "guide"];
pub const SPEC_KINDS: &[&str] = &["spec", "requirement", "constraint", "acceptance_criterion"];
pub const ENTITY_LIFECYCLES: &[&str] = &["in_flight", "promoted"];

const MAX_TITLE: usize = 512;
const MAX_SUMMARY: usize = 2 * 1024;
const MAX_RATIONALE: usize = 8 * 1024;
const MAX_BODY: usize = 16 * 1024;
const MAX_TOPICS: usize = 64;

// ---------------------------------------------------------------------------
// Typed changes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct EntityTarget {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields)]
pub struct ContentPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpecRelation {
    /// `derived_from`, `refines`, `constrained_by` or `verified_by`.
    #[serde(rename = "type")]
    pub rel_type: String,
    pub target: EntityTarget,
}

/// One closed, typed knowledge change (no raw file slot).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum InboxChange {
    #[serde(rename = "knowledge.create")]
    KnowledgeCreate {
        #[serde(rename = "entityKind")]
        entity_kind: String,
        title: String,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        topics: Vec<String>,
    },
    #[serde(rename = "knowledge.update")]
    KnowledgeUpdate { target: EntityTarget, patch: ContentPatch },
    #[serde(rename = "spec.create")]
    SpecCreate {
        #[serde(rename = "entityKind")]
        entity_kind: String,
        title: String,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        topics: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relation: Option<SpecRelation>,
    },
    #[serde(rename = "spec.update")]
    SpecUpdate { target: EntityTarget, patch: ContentPatch },
}

impl InboxChange {
    pub fn kind(&self) -> &'static str {
        match self {
            InboxChange::KnowledgeCreate { .. } => "knowledge.create",
            InboxChange::KnowledgeUpdate { .. } => "knowledge.update",
            InboxChange::SpecCreate { .. } => "spec.create",
            InboxChange::SpecUpdate { .. } => "spec.update",
        }
    }
    fn is_spec(&self) -> bool {
        matches!(self, InboxChange::SpecCreate { .. } | InboxChange::SpecUpdate { .. })
    }
}

// ---------------------------------------------------------------------------
// Stored records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct InboxDraft {
    pub id: String,
    pub target: String,
    pub title: String,
    #[serde(rename = "proposedContent")]
    pub proposed_content: String,
    pub reason: String,
    pub author: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<InboxChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default, rename = "targetRevisions", skip_serializing_if = "Vec::is_empty")]
    pub target_revisions: Vec<RevisionExpectation>,
    #[serde(default, rename = "updatedAt", skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

fn default_proposal_mode() -> String {
    PROPOSAL_MODE_APPEND.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct InboxProposal {
    pub id: String,
    pub target: String,
    pub title: String,
    #[serde(rename = "proposedContent")]
    pub proposed_content: String,
    pub reason: String,
    pub author: String,
    /// Lifecycle state: pending, approved, rejected, withdrawn or stale.
    pub status: String,
    /// How legacy content is applied to an existing target: `append` (default) or `replace`.
    #[serde(default = "default_proposal_mode")]
    pub mode: String,
    #[serde(default, rename = "decisionReason", skip_serializing_if = "Option::is_none")]
    pub decision_reason: Option<String>,
    #[serde(default, rename = "decisionBy", skip_serializing_if = "Option::is_none")]
    pub decision_by: Option<String>,
    #[serde(default, rename = "decidedAt", skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<InboxChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<EvidenceRef>,
    /// Target revision captured at publication; approval refuses once it drifts.
    #[serde(default, rename = "targetRevisions", skip_serializing_if = "Vec::is_empty")]
    pub target_revisions: Vec<RevisionExpectation>,
    /// Entity id created by a `*.create` change.
    #[serde(default, rename = "entityId", skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(default, rename = "selfApproved", skip_serializing_if = "std::ops::Not::not")]
    pub self_approved: bool,
    #[serde(default, rename = "staleReason", skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
    /// Members who repaired this proposal's content after it went stale. Each
    /// repairer counts as an author for the self-approval guard.
    #[serde(default, rename = "repairedBy", skip_serializing_if = "Vec::is_empty")]
    pub repaired_by: Vec<String>,
}

impl InboxProposal {
    /// True when `member` authored the proposal or contributed content to it
    /// through a repair.
    pub fn is_contributor(&self, member: &str) -> bool {
        self.author == member || self.repaired_by.iter().any(|m| m == member)
    }

    pub fn change_kind(&self) -> String {
        match &self.change {
            Some(c) => c.kind().to_string(),
            None => format!("content.{}", self.mode),
        }
    }
}

pub fn inbox_drafts_dir(config: &KnobyteConfig) -> PathBuf {
    config.local_dir().join("inbox_drafts")
}

/// Validate a proposal mode, returning the canonical lowercase form.
pub fn normalize_proposal_mode(mode: &str) -> Result<String, String> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "" | PROPOSAL_MODE_APPEND => Ok(PROPOSAL_MODE_APPEND.to_string()),
        PROPOSAL_MODE_REPLACE => Ok(PROPOSAL_MODE_REPLACE.to_string()),
        other => Err(format!("Invalid proposal mode '{}': expected 'append' or 'replace'", other)),
    }
}

/// Normalize and validate a proposal target, returning a path relative to the
/// scaffold root (e.g. `context/rate-limit.md`).
///
/// * a leading `.knobyte/` (or `./`) is stripped,
/// * a bare name without a directory is placed under `context/`,
/// * a missing extension becomes `.md`,
/// * absolute paths, `..` components, `local/` and non-Markdown targets are refused.
pub fn normalize_proposal_target(target: &str) -> Result<String, String> {
    let trimmed = target.trim().replace('\\', "/");
    if trimmed.is_empty() {
        return Err("Proposal target must not be empty".to_string());
    }
    if trimmed.starts_with('/') || Path::new(&trimmed).is_absolute() || trimmed.contains(':') {
        return Err(format!("Proposal target '{}' must be a path relative to the .knobyte/ scaffold", target));
    }

    let mut rel = trimmed.as_str();
    loop {
        if let Some(rest) = rel.strip_prefix("./") {
            rel = rest;
        } else if let Some(rest) = rel.strip_prefix(".knobyte/") {
            rel = rest;
        } else {
            break;
        }
    }

    let mut parts: Vec<&str> = Vec::new();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(p) => parts.push(p.to_str().ok_or_else(|| format!("Proposal target '{}' is not valid UTF-8", target))?),
            Component::CurDir => {}
            _ => return Err(format!("Proposal target '{}' must not contain '..' or absolute components", target)),
        }
    }
    if parts.is_empty() {
        return Err(format!("Proposal target '{}' is empty", target));
    }
    if parts.iter().any(|p| p.starts_with('.')) {
        return Err(format!("Proposal target '{}' must not reference hidden files or directories", target));
    }
    if parts[0] == "local" {
        return Err(format!("Proposal target '{}' points into checkout-local state (local/)", target));
    }

    let mut normalized = if parts.len() == 1 { format!("context/{}", parts[0]) } else { parts.join("/") };

    match Path::new(&normalized).extension().and_then(|e| e.to_str()) {
        None => normalized.push_str(".md"),
        Some(ext) if ext.eq_ignore_ascii_case("md") => {}
        Some(ext) => return Err(format!("Proposal target '{}' must be a Markdown document (got .{})", target, ext)),
    }

    Ok(normalized)
}

/// Resolve a proposal target to an absolute path inside the scaffold, refusing
/// anything that would escape it (including via symlinked directories).
fn resolve_target_path(config: &KnobyteConfig, target: &str) -> Result<(String, PathBuf), String> {
    let rel = normalize_proposal_target(target)?;
    let path = config.scaffold_root.join(&rel);
    if path.is_dir() {
        return Err(format!("Proposal target '{}' is a directory", rel));
    }
    let scaffold_canon = config.scaffold_root.canonicalize().map_err(|e| format!("Scaffold root not accessible: {}", e))?;
    let mut probe = path.as_path();
    loop {
        if probe.exists() {
            let canon = probe.canonicalize().map_err(|e| e.to_string())?;
            if !canon.starts_with(&scaffold_canon) {
                return Err(format!("Proposal target '{}' escapes the .knobyte/ scaffold", rel));
            }
            break;
        }
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break,
        }
    }
    Ok((rel, path))
}

fn draft_path(config: &KnobyteConfig, id: &str) -> Result<PathBuf, String> {
    validate_entity_id(id)?;
    Ok(inbox_drafts_dir(config).join(format!("{}.json", id)))
}

fn proposal_path(config: &KnobyteConfig, id: &str) -> Result<PathBuf, String> {
    validate_entity_id(id)?;
    Ok(config.inbox_dir().join(format!("{}.json", id)))
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

pub fn list_inbox_drafts(config: &KnobyteConfig) -> Vec<InboxDraft> {
    let mut list: Vec<InboxDraft> = load_dir(inbox_drafts_dir(config));
    list.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| a.id.cmp(&b.id)));
    list
}

pub fn list_inbox_drafts_page(config: &KnobyteConfig, cursor: Option<&str>, limit: Option<usize>) -> Result<Page<InboxDraft>, TeamError> {
    paginate(list_inbox_drafts(config), |d| d.id.clone(), "inbox-drafts", cursor, limit)
}

pub fn get_inbox_draft(config: &KnobyteConfig, id: &str) -> Option<InboxDraft> {
    read_json_file(&draft_path(config, id).ok()?)
}

/// Write a legacy content draft directly (atomic).
pub fn save_inbox_draft(config: &KnobyteConfig, draft: &InboxDraft) -> Result<(), String> {
    let path = draft_path(config, &draft.id)?;
    crate::team::store::write_json_atomic(&path, draft)
}

/// Save an inbox draft together with the mode (`append` or `replace`) that
/// will be used when the resulting proposal is approved.
pub fn save_inbox_draft_with_mode(config: &KnobyteConfig, draft: &InboxDraft, mode: &str) -> Result<(), String> {
    let mode = normalize_proposal_mode(mode)?;
    let mut d = draft.clone();
    d.mode = Some(mode);
    save_inbox_draft(config, &d)
}

pub fn delete_inbox_draft(config: &KnobyteConfig, id: &str) -> Result<(), String> {
    let path = draft_path(config, id)?;
    crate::team::store::remove_if_exists(&path).map_err(|e| e.to_string())
}

pub fn list_inbox_proposals(config: &KnobyteConfig) -> Vec<InboxProposal> {
    let mut list: Vec<InboxProposal> = load_dir(config.inbox_dir());
    list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then_with(|| a.id.cmp(&b.id)));
    list
}

/// Paged proposal listing filtered by any of `states`.
pub fn list_inbox_proposals_page(
    config: &KnobyteConfig,
    states: &[String],
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<Page<InboxProposal>, TeamError> {
    for s in states {
        if !PROPOSAL_STATES.contains(&s.as_str()) {
            return Err(TeamError::usage(format!("--state must be one of: {}", PROPOSAL_STATES.join(", "))));
        }
    }
    let mut all = list_inbox_proposals(config);
    if !states.is_empty() {
        all.retain(|p| states.contains(&p.status));
    }
    paginate(all, |p| p.id.clone(), &format!("states={:?}", states), cursor, limit)
}

/// Load a single published proposal by id.
pub fn get_proposal(config: &KnobyteConfig, proposal_id: &str) -> Result<InboxProposal, String> {
    let path = proposal_path(config, proposal_id)?;
    if !path.exists() {
        return Err(format!("Proposal '{}' not found", proposal_id));
    }
    let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    serde_json::from_str::<InboxProposal>(&content).map_err(|e| format!("Proposal '{}' is malformed: {}", proposal_id, e))
}

// ---------------------------------------------------------------------------
// Knowledge entity lookup
// ---------------------------------------------------------------------------

/// A knowledge record located in the scaffold.
#[derive(Debug, Clone)]
pub struct LocatedEntity {
    pub rel: String,
    pub entity: WikiEntity,
    pub content: String,
}

fn scan_entities(config: &KnobyteConfig) -> Vec<LocatedEntity> {
    let mut out = Vec::new();
    let local = config.local_dir();
    for entry in WalkDir::new(&config.scaffold_root).into_iter().filter_entry(|e| e.path() != local).filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let rel = crate::team::workflow::rel_path(config, path);
        if rel.split('/').any(|p| p.starts_with('.')) {
            continue;
        }
        if let Ok(content) = fs::read_to_string(path) {
            if let Some(entity) = parse_markdown_entity(&rel, &content) {
                out.push(LocatedEntity { rel, entity, content });
            }
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    out
}

/// Find a knowledge entity by frontmatter id (or scaffold-relative path).
pub fn find_entity(config: &KnobyteConfig, id: &str) -> Option<LocatedEntity> {
    let wanted = id.trim().trim_start_matches(".knobyte/");
    scan_entities(config).into_iter().find(|e| e.entity.id == wanted || e.rel == wanted)
}

/// Map an entity's stored status to a lifecycle state.
pub fn lifecycle_of(status: &str) -> String {
    match status.to_ascii_lowercase().as_str() {
        "in_flight" | "draft" | "proposed" | "in-flight" | "pending" => "in_flight".to_string(),
        "deprecated" | "superseded" => "deprecated".to_string(),
        "archived" => "archived".to_string(),
        _ => "promoted".to_string(),
    }
}

/// `inbox target <id>`: the exact current record and revision a correction must bind to.
pub fn inbox_target(config: &KnobyteConfig, id: &str) -> Result<Value, TeamError> {
    let found = find_entity(config, id).ok_or_else(|| TeamError::not_found("Knowledge target", id))?;
    let kind = found.entity.entity_type.clone();
    if !KNOWLEDGE_KINDS.contains(&kind.as_str()) && !SPEC_KINDS.contains(&kind.as_str()) && kind != "topic" {
        return Err(TeamError::usage(format!("Entity kind '{}' is not supported by Inbox", kind)));
    }
    let data = json!({
        "target": { "id": found.entity.id, "kind": kind, "title": found.entity.title },
        "version": {
            "semanticRevision": found.entity.revision,
            "contentHash": crate::team::store::revision_of(found.content.as_bytes()),
        },
        "sourcePath": found.rel,
        "lifecycleState": lifecycle_of(&found.entity.status),
        "summary": found.entity.summary,
        "body": found.entity.body,
    });
    if serde_json::to_string(&data).map(|s| s.len()).unwrap_or(0) > 64 * 1024 {
        return Err(TeamError::usage("The target exceeds the 64 KiB Inbox read bound"));
    }
    Ok(data)
}

// ---------------------------------------------------------------------------
// Frontmatter rendering
// ---------------------------------------------------------------------------

fn split_frontmatter(content: &str) -> (Option<serde_yaml::Mapping>, String) {
    let normalized = content.replace("\r\n", "\n");
    if let Some(rest) = normalized.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            let yaml = &rest[..end];
            let after = &rest[end + 4..];
            let body = after.strip_prefix('\n').unwrap_or(after).trim_start_matches('\n').to_string();
            if let Ok(serde_yaml::Value::Mapping(m)) = serde_yaml::from_str::<serde_yaml::Value>(yaml) {
                return (Some(m), body);
            }
            return (Some(serde_yaml::Mapping::new()), body);
        }
    }
    (None, normalized)
}

fn render_markdown(fm: &serde_yaml::Mapping, body: &str) -> Result<String, TeamError> {
    let yaml = serde_yaml::to_string(fm).map_err(|e| TeamError::internal(e.to_string()))?;
    let mut out = format!("---\n{}---\n\n{}", yaml, body.trim_end());
    out.push('\n');
    Ok(out)
}

fn ystr(s: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(s.to_string())
}

fn slugify(title: &str) -> String {
    let mut s = String::new();
    for c in title.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.is_empty() && !s.ends_with('-') {
            s.push('-');
        }
    }
    let s: String = s.trim_end_matches('-').chars().take(48).collect();
    if s.is_empty() { "entry".to_string() } else { s.trim_end_matches('-').to_string() }
}

fn dir_for_kind(kind: &str) -> &'static str {
    match kind {
        "pattern" => "patterns",
        k if SPEC_KINDS.contains(&k) => "specs",
        _ => "context",
    }
}

// ---------------------------------------------------------------------------
// Draft input normalization
// ---------------------------------------------------------------------------

/// Caller-owned inbox draft content: either a typed `change`, or a legacy
/// `target` + `content` (+ `mode`) Markdown edit.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct InboxDraftInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<InboxChange>,
    pub rationale: String,
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    #[serde(default, rename = "targetRevisions")]
    pub target_revisions: Vec<RevisionExpectation>,
}

fn bounded(label: &str, v: &str, max: usize, required: bool) -> Result<String, TeamError> {
    let t = v.trim();
    if required && t.is_empty() {
        return Err(TeamError::validation(format!("{} must not be empty", label)));
    }
    if t.len() > max {
        return Err(TeamError::validation(format!("{} exceeds {} bytes", label, max)));
    }
    Ok(t.to_string())
}

fn validate_patch(p: &ContentPatch) -> Result<(), TeamError> {
    if p.title.is_none() && p.summary.is_none() && p.body.is_none() {
        return Err(TeamError::validation("An update patch must change the title, summary or body"));
    }
    if let Some(t) = &p.title {
        bounded("title", t, MAX_TITLE, true)?;
    }
    if let Some(s) = &p.summary {
        bounded("summary", s, MAX_SUMMARY, false)?;
    }
    if let Some(b) = &p.body {
        bounded("body", b, MAX_BODY, true)?;
    }
    Ok(())
}

fn allowed_relation(entity_kind: &str, rel: &str) -> Option<&'static [&'static str]> {
    match (entity_kind, rel) {
        (_, "constrained_by") => Some(&["constraint"]),
        ("requirement", "derived_from") => Some(&["spec"]),
        ("requirement", "refines") => Some(&["requirement"]),
        ("acceptance_criterion", "verified_by") => Some(&["requirement", "spec"]),
        _ => None,
    }
}

fn validate_change(change: &InboxChange) -> Result<(), TeamError> {
    match change {
        InboxChange::KnowledgeCreate { entity_kind, title, body, summary, status, topics }
        | InboxChange::SpecCreate { entity_kind, title, body, summary, status, topics, .. } => {
            let kinds = if change.is_spec() { SPEC_KINDS } else { KNOWLEDGE_KINDS };
            if !kinds.contains(&entity_kind.as_str()) {
                return Err(TeamError::validation(format!("{} entityKind must be one of: {}", change.kind(), kinds.join(", "))));
            }
            bounded("title", title, MAX_TITLE, true)?;
            bounded("body", body, MAX_BODY, true)?;
            if let Some(s) = summary {
                bounded("summary", s, MAX_SUMMARY, false)?;
            }
            if let Some(s) = status {
                if !ENTITY_LIFECYCLES.contains(&s.as_str()) {
                    return Err(TeamError::validation("status must be 'in_flight' or 'promoted'"));
                }
            }
            if topics.len() > MAX_TOPICS {
                return Err(TeamError::validation(format!("At most {} topics are allowed", MAX_TOPICS)));
            }
            if let InboxChange::SpecCreate { relation: Some(r), entity_kind, .. } = change {
                if allowed_relation(entity_kind, &r.rel_type).is_none() {
                    return Err(TeamError::validation(format!("A {} cannot carry a '{}' relation", entity_kind, r.rel_type)));
                }
                validate_entity_id(&r.target.id).map_err(TeamError::validation)?;
            }
        }
        InboxChange::KnowledgeUpdate { target, patch } | InboxChange::SpecUpdate { target, patch } => {
            if target.id.trim().is_empty() {
                return Err(TeamError::validation("An update change requires target.id"));
            }
            validate_patch(patch)?;
        }
    }
    Ok(())
}

/// The file a draft/proposal writes and the expectation captured at publication.
struct ResolvedTarget {
    rel: String,
    expectation: RevisionExpectation,
    entity_id: Option<String>,
    title: String,
}

fn resolve_change_target(config: &KnobyteConfig, change: &InboxChange, plan: &mut Plan) -> Result<ResolvedTarget, TeamError> {
    match change {
        InboxChange::KnowledgeCreate { entity_kind, title, .. } | InboxChange::SpecCreate { entity_kind, title, .. } => {
            if let InboxChange::SpecCreate { relation: Some(r), entity_kind, .. } = change {
                let allowed = allowed_relation(entity_kind, &r.rel_type).unwrap_or(&[]);
                let target = find_entity(config, &r.target.id).ok_or_else(|| TeamError::not_found("Relation target", &r.target.id))?;
                if !allowed.contains(&target.entity.entity_type.as_str()) {
                    return Err(TeamError::validation(format!(
                        "Relation '{}' must point at a {} (found {})",
                        r.rel_type,
                        allowed.join(" or "),
                        target.entity.entity_type
                    )));
                }
                plan.read(target.rel.clone());
            }
            let slug = slugify(title);
            let dir = dir_for_kind(entity_kind);
            let existing: Vec<String> = scan_entities(config).into_iter().map(|e| e.entity.id).collect();
            let mut n = 1;
            let (rel, id) = loop {
                let suffix = if n == 1 { String::new() } else { format!("-{}", n) };
                let rel = format!("{}/{}{}.md", dir, slug, suffix);
                let id = format!("kb_{}{}", slug.replace('-', "_"), suffix.replace('-', "_"));
                if !config.scaffold_root.join(&rel).exists() && !existing.contains(&id) {
                    break (rel, id);
                }
                n += 1;
            };
            Ok(ResolvedTarget { expectation: RevisionExpectation { path: rel.clone(), revision: None }, rel, entity_id: Some(id), title: title.trim().to_string() })
        }
        InboxChange::KnowledgeUpdate { target, patch } | InboxChange::SpecUpdate { target, patch } => {
            let found = find_entity(config, &target.id).ok_or_else(|| TeamError::not_found("Knowledge target", &target.id))?;
            let kind = found.entity.entity_type.as_str();
            let kinds = if change.is_spec() { SPEC_KINDS } else { KNOWLEDGE_KINDS };
            if !kinds.contains(&kind) && (kind != "topic" || change.is_spec()) {
                return Err(TeamError::validation(format!(
                    "{} cannot target a '{}' entity; use {}",
                    change.kind(),
                    kind,
                    if change.is_spec() { "knowledge.update" } else { "spec.update" }
                )));
            }
            if let Some(k) = &target.kind {
                if k != kind {
                    return Err(TeamError::conflict(format!("Target {} is a '{}', not a '{}'", target.id, kind, k)));
                }
            }
            let rev = crate::team::store::revision_of(found.content.as_bytes());
            Ok(ResolvedTarget {
                rel: found.rel.clone(),
                expectation: RevisionExpectation { path: found.rel.clone(), revision: Some(rev) },
                entity_id: Some(found.entity.id.clone()),
                title: patch.title.clone().unwrap_or_else(|| found.entity.title.clone()),
            })
        }
    }
}

/// Normalize a draft input into the stored draft fields (target path, title, content).
fn normalize_draft_input(config: &KnobyteConfig, input: InboxDraftInput, plan: &mut Plan) -> Result<(InboxDraftInput, String, String, String), TeamError> {
    let rationale = bounded("rationale", &input.rationale, MAX_RATIONALE, true)?;
    if input.evidence.len() > 64 || input.target_revisions.len() > 64 {
        return Err(TeamError::validation("At most 64 evidence references and target revisions are allowed"));
    }
    match (&input.change, &input.target) {
        (Some(_), Some(_)) | (Some(_), None) if input.content.is_some() => {
            Err(TeamError::usage("Give either a typed change or target+content, not both"))
        }
        (Some(change), _) => {
            validate_change(change)?;
            let resolved = resolve_change_target(config, change, plan)?;
            let content = match change {
                InboxChange::KnowledgeCreate { body, .. } | InboxChange::SpecCreate { body, .. } => body.clone(),
                InboxChange::KnowledgeUpdate { patch, .. } | InboxChange::SpecUpdate { patch, .. } => patch.body.clone().unwrap_or_default(),
            };
            let title = input.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or(resolved.title);
            Ok((InboxDraftInput { rationale, target: None, content: None, mode: None, ..input }, resolved.rel, bounded("title", &title, MAX_TITLE, true)?, content))
        }
        (None, Some(target)) => {
            let target = normalize_proposal_target(target).map_err(|e| TeamError::new(ErrorCode::PathOutsideProject, "Invalid target", e))?;
            let content = input.content.clone().ok_or_else(|| TeamError::usage("A content draft requires 'content'"))?;
            if content.trim().is_empty() || content.len() > MAX_BODY * 4 {
                return Err(TeamError::validation("Proposal content must be 1-65536 bytes"));
            }
            let mode = normalize_proposal_mode(input.mode.as_deref().unwrap_or("")).map_err(TeamError::validation)?;
            let title = bounded("title", input.title.as_deref().unwrap_or(""), MAX_TITLE, true)?;
            Ok((InboxDraftInput { rationale, mode: Some(mode), ..input }, target, title, content))
        }
        (None, None) => Err(TeamError::usage("A draft needs a typed 'change' or a 'target' with 'content'")),
    }
}

// ---------------------------------------------------------------------------
// Workflow planners
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftSaveAction {
    #[serde(default, rename = "draftId")]
    draft_id: Option<String>,
    draft: InboxDraftInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftIdAction {
    #[serde(rename = "draftId")]
    draft_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalAction {
    #[serde(rename = "proposalId")]
    proposal_id: String,
    #[serde(default)]
    rationale: Option<String>,
    #[serde(default, rename = "selfApprove")]
    self_approve: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairAction {
    #[serde(rename = "proposalId")]
    proposal_id: String,
    replacement: InboxDraftInput,
}

fn draft_rel(ctx: &Ctx, id: &str) -> Result<String, TeamError> {
    Ok(ctx.rel(&draft_path(ctx.config, id).map_err(TeamError::usage)?))
}

fn proposal_rel(ctx: &Ctx, id: &str) -> Result<String, TeamError> {
    Ok(ctx.rel(&proposal_path(ctx.config, id).map_err(TeamError::usage)?))
}

fn require_draft(ctx: &Ctx, id: &str) -> Result<InboxDraft, TeamError> {
    validate_entity_id(id).map_err(TeamError::usage)?;
    get_inbox_draft(ctx.config, id).ok_or_else(|| TeamError::not_found("Inbox draft", id))
}

fn require_proposal(ctx: &Ctx, id: &str) -> Result<InboxProposal, TeamError> {
    validate_entity_id(id).map_err(TeamError::usage)?;
    let path = proposal_path(ctx.config, id).map_err(TeamError::usage)?;
    if !path.exists() {
        return Err(TeamError::not_found("Proposal", id));
    }
    get_proposal(ctx.config, id).map_err(TeamError::validation)
}

/// Draft fields as an input (for re-normalizing at publish time).
fn draft_as_input(d: &InboxDraft) -> InboxDraftInput {
    InboxDraftInput {
        title: Some(d.title.clone()),
        target: if d.change.is_some() { None } else { Some(d.target.clone()) },
        content: if d.change.is_some() { None } else { Some(d.proposed_content.clone()) },
        mode: if d.change.is_some() { None } else { d.mode.clone() },
        change: d.change.clone(),
        rationale: d.reason.clone(),
        evidence: d.evidence.clone(),
        target_revisions: d.target_revisions.clone(),
    }
}

/// Capture the publication-time expectation for a draft and check caller pins.
fn capture_expectations(ctx: &Ctx, input: &InboxDraftInput, target_rel: &str, plan: &mut Plan) -> Result<(Vec<RevisionExpectation>, Option<String>), TeamError> {
    let (expectation, entity_id) = match &input.change {
        Some(change) => {
            let r = resolve_change_target(ctx.config, change, plan)?;
            (r.expectation, r.entity_id)
        }
        None => {
            let (rel, abs) = resolve_target_path(ctx.config, target_rel).map_err(|e| TeamError::new(ErrorCode::PathOutsideProject, "Invalid target", e))?;
            (RevisionExpectation { path: rel, revision: file_revision(&abs) }, None)
        }
    };
    for pin in &input.target_revisions {
        if pin.path == expectation.path && pin.revision != expectation.revision {
            return Err(TeamError::conflict(format!(
                "{} changed since it was read for this draft; run `knobyte inbox target` again and repair the draft",
                pin.path
            )));
        }
    }
    Ok((vec![expectation], entity_id))
}

/// True when any captured target revision differs from the current file.
fn proposal_drifted(config: &KnobyteConfig, p: &InboxProposal) -> Option<String> {
    for e in &p.target_revisions {
        let current = file_revision(&config.scaffold_root.join(&e.path));
        if current != e.revision {
            return Some(e.path.clone());
        }
    }
    None
}

fn decision_event(ctx: &mut Ctx, p: &InboxProposal, verb: &str, note: Option<&str>) -> Result<JournalAppend, TeamError> {
    let id = ctx.ids.id("event", || Uuid::new_v4().to_string())?;
    let entry = EventEntry {
        id: id.clone(),
        timestamp: ctx.now(),
        kind: "decision".to_string(),
        summary: format!("{} inbox proposal '{}' for {}", verb, p.title, p.target),
        details: note.map(|n| n.to_string()),
        tags: vec!["inbox".to_string(), verb.to_ascii_lowercase()],
        files: vec![format!(".knobyte/{}", p.target)],
        actor: Some(ctx.actor_id()),
        confidence: Some(1.0),
        provenance: Some(format!("inbox:{}", p.id)),
        source: Some("inbox".to_string()),
        status: Some(verb.to_ascii_lowercase()),
        ..Default::default()
    };
    Ok(JournalAppend {
        path: ctx.rel(&ctx.config.decisions_log_path()),
        line: serde_json::to_string(&entry).map_err(|e| TeamError::internal(e.to_string()))?,
        marker: Some(format!("\"id\":\"{}\"", id)),
    })
}

/// Compute the new content of the approval target.
/// `YYYY-MM-DD` of the operation, for the `last_updated` frontmatter field the drift
/// checkers require on knowledge files.
fn frontmatter_date(ctx: &Ctx) -> String {
    ctx.now().chars().take(10).collect()
}

/// Set (or add) the frontmatter `last_updated` of `content` to `date`; content without
/// frontmatter is returned unchanged.
fn bump_last_updated(content: &str, date: &str) -> String {
    let eol = if content.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = content.split(eol).collect();
    if lines.first() != Some(&"---") {
        return content.to_string();
    }
    let Some(close) = lines.iter().skip(1).position(|l| *l == "---").map(|i| i + 1) else {
        return content.to_string();
    };
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    match (1..close).find(|i| lines[*i].starts_with("last_updated:")) {
        Some(i) => out[i] = format!("last_updated: {}", date),
        None => out.insert(close, format!("last_updated: {}", date)),
    }
    out.join(eol)
}

/// A free-text proposal approved onto a path that does not exist yet becomes a proper wiki
/// entity: frontmatter with a fresh `kb_<slug>` id, the proposal title, a type inferred from
/// the path, `in_flight` status, the reason as summary, `last_updated` and revision 1. Keys
/// the proposed content already declares in its own frontmatter are kept.
fn new_entity_file(ctx: &Ctx, p: &InboxProposal, rel: &str, content: &str) -> Result<String, TeamError> {
    let (own, body) = split_frontmatter(content);
    let mut fm = serde_yaml::Mapping::new();
    let own = own.unwrap_or_default();
    let has = |k: &str| own.get(ystr(k)).is_some_and(|v| !v.is_null());
    if !has("id") {
        let stem = rel.rsplit('/').next().unwrap_or(rel).trim_end_matches(".md");
        let base = format!("kb_{}", slugify(stem).replace('-', "_"));
        let existing: Vec<String> = scan_entities(ctx.config).into_iter().map(|e| e.entity.id).collect();
        let mut id = base.clone();
        let mut n = 2;
        while existing.contains(&id) {
            id = format!("{}_{}", base, n);
            n += 1;
        }
        fm.insert(ystr("id"), ystr(&id));
    }
    if !has("title") && !has("name") {
        fm.insert(ystr("title"), ystr(p.title.trim()));
    }
    if !has("type") {
        let inferred = crate::wiki::parser::infer_type_from_path(rel);
        let t = if inferred == "document" { crate::wiki::migrate::DOCUMENT_REPLACEMENT_TYPE.to_string() } else { inferred };
        fm.insert(ystr("type"), ystr(&t));
    }
    if !has("status") {
        fm.insert(ystr("status"), ystr("in_flight"));
    }
    let reason = p.reason.trim();
    if !has("summary") && !has("description") && !reason.is_empty() {
        fm.insert(ystr("summary"), ystr(reason));
    }
    if !has("revision") {
        fm.insert(ystr("revision"), serde_yaml::Value::Number(1.into()));
    }
    for (k, v) in own {
        if k.as_str() != Some("last_updated") {
            fm.insert(k, v);
        }
    }
    fm.insert(ystr("last_updated"), ystr(&frontmatter_date(ctx)));
    fm.insert(ystr("source_proposal"), ystr(&p.id));
    render_markdown(&fm, &body)
}

fn approved_content(ctx: &Ctx, p: &InboxProposal) -> Result<(String, String), TeamError> {
    match &p.change {
        None => {
            let (rel, abs) = resolve_target_path(ctx.config, &p.target).map_err(|e| TeamError::new(ErrorCode::PathOutsideProject, "Invalid target", e))?;
            let mode = normalize_proposal_mode(&p.mode).map_err(TeamError::validation)?;
            let mut body = p.proposed_content.clone();
            if !body.ends_with('\n') {
                body.push('\n');
            }
            if !abs.exists() {
                return Ok((rel.clone(), new_entity_file(ctx, p, &rel, &body)?));
            }
            let content = if mode == PROPOSAL_MODE_APPEND {
                let mut existing = fs::read_to_string(&abs).map_err(|e| TeamError::internal(e.to_string()))?;
                if !existing.is_empty() {
                    if !existing.ends_with('\n') {
                        existing.push('\n');
                    }
                    existing.push('\n');
                }
                existing.push_str(&body);
                existing
            } else {
                body
            };
            Ok((rel, bump_last_updated(&content, &frontmatter_date(ctx))))
        }
        Some(InboxChange::KnowledgeCreate { entity_kind, title, body, summary, status, topics })
        | Some(InboxChange::SpecCreate { entity_kind, title, body, summary, status, topics, .. }) => {
            let rel = p.target.clone();
            normalize_proposal_target(&rel).map_err(TeamError::validation)?;
            if ctx.config.scaffold_root.join(&rel).exists() {
                return Err(TeamError::conflict(format!("{} already exists; mark the proposal stale and repair it", rel)));
            }
            let id = p.entity_id.clone().ok_or_else(|| TeamError::validation("Create proposal has no entity id"))?;
            let mut fm = serde_yaml::Mapping::new();
            fm.insert(ystr("id"), ystr(&id));
            fm.insert(ystr("title"), ystr(title.trim()));
            fm.insert(ystr("type"), ystr(entity_kind));
            if let Some(s) = summary.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                fm.insert(ystr("summary"), ystr(s));
            }
            fm.insert(ystr("status"), ystr(status.as_deref().unwrap_or("in_flight")));
            fm.insert(ystr("revision"), serde_yaml::Value::Number(1.into()));
            fm.insert(ystr("last_updated"), ystr(&frontmatter_date(ctx)));
            if !topics.is_empty() {
                fm.insert(ystr("topics"), serde_yaml::Value::Sequence(topics.iter().map(|t| ystr(t)).collect()));
            }
            if let Some(InboxChange::SpecCreate { relation: Some(r), .. }) = &p.change {
                let mut rel_map = serde_yaml::Mapping::new();
                rel_map.insert(ystr("type"), ystr(&r.rel_type));
                rel_map.insert(ystr("target_id"), ystr(&r.target.id));
                fm.insert(ystr("relations"), serde_yaml::Value::Sequence(vec![serde_yaml::Value::Mapping(rel_map)]));
            }
            fm.insert(ystr("source_proposal"), ystr(&p.id));
            Ok((rel, render_markdown(&fm, body)?))
        }
        Some(InboxChange::KnowledgeUpdate { patch, .. }) | Some(InboxChange::SpecUpdate { patch, .. }) => {
            let rel = p.target.clone();
            let (_, abs) = resolve_target_path(ctx.config, &rel).map_err(|e| TeamError::new(ErrorCode::PathOutsideProject, "Invalid target", e))?;
            let current = fs::read_to_string(&abs).map_err(|_| TeamError::not_found("Knowledge target", &rel))?;
            let (fm, body) = split_frontmatter(&current);
            let mut fm = fm.unwrap_or_default();
            if let Some(t) = &patch.title {
                fm.insert(ystr("title"), ystr(t.trim()));
            }
            if let Some(s) = &patch.summary {
                fm.insert(ystr("summary"), ystr(s.trim()));
            }
            let revision = fm.get(ystr("revision")).and_then(|v| v.as_i64()).unwrap_or(1);
            fm.insert(ystr("revision"), serde_yaml::Value::Number((revision + 1).into()));
            fm.insert(ystr("last_updated"), ystr(&frontmatter_date(ctx)));
            let body = patch.body.clone().unwrap_or(body);
            Ok((rel, render_markdown(&fm, &body)?))
        }
    }
}

pub(crate) fn plan(ctx: &mut Ctx, kind: &str, action: &Value) -> Result<Plan, TeamError> {
    let mut plan = Plan::default();
    match kind {
        "inbox.draft.save" => {
            let DraftSaveAction { draft_id, draft } = parse_action(action)?;
            let existing = match &draft_id {
                Some(id) => Some(require_draft(ctx, id)?),
                None => None,
            };
            let (input, target, title, content) = normalize_draft_input(ctx.config, draft, &mut plan)?;
            let id = match draft_id {
                Some(id) => id,
                None => ctx.ids.id("inbox-draft", || format!("draft_{}", Uuid::new_v4()))?,
            };
            let d = InboxDraft {
                id: id.clone(),
                target,
                title,
                proposed_content: content,
                reason: input.rationale.clone(),
                author: existing.as_ref().map(|e| e.author.clone()).unwrap_or_else(|| ctx.actor_id()),
                created_at: existing.as_ref().map(|e| e.created_at.clone()).unwrap_or_else(|| ctx.now()),
                mode: input.mode.clone(),
                change: input.change.clone(),
                evidence: input.evidence.clone(),
                target_revisions: input.target_revisions.clone(),
                updated_at: Some(ctx.now()),
            };
            plan.write_json(draft_rel(ctx, &id)?, "local", &d, "Save local inbox draft")?;
            plan.summary = format!("Save inbox draft '{}'", id);
            plan.result = json!(d);
        }
        "inbox.draft.delete" => {
            let DraftIdAction { draft_id } = parse_action(action)?;
            require_draft(ctx, &draft_id)?;
            plan.delete(draft_rel(ctx, &draft_id)?, "local", "Delete local inbox draft");
            plan.summary = format!("Delete inbox draft '{}'", draft_id);
            plan.result = json!({ "id": draft_id });
        }
        "inbox.publish" => {
            let DraftIdAction { draft_id } = parse_action(action)?;
            let d = require_draft(ctx, &draft_id)?;
            let (input, target, title, content) = normalize_draft_input(ctx.config, draft_as_input(&d), &mut plan)?;
            let (target_revisions, entity_id) = capture_expectations(ctx, &input, &target, &mut plan)?;
            let id = ctx.ids.id("proposal", || format!("prop_{}", Uuid::new_v4()))?;
            let author = if d.author.trim().is_empty() || d.author == "engineer" { ctx.actor_id() } else { d.author.clone() };
            let p = InboxProposal {
                id: id.clone(),
                target,
                title,
                proposed_content: content,
                reason: input.rationale.clone(),
                author,
                status: PROPOSAL_STATUS_PENDING.to_string(),
                mode: input.mode.clone().unwrap_or_else(default_proposal_mode),
                decision_reason: None,
                decision_by: None,
                decided_at: None,
                created_at: ctx.now(),
                updated_at: ctx.now(),
                change: input.change.clone(),
                evidence: input.evidence.clone(),
                target_revisions,
                entity_id,
                self_approved: false,
                stale_reason: None,
                repaired_by: Vec::new(),
            };
            plan.write_json(proposal_rel(ctx, &id)?, "canonical", &p, "Publish inbox proposal")?;
            plan.delete(draft_rel(ctx, &draft_id)?, "local", "Remove published inbox draft");
            let actor = Some(p.author.clone());
            ctx.activity(
                &mut plan,
                actor,
                "inbox.publish",
                "inbox_proposal",
                &p.id,
                &p.title,
                format!("Published inbox proposal '{}' for {}", p.title, p.target),
                crate::team::workflow::meta(&[("target", json!(p.target)), ("mode", json!(p.mode)), ("changeKind", json!(p.change_kind()))]),
                vec![ActivitySubject::entity("proposal", &p.id, Some(&p.title))],
                None,
            )?;
            plan.summary = format!("Publish proposal '{}' for {}", p.title, p.target);
            plan.result = json!(p);
        }
        "inbox.approve" | "inbox.reject" | "inbox.withdraw" | "inbox.mark-stale" => {
            let ProposalAction { proposal_id, rationale, self_approve } = parse_action(action)?;
            let rationale = rationale.map(|r| r.trim().to_string()).filter(|r| !r.is_empty());
            if let Some(r) = &rationale {
                bounded("rationale", r, MAX_RATIONALE, false)?;
            }
            let mut p = require_proposal(ctx, &proposal_id)?;
            let actor = ctx.actor_id();
            if p.status != PROPOSAL_STATUS_PENDING {
                return Err(TeamError::validation(format!(
                    "Proposal '{}' is not pending (status: {}); only a pending proposal can be reviewed, withdrawn or marked stale",
                    proposal_id, p.status
                )));
            }
            // Repairers rewrote the content, so they count as authors for review.
            let is_author = p.is_contributor(&actor);
            match kind {
                "inbox.approve" => {
                    ctx.require_active_member("review inbox proposals")?;
                    if is_author && !self_approve {
                        return Err(TeamError::new(
                            crate::team::envelope::ErrorCode::Unauthorized,
                            "Self-approval required",
                            format!(
                                "{}: you authored or repaired this proposal. Ask a teammate to review it, or explicitly confirm self-approval (selfApprove) to approve it without teammate review.",
                                SELF_APPROVAL_REQUIRED
                            ),
                        ));
                    }
                    if let Some(path) = proposal_drifted(ctx.config, &p) {
                        return Err(TeamError::conflict(format!(
                            "{} changed since the proposal was published; run `knobyte inbox proposal mark-stale {}` and repair it",
                            path, p.id
                        )));
                    }
                    for e in &p.target_revisions {
                        plan.read(e.path.clone());
                    }
                    let (rel, content) = approved_content(ctx, &p)?;
                    let existed = ctx.config.scaffold_root.join(&rel).exists();
                    plan.write_text(rel.clone(), "canonical", content, "Apply approved knowledge change");
                    p.target = rel;
                    p.status = PROPOSAL_STATUS_APPROVED.to_string();
                    p.self_approved = is_author;
                    let applied = if !existed { "created".to_string() } else if p.change.is_some() { "updated".to_string() } else { p.mode.clone() };
                    p.decision_by = Some(actor.clone());
                    p.decision_reason = rationale.clone();
                    p.decided_at = Some(ctx.now());
                    p.updated_at = ctx.now();
                    plan.write_json(proposal_rel(ctx, &p.id)?, "canonical", &p, "Approve proposal")?;
                    let mut subjects = vec![ActivitySubject::entity("proposal", &p.id, Some(&p.title))];
                    if let Some(eid) = &p.entity_id {
                        subjects.push(ActivitySubject::entity("entity", eid, None));
                    }
                    ctx.activity(
                        &mut plan,
                        None,
                        "inbox.approve",
                        "inbox_proposal",
                        &p.id,
                        &p.title,
                        format!("Approved inbox proposal '{}' into {} ({})", p.title, p.target, applied),
                        crate::team::workflow::meta(&[
                            ("target", json!(p.target)),
                            ("mode", json!(p.mode)),
                            ("applied", json!(applied)),
                            ("note", json!(p.decision_reason)),
                            ("selfApproved", json!(p.self_approved)),
                        ]),
                        subjects,
                        None,
                    )?;
                    let ev = decision_event(ctx, &p, "Approved", rationale.as_deref())?;
                    plan.appends.push(ev);
                    if p.self_approved {
                        plan.diagnostics.push(Diagnostic::warning("INBOX_SELF_APPROVED", "Approved without teammate review."));
                    }
                    plan.summary = format!("Approve proposal '{}' into .knobyte/{}", p.title, p.target);
                }
                "inbox.reject" => {
                    ctx.require_active_member("review inbox proposals")?;
                    if is_author {
                        return Err(TeamError::unauthorized(if p.author == actor {
                            "You authored this proposal; withdraw it instead of rejecting it."
                        } else {
                            "You repaired this proposal; ask another teammate to review it."
                        }));
                    }
                    p.status = PROPOSAL_STATUS_REJECTED.to_string();
                    p.decision_by = Some(actor.clone());
                    p.decision_reason = rationale.clone();
                    p.decided_at = Some(ctx.now());
                    p.updated_at = ctx.now();
                    plan.write_json(proposal_rel(ctx, &p.id)?, "canonical", &p, "Reject proposal")?;
                    ctx.activity(&mut plan, None, "inbox.reject", "inbox_proposal", &p.id, &p.title,
                        format!("Rejected inbox proposal '{}' for {}", p.title, p.target),
                        crate::team::workflow::meta(&[("target", json!(p.target)), ("note", json!(p.decision_reason))]),
                        vec![ActivitySubject::entity("proposal", &p.id, Some(&p.title))], None)?;
                    let ev = decision_event(ctx, &p, "Rejected", rationale.as_deref())?;
                    plan.appends.push(ev);
                    plan.summary = format!("Reject proposal '{}'", p.title);
                }
                "inbox.withdraw" => {
                    if p.author != actor {
                        return Err(TeamError::unauthorized(format!("Only the author ('{}') can withdraw this proposal.", p.author)));
                    }
                    p.status = PROPOSAL_STATUS_WITHDRAWN.to_string();
                    p.decision_by = Some(actor.clone());
                    p.decision_reason = rationale.clone();
                    p.decided_at = Some(ctx.now());
                    p.updated_at = ctx.now();
                    plan.write_json(proposal_rel(ctx, &p.id)?, "canonical", &p, "Withdraw proposal")?;
                    ctx.activity(&mut plan, None, "inbox.withdraw", "inbox_proposal", &p.id, &p.title,
                        format!("Withdrew inbox proposal '{}'", p.title), None,
                        vec![ActivitySubject::entity("proposal", &p.id, Some(&p.title))], None)?;
                    plan.summary = format!("Withdraw proposal '{}'", p.title);
                }
                _ => {
                    let reason = rationale.clone().ok_or_else(|| TeamError::usage("mark-stale requires a rationale (--reason)"))?;
                    let drifted = proposal_drifted(ctx.config, &p).ok_or_else(|| {
                        TeamError::validation("The proposal target is still current; a proposal can be marked stale only after its target changed.")
                    })?;
                    p.status = PROPOSAL_STATUS_STALE.to_string();
                    p.stale_reason = Some(format!("{} ({} changed)", reason, drifted));
                    p.updated_at = ctx.now();
                    plan.write_json(proposal_rel(ctx, &p.id)?, "canonical", &p, "Mark proposal stale")?;
                    ctx.activity(&mut plan, None, "inbox.mark-stale", "inbox_proposal", &p.id, &p.title,
                        format!("Marked inbox proposal '{}' stale", p.title),
                        crate::team::workflow::meta(&[("rationale", json!(reason.chars().take(240).collect::<String>()))]),
                        vec![ActivitySubject::entity("proposal", &p.id, Some(&p.title))], None)?;
                    plan.summary = format!("Mark proposal '{}' stale", p.title);
                }
            }
            plan.result = json!(p);
        }
        "inbox.repair" => {
            let RepairAction { proposal_id, replacement } = parse_action(action)?;
            let mut p = require_proposal(ctx, &proposal_id)?;
            if p.status != PROPOSAL_STATUS_STALE {
                return Err(TeamError::validation("Only a stale inbox proposal can be repaired."));
            }
            ctx.require_active_member("repair inbox proposals")?;
            let repairer = ctx.actor_id();
            if !p.is_contributor(&repairer) {
                p.repaired_by.push(repairer.clone());
            }
            let (input, target, title, content) = normalize_draft_input(ctx.config, replacement, &mut plan)?;
            let (target_revisions, entity_id) = capture_expectations(ctx, &input, &target, &mut plan)?;
            p.target = target;
            p.title = title;
            p.proposed_content = content;
            p.reason = input.rationale.clone();
            p.mode = input.mode.clone().unwrap_or_else(default_proposal_mode);
            p.change = input.change.clone();
            p.evidence = input.evidence.clone();
            p.target_revisions = target_revisions;
            p.entity_id = entity_id;
            p.status = PROPOSAL_STATUS_PENDING.to_string();
            p.decision_by = None;
            p.decision_reason = None;
            p.decided_at = None;
            p.stale_reason = None;
            p.updated_at = ctx.now();
            plan.write_json(proposal_rel(ctx, &p.id)?, "canonical", &p, "Repair proposal")?;
            ctx.activity(&mut plan, None, "inbox.repair", "inbox_proposal", &p.id, &p.title,
                format!("Repaired inbox proposal '{}'", p.title),
                crate::team::workflow::meta(&[
                    ("author", json!(p.author)),
                    ("repairedBy", json!(p.repaired_by)),
                    ("authorRepair", json!(p.author == repairer)),
                ]),
                vec![ActivitySubject::entity("proposal", &p.id, Some(&p.title))], None)?;
            plan.summary = format!("Repair proposal '{}'", p.title);
            plan.result = json!(p);
        }
        other => return Err(TeamError::usage(format!("Unsupported inbox action '{}'", other))),
    }
    Ok(plan)
}

// ---------------------------------------------------------------------------
// Library API (one-shot preview + apply)
// ---------------------------------------------------------------------------

fn result_proposal(r: crate::team::workflow::ApplyResult) -> Result<InboxProposal, TeamError> {
    serde_json::from_value(r.result).map_err(|e| TeamError::internal(e.to_string()))
}

pub fn publish_inbox_draft(config: &KnobyteConfig, draft_id: &str) -> Result<InboxProposal, String> {
    validate_entity_id(draft_id)?;
    Ok(result_proposal(run_action(config, json!({ "kind": "inbox.publish", "draftId": draft_id }), &ActorChoice::resolved())?)?)
}

/// Approve a pending proposal as `reviewer_member_id`. Authors cannot approve
/// their own proposal here; use [`approve_proposal_with`] with `self_approve`.
pub fn approve_proposal(config: &KnobyteConfig, proposal_id: &str, reviewer_member_id: &str, note: Option<&str>) -> Result<InboxProposal, String> {
    Ok(approve_proposal_with(config, proposal_id, reviewer_member_id, note, false)?)
}

/// Approve with an explicit self-approval acknowledgement.
pub fn approve_proposal_with(
    config: &KnobyteConfig,
    proposal_id: &str,
    reviewer_member_id: &str,
    note: Option<&str>,
    self_approve: bool,
) -> Result<InboxProposal, TeamError> {
    validate_entity_id(reviewer_member_id).map_err(|e| TeamError::usage(format!("Invalid reviewer: {}", e)))?;
    let action = json!({ "kind": "inbox.approve", "proposalId": proposal_id, "rationale": note, "selfApprove": self_approve });
    result_proposal(run_action(config, action, &ActorChoice::trusted(reviewer_member_id))?)
}

/// Reject a pending proposal without touching its target.
pub fn reject_proposal(config: &KnobyteConfig, proposal_id: &str, reviewer_member_id: &str, note: Option<&str>) -> Result<InboxProposal, String> {
    validate_entity_id(reviewer_member_id).map_err(|e| format!("Invalid reviewer: {}", e))?;
    let action = json!({ "kind": "inbox.reject", "proposalId": proposal_id, "rationale": note });
    Ok(result_proposal(run_action(config, action, &ActorChoice::trusted(reviewer_member_id))?)?)
}

/// Withdraw a pending proposal (author only).
pub fn withdraw_proposal(config: &KnobyteConfig, proposal_id: &str, actor: &ActorChoice, note: Option<&str>) -> Result<InboxProposal, TeamError> {
    result_proposal(run_action(config, json!({ "kind": "inbox.withdraw", "proposalId": proposal_id, "rationale": note }), actor)?)
}
