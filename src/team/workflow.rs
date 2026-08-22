//! Transactional preview/apply engine for every team mutation.
//!
//! A caller-authored [`TeamCommand`] is *previewed*: the service binds it to
//! service-owned authority (actor, time, repository state), plans the exact
//! file changes, records the content revision of every touched file as an
//! expectation, and signs the result (HMAC, checkout-local key) into a
//! [`PreviewEnvelope`]. *Applying* that envelope verifies the signature, the
//! 30-minute age bound and the actor, takes the team lock, re-plans with the
//! recorded authority and ids, refuses if anything changed, and performs the
//! writes through the intent -> complete journal.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::KnobyteConfig;
use crate::team::activity::{ActivityOrigin, ActivityRecord, ActivitySubject, ACTIVITY_SCHEMA_VERSION};
use crate::team::envelope::{Diagnostic, ErrorCode, TeamError};
use crate::team::identity::{explicit_actor, resolve_actor, ActorRef, ActorResolution, ActorSource};
use crate::team::relay::{observe_repo_state, ObservedRepoState};
use crate::team::store::{
    self, canonical_json, execute_journaled, file_revision, read_journal, recover_interrupted, revision_of,
    value_revision, JournalAppend, JournalEntry, JournalWrite, RecoveryReport, TeamLock, JOURNAL_COMPLETE,
};
use crate::team::token::{sign_preview_payload, verify_preview_payload};

pub const RECEIPT_SCHEMA_VERSION: u32 = 1;
/// A preview older than this cannot be applied.
pub const MAX_PREVIEW_AGE_SECS: i64 = 30 * 60;
/// Tolerated clock skew for previews stamped in the future.
pub const MAX_FUTURE_SKEW_SECS: i64 = 5;
/// Upper bound of purpose ids carried in a receipt.
pub const MAX_PURPOSE_IDS: usize = 4;

/// Optimistic precondition: the exact content revision of a scaffold-relative
/// file (`None` = the file must not exist).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RevisionExpectation {
    pub path: String,
    pub revision: Option<String>,
}

/// Caller intent. Actor, time and repository state have no caller slot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TeamCommand {
    #[serde(rename = "operationId")]
    pub operation_id: String,
    pub action: Value,
    #[serde(default, rename = "expectedRevisions")]
    pub expected_revisions: Vec<RevisionExpectation>,
}

impl TeamCommand {
    pub fn new(action: Value) -> Self {
        TeamCommand { operation_id: new_operation_id(), action, expected_revisions: Vec::new() }
    }

    pub fn kind(&self) -> String {
        self.action.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string()
    }
}

pub fn new_operation_id() -> String {
    format!("op_{}", uuid::Uuid::new_v4().simple())
}

/// Authority captured by the service while preparing a preview.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Authority {
    pub actor: ActorRef,
    #[serde(rename = "actorSource")]
    pub actor_source: ActorSource,
    #[serde(rename = "occurredAt")]
    pub occurred_at: String,
    #[serde(rename = "repoState")]
    pub repo_state: ObservedRepoState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PurposeId {
    pub purpose: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileChange {
    pub path: String,
    pub kind: String,
    pub namespace: String,
    #[serde(rename = "beforeRevision")]
    pub before_revision: Option<String>,
    #[serde(rename = "afterRevision")]
    pub after_revision: Option<String>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PublicPreview {
    pub valid: bool,
    /// `canonical` (shared scaffold), `local` (checkout-only) or `mixed`.
    pub scope: String,
    pub summary: String,
    pub changes: Vec<FileChange>,
    pub diagnostics: Vec<Diagnostic>,
    /// Projection of the primary record as it will be after apply.
    pub result: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Receipt {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub authority: Authority,
    #[serde(rename = "purposeIds")]
    pub purpose_ids: Vec<PurposeId>,
    #[serde(rename = "requestRevision")]
    pub request_revision: String,
    #[serde(rename = "presentationRevision")]
    pub presentation_revision: String,
    #[serde(rename = "previewRevision")]
    pub preview_revision: String,
    pub signature: String,
}

/// Signed, exact preview that `--apply` consumes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreviewEnvelope {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub request: TeamCommand,
    pub preview: PublicPreview,
    pub receipt: Receipt,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyResult {
    #[serde(rename = "operationId")]
    pub operation_id: String,
    #[serde(rename = "previewRevision")]
    pub preview_revision: String,
    pub applied: bool,
    #[serde(rename = "idempotentReplay")]
    pub idempotent_replay: bool,
    pub summary: String,
    pub changes: Vec<FileChange>,
    pub result: Value,
    pub diagnostics: Vec<Diagnostic>,
    #[serde(default)]
    pub recovered: Vec<RecoveryReport>,
}

/// Who acts.
///
/// * [`ActorChoice::resolved`]: the checkout's resolved actor.
/// * [`ActorChoice::member`]: a member id *requested* by a transport (CLI
///   `--member`/`--sender`, Hub `actorMemberId`, MCP). It is only an assertion:
///   it must equal the resolved actor, so no caller can act as somebody else
///   (for example approve their own proposal as a teammate).
/// * [`ActorChoice::trusted`]: an explicit member for in-process library
///   callers that have already established who acts. Never build it from
///   caller input.
#[derive(Debug, Clone, Default)]
pub struct ActorChoice {
    pub member_id: Option<String>,
    trusted: bool,
}

/// Machine code (leading the error detail) when a requested actor is not the resolved actor.
pub const ACTOR_MISMATCH: &str = "ACTOR_MISMATCH";

impl ActorChoice {
    pub fn resolved() -> Self {
        ActorChoice { member_id: None, trusted: false }
    }
    /// A requested member that must equal the resolved actor.
    pub fn member(id: &str) -> Self {
        ActorChoice { member_id: Some(id.to_string()), trusted: false }
    }
    /// An explicit active member trusted by in-process code (library helpers, tests).
    pub fn trusted(id: &str) -> Self {
        ActorChoice { member_id: Some(id.to_string()), trusted: true }
    }
    fn resolve(&self, config: &KnobyteConfig) -> Result<ActorResolution, TeamError> {
        match &self.member_id {
            Some(id) if self.trusted => explicit_actor(config, id).map_err(|e| {
                if e.contains("not found") {
                    TeamError::new(ErrorCode::NotFound, "Member not found", e)
                } else {
                    TeamError::unauthorized(e)
                }
            }),
            Some(id) => {
                let res = resolve_actor(config);
                match res.actor.member_id() {
                    Some(current) if current == id.trim() => Ok(res),
                    current => Err(TeamError::new(
                        ErrorCode::Unauthorized,
                        "Actor mismatch",
                        format!(
                            "{}: requested member '{}' is not the current actor ({}); a caller cannot act as another member. That member must act from their own checkout.",
                            ACTOR_MISMATCH,
                            id.trim(),
                            current.map(|c| format!("'{}'", c)).unwrap_or_else(|| res.actor.id()),
                        ),
                    )),
                }
            }
            None => Ok(resolve_actor(config)),
        }
    }
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

pub(crate) struct PlannedWrite {
    pub rel: String,
    pub namespace: &'static str,
    /// `None` deletes the file.
    pub content: Option<Vec<u8>>,
    pub summary: String,
}

#[derive(Default)]
pub(crate) struct Plan {
    pub writes: Vec<PlannedWrite>,
    pub appends: Vec<JournalAppend>,
    /// Files read as dependencies (their revisions become expectations).
    pub reads: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
    pub result: Value,
    pub summary: String,
}

impl Plan {
    pub fn write_json<T: Serialize>(&mut self, rel: String, namespace: &'static str, value: &T, summary: impl Into<String>) -> Result<(), TeamError> {
        let bytes = store::json_bytes(value).map_err(TeamError::internal)?;
        self.writes.push(PlannedWrite { rel, namespace, content: Some(bytes), summary: summary.into() });
        Ok(())
    }
    pub fn write_text(&mut self, rel: String, namespace: &'static str, text: String, summary: impl Into<String>) {
        self.writes.push(PlannedWrite { rel, namespace, content: Some(text.into_bytes()), summary: summary.into() });
    }
    pub fn delete(&mut self, rel: String, namespace: &'static str, summary: impl Into<String>) {
        self.writes.push(PlannedWrite { rel, namespace, content: None, summary: summary.into() });
    }
    pub fn read(&mut self, rel: String) {
        if !self.reads.contains(&rel) {
            self.reads.push(rel);
        }
    }
}

/// Ids minted during preview and replayed exactly on apply.
pub(crate) struct IdMinter {
    ids: Vec<PurposeId>,
    replay: bool,
}

impl IdMinter {
    fn fresh() -> Self {
        IdMinter { ids: Vec::new(), replay: false }
    }
    fn replay(ids: Vec<PurposeId>) -> Self {
        IdMinter { ids, replay: true }
    }
    pub fn id(&mut self, purpose: &str, mint: impl FnOnce() -> String) -> Result<String, TeamError> {
        if let Some(p) = self.ids.iter().find(|p| p.purpose == purpose) {
            return Ok(p.id.clone());
        }
        if self.replay {
            return Err(TeamError::conflict(format!(
                "The preview receipt has no '{}' id; preview again",
                purpose
            )));
        }
        let id = mint();
        self.ids.push(PurposeId { purpose: purpose.to_string(), id: id.clone() });
        Ok(id)
    }
}

/// Planning context handed to each action planner.
pub(crate) struct Ctx<'a> {
    pub config: &'a KnobyteConfig,
    pub authority: &'a Authority,
    pub ids: IdMinter,
    pub kind: String,
}

impl Ctx<'_> {
    pub fn actor_id(&self) -> String {
        self.authority.actor.id()
    }
    pub fn now(&self) -> String {
        self.authority.occurred_at.clone()
    }
    /// Path of `abs` relative to the scaffold root.
    pub fn rel(&self, abs: &Path) -> String {
        rel_path(self.config, abs)
    }
    /// Require the actor to be an active member, returning its id.
    pub fn require_active_member(&self, purpose: &str) -> Result<String, TeamError> {
        let id = self.authority.actor.member_id().ok_or_else(|| {
            TeamError::unauthorized(format!(
                "Select an active member (`knobyte member select <id>`), or configure one unique active Git alias, to {}.",
                purpose
            ))
        })?;
        match crate::team::members::get_member(self.config, id) {
            Some(m) if m.is_active() => Ok(id.to_string()),
            _ => Err(TeamError::unauthorized(format!("The current actor '{}' is missing or inactive.", id))),
        }
    }

    /// Plan an activity record for this operation.
    #[allow(clippy::too_many_arguments)]
    pub fn activity(
        &mut self,
        plan: &mut Plan,
        actor: Option<String>,
        action: &str,
        entity_kind: &str,
        entity_id: &str,
        entity_title: &str,
        summary: String,
        metadata: Option<Value>,
        subjects: Vec<ActivitySubject>,
        workstream: Option<String>,
    ) -> Result<(), TeamError> {
        let id = self.ids.id("activity", || uuid::Uuid::new_v4().to_string())?;
        let record = ActivityRecord {
            id: id.clone(),
            timestamp: self.now(),
            actor: actor.unwrap_or_else(|| self.actor_id()),
            action: action.to_string(),
            entity_kind: entity_kind.to_string(),
            entity_id: entity_id.to_string(),
            entity_title: entity_title.to_string(),
            summary,
            metadata,
            schema_version: ACTIVITY_SCHEMA_VERSION,
            actor_ref: Some(self.authority.actor.clone()),
            subjects,
            origin: Some(ActivityOrigin::Workflow { operation: self.kind.clone() }),
            repo_state: Some(self.authority.repo_state.clone()),
            workstream,
            label: Some(entity_title.chars().take(200).collect()),
        };
        let rel = self.rel(&self.config.activity_dir().join(format!("{}.json", id)));
        plan.write_json(rel, "canonical", &record, format!("Record activity {}", action))
    }
}

pub fn rel_path(config: &KnobyteConfig, abs: &Path) -> String {
    abs.strip_prefix(&config.scaffold_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| abs.to_string_lossy().to_string())
}

fn abs_path(config: &KnobyteConfig, rel: &str) -> Result<PathBuf, TeamError> {
    let p = Path::new(rel);
    if p.is_absolute() || p.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
        return Err(TeamError::new(
            ErrorCode::PathOutsideProject,
            "Path outside project",
            format!("'{}' must be a path relative to the .knobyte/ scaffold", rel),
        ));
    }
    Ok(config.scaffold_root.join(p))
}

fn plan_action(ctx: &mut Ctx, action: &Value) -> Result<Plan, TeamError> {
    let kind = action.get("kind").and_then(|k| k.as_str()).unwrap_or("");
    match kind {
        k if k.starts_with("member.") => crate::team::members::plan(ctx, k, action),
        "activity.record" => crate::team::activity::plan_record(ctx, action),
        k if k.starts_with("workstream.") => crate::team::workstreams::plan(ctx, k, action),
        k if k.starts_with("inbox.") => crate::team::inbox::plan(ctx, k, action),
        k if k.starts_with("relay.") => crate::team::relay::plan(ctx, k, action),
        k if k.starts_with("playbook.") => crate::team::playbooks::plan(ctx, k, action),
        k if k.starts_with("catchup.") => crate::team::catchup::plan(ctx, k, action),
        "" => Err(TeamError::usage("The request action has no 'kind'")),
        other => Err(TeamError::usage(format!("Unsupported team action '{}'", other))),
    }
}

/// Deserialize an action payload into its typed shape, refusing unknown fields
/// where the target type denies them.
pub(crate) fn parse_action<T: for<'de> Deserialize<'de>>(action: &Value) -> Result<T, TeamError> {
    let mut v = action.clone();
    if let Some(obj) = v.as_object_mut() {
        obj.remove("kind");
    }
    serde_json::from_value(v).map_err(|e| TeamError::usage(format!("Invalid action payload: {}", e)))
}

struct Prepared {
    plan: Plan,
    changes: Vec<FileChange>,
    purpose_ids: Vec<PurposeId>,
}

fn prepare(config: &KnobyteConfig, command: &TeamCommand, authority: &Authority, ids: IdMinter) -> Result<Prepared, TeamError> {
    let mut ctx = Ctx { config, authority, ids, kind: command.kind() };
    let plan = plan_action(&mut ctx, &command.action)?;
    let mut changes = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for w in &plan.writes {
        if !seen.insert(w.rel.clone()) {
            return Err(TeamError::internal(format!("Plan writes {} twice", w.rel)));
        }
        let abs = abs_path(config, &w.rel)?;
        let before = file_revision(&abs);
        let after = w.content.as_ref().map(|c| revision_of(c));
        if before == after {
            continue;
        }
        let kind = match (&before, &after) {
            (None, Some(_)) => "create",
            (Some(_), None) => "delete",
            _ => "update",
        };
        changes.push(FileChange {
            path: w.rel.clone(),
            kind: kind.to_string(),
            namespace: w.namespace.to_string(),
            before_revision: before,
            after_revision: after,
            summary: w.summary.clone(),
        });
    }
    let purpose_ids = ctx.ids.ids;
    if purpose_ids.len() > MAX_PURPOSE_IDS {
        return Err(TeamError::internal("Too many purpose ids in one preview"));
    }
    Ok(Prepared { plan, changes, purpose_ids })
}

fn scope_of(changes: &[FileChange]) -> String {
    let canonical = changes.iter().any(|c| c.namespace == "canonical");
    let local = changes.iter().any(|c| c.namespace == "local");
    match (canonical, local) {
        (true, true) => "mixed",
        (false, true) => "local",
        _ => "canonical",
    }
    .to_string()
}

fn compute_preview_revision(request_revision: &str, presentation_revision: &str, authority: &Authority, purpose_ids: &[PurposeId], appends: &[JournalAppend]) -> String {
    let v = serde_json::json!({
        "requestRevision": request_revision,
        "presentationRevision": presentation_revision,
        "authority": authority,
        "purposeIds": purpose_ids,
        "appends": appends,
    });
    revision_of(canonical_json(&v).as_bytes())
}

fn signing_payload(request: &TeamCommand, preview: &PublicPreview, receipt: &Receipt) -> String {
    let mut r = serde_json::to_value(receipt).unwrap_or(Value::Null);
    if let Some(obj) = r.as_object_mut() {
        obj.remove("signature");
    }
    let v = serde_json::json!({
        "purpose": "knobyte.team.preview.v1",
        "request": request,
        "preview": preview,
        "receipt": r,
    });
    canonical_json(&v)
}

fn validate_operation_id(id: &str) -> Result<(), TeamError> {
    let ok = !id.is_empty()
        && id.len() <= 128
        && id.chars().next().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false)
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'));
    if ok {
        Ok(())
    } else {
        Err(TeamError::usage("operationId must be 1-128 characters of letters, digits, '_', '-', '.', ':'"))
    }
}

fn check_expectations(config: &KnobyteConfig, expectations: &[RevisionExpectation]) -> Result<(), TeamError> {
    for e in expectations {
        let current = file_revision(&abs_path(config, &e.path)?);
        if current != e.revision {
            return Err(TeamError::conflict(format!(
                "{} changed since it was read (expected {}, found {}); preview again",
                e.path,
                e.revision.as_deref().unwrap_or("absent"),
                current.as_deref().unwrap_or("absent")
            )));
        }
    }
    Ok(())
}

/// Preview a command: plan exact changes and return a signed envelope. Nothing is written
/// except the checkout-local signing key on first use.
pub fn preview(config: &KnobyteConfig, command: &TeamCommand, actor: &ActorChoice) -> Result<PreviewEnvelope, TeamError> {
    validate_operation_id(&command.operation_id)?;
    if command.expected_revisions.len() > 64 {
        return Err(TeamError::usage("At most 64 expectedRevisions are allowed"));
    }
    let resolution = actor.resolve(config)?;
    let authority = Authority {
        actor: resolution.actor,
        actor_source: resolution.source,
        occurred_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        repo_state: observe_repo_state(&config.project_root),
    };
    // Caller-supplied expectations must hold right now.
    check_expectations(config, &command.expected_revisions)?;
    let prepared = prepare(config, command, &authority, IdMinter::fresh())?;

    // Complete the expectations with every touched and read file.
    let mut request = command.clone();
    let mut push = |path: &str, revision: Option<String>| {
        if !request.expected_revisions.iter().any(|e| e.path == path) {
            request.expected_revisions.push(RevisionExpectation { path: path.to_string(), revision });
        }
    };
    for c in &prepared.changes {
        push(&c.path, c.before_revision.clone());
    }
    for r in &prepared.plan.reads {
        push(r, file_revision(&abs_path(config, r)?));
    }
    request.expected_revisions.sort_by(|a, b| a.path.cmp(&b.path));

    let mut diagnostics = resolution.diagnostics;
    diagnostics.extend(prepared.plan.diagnostics.clone());
    let preview = PublicPreview {
        valid: true,
        scope: scope_of(&prepared.changes),
        summary: prepared.plan.summary.clone(),
        changes: prepared.changes.clone(),
        diagnostics,
        result: prepared.plan.result.clone(),
    };
    let request_revision = value_revision(&request);
    let presentation_revision = value_revision(&preview);
    let preview_revision = compute_preview_revision(
        &request_revision,
        &presentation_revision,
        &authority,
        &prepared.purpose_ids,
        &prepared.plan.appends,
    );
    let mut receipt = Receipt {
        schema_version: RECEIPT_SCHEMA_VERSION,
        authority,
        purpose_ids: prepared.purpose_ids,
        request_revision,
        presentation_revision,
        preview_revision,
        signature: String::new(),
    };
    receipt.signature = sign_preview_payload(&config.local_dir(), &signing_payload(&request, &preview, &receipt));
    Ok(PreviewEnvelope { schema_version: RECEIPT_SCHEMA_VERSION, request, preview, receipt })
}

/// Re-sign an envelope with this checkout's key (tooling and tests; the key
/// never leaves `local/`, so this grants nothing a local caller lacks).
pub fn sign_envelope(config: &KnobyteConfig, env: &mut PreviewEnvelope) {
    env.receipt.signature = sign_preview_payload(&config.local_dir(), &signing_payload(&env.request, &env.preview, &env.receipt));
}

fn verify_envelope(config: &KnobyteConfig, env: &PreviewEnvelope) -> Result<(), TeamError> {
    if env.schema_version != RECEIPT_SCHEMA_VERSION || env.receipt.schema_version != RECEIPT_SCHEMA_VERSION {
        return Err(TeamError::usage("Unsupported preview envelope schema version"));
    }
    if !verify_preview_payload(&config.local_dir(), &signing_payload(&env.request, &env.preview, &env.receipt), &env.receipt.signature) {
        return Err(TeamError::unauthorized(
            "The preview envelope signature is invalid: it was altered or issued by another checkout. Preview again.",
        ));
    }
    if value_revision(&env.request) != env.receipt.request_revision
        || value_revision(&env.preview) != env.receipt.presentation_revision
    {
        return Err(TeamError::unauthorized("The preview envelope was altered after signing."));
    }
    let occurred = chrono::DateTime::parse_from_rfc3339(&env.receipt.authority.occurred_at)
        .map_err(|_| TeamError::usage("The preview receipt has an invalid timestamp"))?
        .with_timezone(&chrono::Utc);
    let now = chrono::Utc::now();
    if (occurred - now).num_seconds() > MAX_FUTURE_SKEW_SECS {
        return Err(TeamError::conflict("The preview is stamped in the future; check the system clock and preview again."));
    }
    if (now - occurred).num_seconds() > MAX_PREVIEW_AGE_SECS {
        return Err(TeamError::conflict("The preview is older than 30 minutes; preview again."));
    }
    Ok(())
}

/// Apply a signed preview envelope. Refuses if the actor, any touched file, or
/// the planned result differs from the preview.
pub fn apply(config: &KnobyteConfig, env: &PreviewEnvelope, actor: &ActorChoice) -> Result<ApplyResult, TeamError> {
    verify_envelope(config, env)?;
    let current = actor.resolve(config)?;
    if current.actor.principal() != env.receipt.authority.actor.principal() {
        return Err(TeamError::unauthorized(format!(
            "The preview was issued for actor '{}' but the current actor is '{}'.",
            env.receipt.authority.actor.id(),
            current.actor.id()
        )));
    }

    let _lock = TeamLock::acquire(config).map_err(|e| TeamError::new(ErrorCode::RevisionConflict, "Team state is locked", e))?;
    let recovered = recover_interrupted(config);

    let op = &env.request.operation_id;
    if let Some(j) = read_journal(config, op) {
        if j.state == JOURNAL_COMPLETE && j.preview_revision == env.receipt.preview_revision {
            return Ok(ApplyResult {
                operation_id: op.clone(),
                preview_revision: j.preview_revision.clone(),
                applied: true,
                idempotent_replay: true,
                summary: env.preview.summary.clone(),
                changes: env.preview.changes.clone(),
                result: j.result.clone(),
                diagnostics: Vec::new(),
                recovered,
            });
        }
        if j.state == JOURNAL_COMPLETE {
            return Err(TeamError::conflict(format!(
                "Operation id '{}' was already applied with different content; use a new operation id.",
                op
            )));
        }
        return Err(TeamError::new(
            ErrorCode::OperationInterrupted,
            "Operation interrupted",
            format!("Operation '{}' was interrupted and could not be recovered automatically: {}", op, j.state),
        ));
    }

    check_expectations(config, &env.request.expected_revisions)?;
    let prepared = prepare(config, &env.request, &env.receipt.authority, IdMinter::replay(env.receipt.purpose_ids.clone()))?;
    if prepared.changes != env.preview.changes || prepared.plan.result != env.preview.result {
        return Err(TeamError::conflict("Team state changed since the preview; preview again."));
    }
    let presentation = PublicPreview {
        valid: true,
        scope: scope_of(&prepared.changes),
        summary: prepared.plan.summary.clone(),
        changes: prepared.changes.clone(),
        diagnostics: env.preview.diagnostics.clone(),
        result: prepared.plan.result.clone(),
    };
    let preview_revision = compute_preview_revision(
        &env.receipt.request_revision,
        &value_revision(&presentation),
        &env.receipt.authority,
        &prepared.purpose_ids,
        &prepared.plan.appends,
    );
    if preview_revision != env.receipt.preview_revision {
        return Err(TeamError::conflict("Team state changed since the preview; preview again."));
    }

    let mut writes = Vec::new();
    for w in &prepared.plan.writes {
        let after = w.content.as_ref().map(|c| revision_of(c));
        let abs = abs_path(config, &w.rel)?;
        let before = file_revision(&abs);
        if before == after {
            continue;
        }
        let content = match &w.content {
            Some(c) => Some(String::from_utf8(c.clone()).map_err(|_| TeamError::internal("non UTF-8 content"))?),
            None => None,
        };
        writes.push(JournalWrite { path: w.rel.clone(), before_revision: before, after_revision: after, content });
    }
    let entry = JournalEntry {
        schema_version: 1,
        operation_id: op.clone(),
        command: env.request.kind(),
        preview_revision: preview_revision.clone(),
        state: String::new(),
        started_at: chrono::Utc::now().to_rfc3339(),
        completed_at: None,
        writes,
        appends: prepared.plan.appends.clone(),
        result: prepared.plan.result.clone(),
    };
    execute_journaled(config, entry).map_err(|e| {
        TeamError::new(ErrorCode::OperationInterrupted, "Operation interrupted", format!("{}; the next team command will complete it", e))
    })?;

    Ok(ApplyResult {
        operation_id: op.clone(),
        preview_revision,
        applied: true,
        idempotent_replay: false,
        summary: prepared.plan.summary,
        changes: prepared.changes,
        result: prepared.plan.result,
        diagnostics: env.preview.diagnostics.clone(),
        recovered,
    })
}

/// One-shot: preview then immediately apply (the human default).
pub fn execute(config: &KnobyteConfig, command: &TeamCommand, actor: &ActorChoice) -> Result<ApplyResult, TeamError> {
    let env = preview(config, command, actor)?;
    apply(config, &env, actor)
}

/// Convenience for library callers: build and execute an action value.
pub fn run_action(config: &KnobyteConfig, action: Value, actor: &ActorChoice) -> Result<ApplyResult, TeamError> {
    execute(config, &TeamCommand::new(action), actor)
}

/// Roll forward any interrupted journal entries now (takes the team lock).
pub fn recover(config: &KnobyteConfig) -> Result<Vec<RecoveryReport>, TeamError> {
    let _lock = TeamLock::acquire(config).map_err(|e| TeamError::new(ErrorCode::RevisionConflict, "Team state is locked", e))?;
    Ok(recover_interrupted(config))
}

/// Parse a preview envelope from file content: accepts the complete CLI
/// envelope emitted by `--preview --json` or the bare preview envelope.
pub fn parse_envelope(content: &str) -> Result<PreviewEnvelope, TeamError> {
    if content.len() > 256 * 1024 {
        return Err(TeamError::usage("The preview envelope exceeds 256 KiB"));
    }
    let v: Value = serde_json::from_str(content).map_err(|e| TeamError::usage(format!("Invalid preview envelope JSON: {}", e)))?;
    let inner = if v.get("receipt").is_some() {
        v
    } else if let Some(data) = v.get("data").filter(|d| d.get("receipt").is_some()) {
        if v.get("mode").and_then(|m| m.as_str()) != Some("preview") {
            return Err(TeamError::usage("Only an envelope emitted in preview mode can be applied"));
        }
        data.clone()
    } else {
        return Err(TeamError::usage("The file is not a team preview envelope"));
    };
    serde_json::from_value(inner).map_err(|e| TeamError::usage(format!("Invalid preview envelope: {}", e)))
}

/// Read a JSON document from disk with a size bound.
pub fn read_bounded_json(path: &Path) -> Result<Value, TeamError> {
    let meta = fs::metadata(path).map_err(|e| TeamError::usage(format!("Cannot read {}: {}", path.display(), e)))?;
    if meta.len() > 256 * 1024 {
        return Err(TeamError::usage(format!("{} exceeds 256 KiB", path.display())));
    }
    let content = fs::read_to_string(path).map_err(|e| TeamError::usage(format!("Cannot read {}: {}", path.display(), e)))?;
    serde_json::from_str(&content).map_err(|e| TeamError::usage(format!("{} is not valid JSON: {}", path.display(), e)))
}

/// Read a caller-authored request file: either a full `TeamCommand`
/// (`{operationId?, action, expectedRevisions?}`) or a bare action object.
pub fn read_request_file(path: &Path, expected_kind: Option<&str>) -> Result<TeamCommand, TeamError> {
    let v = read_bounded_json(path)?;
    let mut cmd = if v.get("action").is_some() {
        let mut obj = v.clone();
        if obj.get("operationId").is_none() {
            obj["operationId"] = Value::String(new_operation_id());
        }
        serde_json::from_value::<TeamCommand>(obj).map_err(|e| TeamError::usage(format!("Invalid request: {}", e)))?
    } else {
        TeamCommand::new(v)
    };
    if let Some(k) = expected_kind {
        if cmd.kind().is_empty() {
            cmd.action["kind"] = Value::String(k.to_string());
        } else if cmd.kind() != k {
            return Err(TeamError::usage(format!("The request action is '{}' but this command runs '{}'", cmd.kind(), k)));
        }
    }
    Ok(cmd)
}

/// Load a JSON record if present.
pub(crate) fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    fs::read_to_string(path).ok().and_then(|c| serde_json::from_str(&c).ok())
}

/// Convert a `BTreeMap` of metadata into a JSON value (helper for planners).
pub(crate) fn meta(pairs: &[(&str, Value)]) -> Option<Value> {
    let map: BTreeMap<String, Value> = pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
    Some(serde_json::to_value(map).unwrap_or(Value::Null))
}
