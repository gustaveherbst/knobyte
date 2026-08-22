//! Typed wiki operations (`knobyte wiki apply <op.json>`).
//!
//! Eleven operations — create-entry, update-entry, set-property, add-relation,
//! remove-relation, add-source, remove-source, set-grounding, supersede-entry, move-entry,
//! archive-entry — are planned against a virtual copy of the scaffold, checked against
//! revision/content-hash preconditions, and turned into byte-range edits that touch only the
//! affected entity's metadata keys, heading or body. After every planned edit the file is
//! re-parsed and every other entity must be byte-identical (write-scope check). A batch is
//! all-or-nothing. Applied operations are recorded in the append-only audit log
//! `.knobyte/events/operations.jsonl` (an `intent` line before writing, a `complete` line after);
//! re-applying a recorded `opId` with the same payload is a no-op, with a different payload an
//! error.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::graph::grounding::{readable_ref_for, resolve_grounding_ref, RefResolution};
use crate::wiki::diagnostics::{diag, has_errors, DiagExt};
use crate::wiki::markdown::{apply_edits, dominant_eol, Edit};
use crate::wiki::models::{
    is_lifecycle_state, is_relation_type, EntityRelation, Provenance, WikiDiagnostic, WikiEntity,
    WikiSource, RELATION_TYPES,
};
use crate::wiki::parser::{
    is_valid_entity_id, parse_markdown_file_with, slugify, MetadataKind, ParsedEntity, ParsedFile,
};
use crate::wiki::scope::{is_canonical_markdown_path, WikiScope};
use crate::wiki::validate::{source_diagnostics, TopicIndex, TopicResolution};
use crate::wiki::yaml::{remove_key_edit, render_key_value, set_key_edit};

pub const OPERATION_TYPES: [&str; 11] = [
    "create-entry",
    "update-entry",
    "set-property",
    "add-relation",
    "remove-relation",
    "add-source",
    "remove-source",
    "set-grounding",
    "supersede-entry",
    "move-entry",
    "archive-entry",
];

pub const SETTABLE_PROPERTIES: [&str; 7] = [
    "type", "status", "title", "summary", "topics", "metadata", "aliases",
];

pub const OPERATION_LOG_FILE: &str = "events/operations.jsonl";

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpActor {
    pub kind: String,
    pub id: String,
    #[serde(default, rename = "sessionId", skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Envelope {
    pub op_id: String,
    pub op_type: String,
    pub entity_id: Option<String>,
    pub base_revision: Option<i64>,
    pub base_content_hash: Option<String>,
    pub actor: OpActor,
    pub reason: Option<String>,
    pub timestamp: String,
    pub payload: Value,
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())
}

/// Hash of `type` + canonical payload JSON (object keys sorted).
pub fn payload_hash(op_type: &str, payload: &Value) -> String {
    sha256_hex(&format!("{}\u{0}{}", op_type, canonical_json(payload)))
}

fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .filter(|k| !m[k.as_str()].is_null())
                .map(|k| {
                    format!(
                        "{}:{}",
                        Value::String((*k).clone()),
                        canonical_json(&m[k.as_str()])
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(canonical_json).collect::<Vec<_>>().join(",")
        ),
        other => other.to_string(),
    }
}

fn str_field(o: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| o.get(*k))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Validate and normalize one envelope. A missing `opId` or `timestamp` is filled in (so a
/// hand-written operation file works), but only a caller-supplied `opId` makes replay
/// idempotent.
pub fn parse_envelope(v: &Value, default_actor: &OpActor) -> Result<Envelope, Vec<WikiDiagnostic>> {
    let Some(o) = v.as_object() else {
        return Err(vec![diag(
            "INVALID_OPERATION_ENVELOPE",
            "Expected an operation envelope object",
            "",
        )]);
    };
    let op_type = str_field(o, &["type"]).unwrap_or_default();
    if !OPERATION_TYPES.contains(&op_type.as_str()) {
        return Err(vec![diag(
            "UNKNOWN_OPERATION_TYPE",
            format!(
                "Unknown operation type {:?}. Expected one of {}",
                op_type,
                OPERATION_TYPES.join(", ")
            ),
            "",
        )]);
    }
    let mut diags = Vec::new();
    let entity_id = str_field(o, &["entityId", "entity_id"]);
    if op_type != "create-entry" && entity_id.is_none() {
        diags.push(diag(
            "INVALID_OPERATION_ENVELOPE",
            format!(
                "A \"{}\" operation must name the entity it acts on (`entityId`)",
                op_type
            ),
            "",
        ));
    }
    let base_revision = match o.get("baseRevision").or_else(|| o.get("base_revision")) {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_i64().filter(|n| *n >= 1) {
            Some(n) => Some(n),
            None => {
                diags.push(diag(
                    "INVALID_OPERATION_ENVELOPE",
                    "`baseRevision` must be an integer >= 1",
                    "",
                ));
                None
            }
        },
    };
    let base_content_hash = str_field(o, &["baseContentHash", "base_content_hash"]);
    if let Some(h) = &base_content_hash {
        if h.len() != 64
            || !h
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            diags.push(diag(
                "INVALID_OPERATION_ENVELOPE",
                "`baseContentHash` must be a 64-character lowercase hex SHA-256 digest",
                "",
            ));
        }
    }
    let actor = match o.get("actor") {
        None | Some(Value::Null) => default_actor.clone(),
        Some(a) => match serde_json::from_value::<OpActor>(a.clone()) {
            Ok(actor)
                if ["human", "agent", "system"].contains(&actor.kind.as_str())
                    && !actor.id.is_empty() =>
            {
                actor
            }
            _ => {
                diags.push(diag(
                    "INVALID_OPERATION_ENVELOPE",
                    "`actor` must be {kind: human|agent|system, id}",
                    "",
                ));
                default_actor.clone()
            }
        },
    };
    let timestamp = match str_field(o, &["timestamp"]) {
        Some(t) => {
            if chrono::DateTime::parse_from_rfc3339(&t).is_err() {
                diags.push(diag(
                    "INVALID_OPERATION_ENVELOPE",
                    format!("\"{}\" is not an ISO 8601 timestamp", t),
                    "",
                ));
            }
            t
        }
        None => chrono::Utc::now().to_rfc3339(),
    };
    let payload = o
        .get("payload")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    if !payload.is_object() {
        diags.push(diag(
            "INVALID_OPERATION_PAYLOAD",
            "`payload` must be an object",
            "",
        ));
    }
    let op_id = str_field(o, &["opId", "op_id"]).unwrap_or_else(|| {
        format!(
            "op_{}",
            &sha256_hex(&format!("{}{}", uuid::Uuid::new_v4(), timestamp))[..24]
        )
    });
    if !diags.is_empty() {
        return Err(diags);
    }
    Ok(Envelope {
        op_id,
        op_type,
        entity_id,
        base_revision,
        base_content_hash,
        actor,
        reason: str_field(o, &["reason"]),
        timestamp,
        payload,
    })
}

// ---------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RevisionChange {
    pub entity_id: String,
    pub before: i64,
    pub after: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    pub v: u32,
    pub phase: String,
    pub op_id: String,
    #[serde(rename = "type")]
    pub op_type: String,
    pub entity_ids: Vec<String>,
    pub created_ids: Vec<String>,
    pub actor: OpActor,
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub files: Vec<String>,
    pub payload_hash: String,
    pub revisions: Vec<RevisionChange>,
}

pub fn operation_log_path(scaffold_root: &Path) -> PathBuf {
    scaffold_root.join(OPERATION_LOG_FILE)
}

/// Read the audit log; malformed lines become `MALFORMED_OPERATION_LOG` diagnostics.
pub fn read_audit_log(scaffold_root: &Path) -> (Vec<AuditEntry>, Vec<WikiDiagnostic>) {
    let path = operation_log_path(scaffold_root);
    let Ok(text) = fs::read_to_string(&path) else {
        return (Vec::new(), Vec::new());
    };
    let mut entries = Vec::new();
    let mut diags = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<AuditEntry>(line) {
            Ok(e) => entries.push(e),
            Err(e) => diags.push(
                diag(
                    "MALFORMED_OPERATION_LOG",
                    format!(
                        "Line {} of {} is not a valid audit entry: {}",
                        i + 1,
                        OPERATION_LOG_FILE,
                        e
                    ),
                    OPERATION_LOG_FILE,
                )
                .at_line(Some(i + 1)),
            ),
        }
    }
    (entries, diags)
}

fn append_audit(scaffold_root: &Path, entries: &[AuditEntry]) -> std::io::Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let path = operation_log_path(scaffold_root);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let mut buf = String::new();
    for e in entries {
        buf.push_str(&serde_json::to_string(e).map_err(std::io::Error::other)?);
        buf.push('\n');
    }
    f.write_all(buf.as_bytes())?;
    f.sync_all()
}

/// Write a file through a temporary sibling and rename (no torn writes).
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(std::io::Error::other(format!(
            "{} is a symlink; refusing to write through it",
            path.display()
        )));
    }
    let tmp = path.with_extension(format!(
        "{}.{}.kbtmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("md"),
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

// ---------------------------------------------------------------------------
// Virtual scaffold
// ---------------------------------------------------------------------------

struct Vfs<'a> {
    scope: &'a WikiScope,
    /// Overlay of planned file contents (None = deleted / absent).
    overlay: HashMap<String, String>,
    known: Vec<String>,
    cache: HashMap<String, ParsedFile>,
    graph: Option<Connection>,
    /// Bump each entity's revision at most once per batch (migration: one revision per
    /// migrated entity, however many findings it had).
    bump_once: bool,
    bumped: HashSet<String>,
}

impl<'a> Vfs<'a> {
    fn new(scope: &'a WikiScope, graph_db: Option<&Path>) -> Self {
        let (files, _) = scope.discover();
        let graph = graph_db
            .filter(|p| p.exists())
            .and_then(|p| Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY).ok());
        Self {
            scope,
            overlay: HashMap::new(),
            known: files.into_iter().map(|(r, _)| r).collect(),
            cache: HashMap::new(),
            graph,
            bump_once: false,
            bumped: HashSet::new(),
        }
    }

    fn read(&self, rel: &str) -> Option<String> {
        if let Some(t) = self.overlay.get(rel) {
            return Some(t.clone());
        }
        let p = self.scope.scaffold_root.join(rel);
        if p.is_file() {
            fs::read(&p)
                .ok()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
        } else {
            None
        }
    }

    fn write(&mut self, rel: &str, text: String) {
        self.cache.remove(rel);
        if !self.known.iter().any(|k| k == rel) {
            self.known.push(rel.to_string());
            self.known.sort();
        }
        self.overlay.insert(rel.to_string(), text);
    }

    fn parsed(&mut self, rel: &str) -> Option<ParsedFile> {
        if let Some(p) = self.cache.get(rel) {
            return Some(p.clone());
        }
        let text = self.read(rel)?;
        let p = parse_markdown_file_with(rel, &text, &self.scope.registry);
        self.cache.insert(rel.to_string(), p.clone());
        Some(p)
    }

    fn all_entities(&mut self) -> Vec<WikiEntity> {
        let known = self.known.clone();
        let mut out = Vec::new();
        for rel in known {
            if let Some(p) = self.parsed(&rel) {
                out.extend(p.entities.into_iter().map(|e| e.entity));
            }
        }
        out
    }

    /// First claimant of `id` (file order).
    fn locate(&mut self, id: &str) -> Option<(ParsedFile, ParsedEntity)> {
        let known = self.known.clone();
        for rel in known {
            if let Some(p) = self.parsed(&rel) {
                if let Some(e) = p.entities.iter().find(|e| e.entity.id == id).cloned() {
                    return Some((p, e));
                }
            }
        }
        None
    }

    fn id_exists(&mut self, id: &str) -> bool {
        self.locate(id).is_some()
    }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub file: String,
    pub created: bool,
    pub diff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpOutcome {
    pub op_id: String,
    #[serde(rename = "type")]
    pub op_type: String,
    pub entity_ids: Vec<String>,
    pub created_ids: Vec<String>,
    pub revisions: Vec<RevisionChange>,
    pub files: Vec<String>,
    pub replayed: bool,
    pub changes: Vec<FileChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReport {
    pub ok: bool,
    pub dry_run: bool,
    pub operations: Vec<OpOutcome>,
    pub changed_files: Vec<String>,
    pub diagnostics: Vec<WikiDiagnostic>,
}

pub struct ApplyOptions<'a> {
    pub scope: &'a WikiScope,
    pub graph_db: Option<&'a Path>,
    pub dry_run: bool,
    pub default_actor: OpActor,
}

/// Envelopes from an operation file: one object, an array, `{ "operations": [...] }`, or JSONL.
pub fn load_operation_file(text: &str) -> Result<Vec<Value>, String> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(a)) => Ok(a),
        Ok(Value::Object(o)) => match o.get("operations") {
            Some(Value::Array(a)) => Ok(a.clone()),
            _ => Ok(vec![Value::Object(o)]),
        },
        Ok(_) => Err("An operation file holds an object or an array of objects".to_string()),
        Err(first) => {
            let mut out = Vec::new();
            for (i, line) in text.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                out.push(serde_json::from_str::<Value>(line).map_err(|e| {
                    format!("line {}: {} (as a single document: {})", i + 1, e, first)
                })?);
            }
            Ok(out)
        }
    }
}

/// Plan and (unless dry-run) apply a batch of operation envelopes. All-or-nothing.
pub fn apply_operations(raw: &[Value], opts: &ApplyOptions) -> ApplyReport {
    apply_operations_with(raw, opts, false)
}

/// [`apply_operations`] where, with `revision_once`, an entity changed by several operations
/// of the batch gets one revision bump in total (used by `wiki migrate`).
pub fn apply_operations_with(raw: &[Value], opts: &ApplyOptions, revision_once: bool) -> ApplyReport {
    let mut report = ApplyReport {
        ok: false,
        dry_run: opts.dry_run,
        operations: Vec::new(),
        changed_files: Vec::new(),
        diagnostics: Vec::new(),
    };
    let mut envelopes = Vec::new();
    for (i, v) in raw.iter().enumerate() {
        match parse_envelope(v, &opts.default_actor) {
            Ok(e) => envelopes.push(e),
            Err(ds) => {
                report.diagnostics.extend(
                    ds.into_iter()
                        .map(|d| d.at_path(format!("operations[{}]", i))),
                );
            }
        }
    }
    if !report.diagnostics.is_empty() {
        return report;
    }
    let (log, log_diags) = read_audit_log(&opts.scope.scaffold_root);
    report.diagnostics.extend(log_diags);
    let mut by_op: HashMap<&str, (Option<&AuditEntry>, Option<&AuditEntry>)> = HashMap::new();
    for e in &log {
        let slot = by_op.entry(e.op_id.as_str()).or_default();
        if e.phase == "complete" {
            slot.1 = Some(e);
        } else {
            slot.0 = Some(e);
        }
    }

    let mut vfs = Vfs::new(opts.scope, opts.graph_db);
    vfs.bump_once = revision_once;
    let mut originals: HashMap<String, Option<String>> = HashMap::new();
    let mut changed_entities: HashSet<String> = HashSet::new();
    let mut planned: Vec<(Envelope, String, OpPlan)> = Vec::new();
    let mut seen_ops: HashSet<String> = HashSet::new();

    for env in envelopes {
        let hash = payload_hash(&env.op_type, &env.payload);
        if !seen_ops.insert(env.op_id.clone()) {
            report.diagnostics.push(diag(
                "INVALID_OPERATION_ENVELOPE",
                format!("opId {} appears twice in this batch", env.op_id),
                "",
            ));
            return report;
        }
        let (intent, complete) = by_op
            .get(env.op_id.as_str())
            .copied()
            .unwrap_or((None, None));
        if let Some(prior) = complete.or(intent) {
            if prior.payload_hash != hash {
                report.diagnostics.push(diag(
                    "INVALID_OPERATION_ENVELOPE",
                    format!(
                        "Operation {} has already been recorded with a different payload. Reusing an opId for a different change is a caller bug, not a retry",
                        env.op_id
                    ),
                    "",
                ));
                return report;
            }
        }
        if let Some(done) = complete {
            report.operations.push(OpOutcome {
                op_id: env.op_id.clone(),
                op_type: env.op_type.clone(),
                entity_ids: done.entity_ids.clone(),
                created_ids: done.created_ids.clone(),
                revisions: done.revisions.clone(),
                files: done.files.clone(),
                replayed: true,
                changes: Vec::new(),
            });
            continue;
        }
        let forced_ids: Vec<String> = intent.map(|i| i.created_ids.clone()).unwrap_or_default();
        // Inside a batch, later operations on an entity changed earlier see its new state.
        let mut env = env;
        if let Some(id) = env.entity_id.clone() {
            if changed_entities.contains(&id) {
                if let Some((_, pe)) = vfs.locate(&id) {
                    if env.base_revision.is_some() {
                        env.base_revision = Some(pe.entity.revision);
                    }
                    if env.base_content_hash.is_some() {
                        env.base_content_hash = Some(pe.entity.content_hash.clone());
                    }
                }
            }
        }
        let before: HashMap<String, String> = vfs
            .known
            .clone()
            .into_iter()
            .filter_map(|k| vfs.read(&k).map(|t| (k, t)))
            .collect();
        let mut ctx = Ctx {
            vfs: &mut vfs,
            forced_ids,
            diags: Vec::new(),
            moving: false,
        };
        let plan = plan_operation(&env, &mut ctx);
        let op_diags = std::mem::take(&mut ctx.diags);
        drop(ctx);
        report.diagnostics.extend(op_diags.clone());
        let Some(plan) = plan else {
            if !has_errors(&op_diags) {
                report.diagnostics.push(diag(
                    "INVALID_OPERATION_PAYLOAD",
                    format!("Operation {} could not be planned", env.op_id),
                    "",
                ));
            }
            return report;
        };
        if has_errors(&op_diags) {
            return report;
        }
        for f in &plan.files {
            originals
                .entry(f.clone())
                .or_insert_with(|| before.get(f).cloned());
        }
        for id in plan.entity_ids.iter().chain(plan.created_ids.iter()) {
            changed_entities.insert(id.clone());
        }
        let changes = plan
            .files
            .iter()
            .map(|f| {
                let old = before.get(f).cloned();
                let new = vfs.read(f).unwrap_or_default();
                FileChange {
                    file: f.clone(),
                    created: old.is_none(),
                    diff: line_diff(old.as_deref().unwrap_or(""), &new),
                }
            })
            .collect();
        report.operations.push(OpOutcome {
            op_id: env.op_id.clone(),
            op_type: env.op_type.clone(),
            entity_ids: plan.entity_ids.clone(),
            created_ids: plan.created_ids.clone(),
            revisions: plan.revisions.clone(),
            files: plan.files.clone(),
            replayed: false,
            changes,
        });
        planned.push((env, hash, plan));
    }

    let mut changed: Vec<String> = originals
        .iter()
        .filter(|(k, v)| {
            vfs.overlay
                .get(*k)
                .map(|t| Some(t) != v.as_ref())
                .unwrap_or(false)
        })
        .map(|(k, _)| k.clone())
        .collect();
    changed.sort();
    for f in &changed {
        if opts.scope.is_read_only(f) {
            report.diagnostics.push(diag(
                "WRITE_SCOPE_VIOLATION",
                format!("{} is read-only to the wiki (team-owned or wiki.readOnly). Nothing was written", f),
                f.clone(),
            ));
            return report;
        }
    }
    // Containment: every write target (and the audit log) must resolve inside the scaffold
    // through its real parent chain, and never through a symlinked file. Checked before the
    // dry-run return so a plan reports the refusal too.
    for f in changed.iter().map(String::as_str).chain(std::iter::once(OPERATION_LOG_FILE)) {
        if let Err(d) = crate::wiki::paths::check_write_target(&opts.scope.scaffold_root, f) {
            report.diagnostics.push(d);
            return report;
        }
    }
    report.changed_files = changed.clone();
    if opts.dry_run {
        report.ok = true;
        return report;
    }

    let entry = |env: &Envelope, hash: &str, plan: &OpPlan, phase: &str| AuditEntry {
        v: 1,
        phase: phase.to_string(),
        op_id: env.op_id.clone(),
        op_type: env.op_type.clone(),
        entity_ids: plan.entity_ids.clone(),
        created_ids: plan.created_ids.clone(),
        actor: env.actor.clone(),
        timestamp: env.timestamp.clone(),
        reason: env.reason.clone(),
        files: plan.files.clone(),
        payload_hash: hash.to_string(),
        revisions: plan.revisions.clone(),
    };
    let intents: Vec<AuditEntry> = planned
        .iter()
        .map(|(e, h, p)| entry(e, h, p, "intent"))
        .collect();
    if let Err(e) = append_audit(&opts.scope.scaffold_root, &intents) {
        report.diagnostics.push(diag(
            "WRITE_SCOPE_VIOLATION",
            format!(
                "Could not append to {}: {}. Nothing was written",
                OPERATION_LOG_FILE, e
            ),
            OPERATION_LOG_FILE,
        ));
        return report;
    }
    for f in &changed {
        let text = vfs.overlay.get(f).cloned().unwrap_or_default();
        if let Err(mut d) = crate::wiki::paths::write_contained(&opts.scope.scaffold_root, f, &text) {
            if d.code == "WRITE_SCOPE_VIOLATION" {
                d.message = format!("{}. Re-apply the same operation file to finish", d.message);
            }
            report.diagnostics.push(d);
            return report;
        }
    }
    let completes: Vec<AuditEntry> = planned
        .iter()
        .map(|(e, h, p)| entry(e, h, p, "complete"))
        .collect();
    if let Err(e) = append_audit(&opts.scope.scaffold_root, &completes) {
        report.diagnostics.push(diag(
            "MALFORMED_OPERATION_LOG",
            format!(
                "Markdown was written but the completion could not be logged: {}",
                e
            ),
            OPERATION_LOG_FILE,
        ));
    }
    report.ok = true;
    report
}

/// Unchanged lines shown around each change by [`line_diff`] (like `diff -U3`).
pub const DIFF_CONTEXT: usize = 3;

/// Unified line diff (`diff -U3` style): only the lines that changed, each hunk with up to
/// [`DIFF_CONTEXT`] unchanged lines around it. Changes further apart get separate hunks, so a
/// frontmatter edit that touches `relations` and `revision` shows those lines, not the whole
/// block. Empty when both texts have the same lines.
pub fn line_diff(old: &str, new: &str) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = diff_ops(&a, &b);
    let changed: Vec<usize> = (0..ops.len())
        .filter(|&i| !matches!(ops[i], DiffOp::Equal(..)))
        .collect();
    if changed.is_empty() {
        return String::new();
    }
    // Group changes whose separating run of unchanged lines fits in two contexts.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    for &i in &changed {
        match groups.last_mut() {
            Some((_, last)) if i - *last - 1 <= 2 * DIFF_CONTEXT => *last = i,
            _ => groups.push((i, i)),
        }
    }
    let mut out = String::new();
    for (first, last) in groups {
        let hunk = &ops[first.saturating_sub(DIFF_CONTEXT)..(last + 1 + DIFF_CONTEXT).min(ops.len())];
        let (a_start, b_start) = match hunk[0] {
            DiffOp::Equal(x, y) | DiffOp::Delete(x, y) | DiffOp::Insert(x, y) => (x, y),
        };
        let a_len = hunk.iter().filter(|o| !matches!(o, DiffOp::Insert(..))).count();
        let b_len = hunk.iter().filter(|o| !matches!(o, DiffOp::Delete(..))).count();
        // An empty range names the line before it, as in `diff -u`.
        let pos = |start: usize, len: usize| if len == 0 { start } else { start + 1 };
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            pos(a_start, a_len),
            a_len,
            pos(b_start, b_len),
            b_len
        ));
        for o in hunk {
            match *o {
                DiffOp::Equal(x, _) => out.push_str(&format!(" {}\n", a[x])),
                DiffOp::Delete(x, _) => out.push_str(&format!("-{}\n", a[x])),
                DiffOp::Insert(_, y) => out.push_str(&format!("+{}\n", b[y])),
            }
        }
    }
    out
}

/// One line of an edit script, with the (old, new) line positions it sits at.
#[derive(Debug, Clone, Copy)]
enum DiffOp {
    Equal(usize, usize),
    Delete(usize, usize),
    Insert(usize, usize),
}

/// Edit script from `a` to `b`: common prefix and suffix, then a longest-common-subsequence
/// alignment of the middle (replaced wholesale only when it is too large to align cheaply).
fn diff_ops(a: &[&str], b: &[&str]) -> Vec<DiffOp> {
    let mut p = 0;
    while p < a.len() && p < b.len() && a[p] == b[p] {
        p += 1;
    }
    let mut s = 0;
    while s < a.len() - p && s < b.len() - p && a[a.len() - 1 - s] == b[b.len() - 1 - s] {
        s += 1;
    }
    let am = &a[p..a.len() - s];
    let bm = &b[p..b.len() - s];
    let (n, m) = (am.len(), bm.len());
    let mut ops: Vec<DiffOp> = (0..p).map(|k| DiffOp::Equal(k, k)).collect();
    const MAX_CELLS: usize = 4_000_000;
    if n.saturating_mul(m) > MAX_CELLS {
        ops.extend((0..n).map(|k| DiffOp::Delete(p + k, p)));
        ops.extend((0..m).map(|k| DiffOp::Insert(p + n, p + k)));
    } else {
        // lcs[i * (m + 1) + j] = length of the LCS of am[i..] and bm[j..].
        let w = m + 1;
        let mut lcs = vec![0u32; (n + 1) * w];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i * w + j] = if am[i] == bm[j] {
                    lcs[(i + 1) * w + j + 1] + 1
                } else {
                    lcs[(i + 1) * w + j].max(lcs[i * w + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && am[i] == bm[j] {
                ops.push(DiffOp::Equal(p + i, p + j));
                i += 1;
                j += 1;
            } else if i < n && (j == m || lcs[(i + 1) * w + j] >= lcs[i * w + j + 1]) {
                // Deletions before insertions within a replaced run.
                ops.push(DiffOp::Delete(p + i, p + j));
                i += 1;
            } else {
                ops.push(DiffOp::Insert(p + i, p + j));
                j += 1;
            }
        }
    }
    let (ea, eb) = (a.len() - s, b.len() - s);
    ops.extend((0..s).map(|k| DiffOp::Equal(ea + k, eb + k)));
    ops
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

struct Ctx<'v, 'a> {
    vfs: &'v mut Vfs<'a>,
    forced_ids: Vec<String>,
    diags: Vec<WikiDiagnostic>,
    /// Set while a move-entry removes its entity from the source file.
    moving: bool,
}

impl Ctx<'_, '_> {
    fn err<T>(&mut self, code: &str, msg: impl Into<String>, entity: Option<&str>) -> Option<T> {
        let mut d = diag(code, msg, "");
        if let Some(e) = entity {
            d = d.for_entity(e);
        }
        self.diags.push(d);
        None
    }
}

#[derive(Debug, Clone, Default)]
struct OpPlan {
    entity_ids: Vec<String>,
    created_ids: Vec<String>,
    revisions: Vec<RevisionChange>,
    files: Vec<String>,
}

impl OpPlan {
    fn touch(&mut self, file: &str) {
        if !self.files.iter().any(|f| f == file) {
            self.files.push(file.to_string());
        }
    }
}

type Fields = Vec<(String, Option<serde_yaml::Value>)>;

fn yv<T: Serialize>(v: &T) -> serde_yaml::Value {
    serde_yaml::to_value(v).unwrap_or(serde_yaml::Value::Null)
}

/// Payload helpers.
fn p_str(p: &Value, keys: &[&str]) -> Option<String> {
    let o = p.as_object()?;
    str_field(o, keys)
}

fn p_get<'p>(p: &'p Value, keys: &[&str]) -> Option<&'p Value> {
    let o = p.as_object()?;
    keys.iter().find_map(|k| o.get(*k)).filter(|v| !v.is_null())
}

fn relation_from(v: &Value) -> Option<EntityRelation> {
    let o = v.as_object()?;
    Some(EntityRelation {
        rel_type: str_field(o, &["type"])?,
        target_id: str_field(o, &["target", "target_id", "targetId"])?,
        note: str_field(o, &["note"]),
        waived: o.get("waived").and_then(|v| v.as_bool()).unwrap_or(false)
            || o.get("metadata")
                .and_then(|m| m.get("waived"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        metadata: None,
        origin: None,
    })
}

fn string_list_value(v: &Value) -> Option<Vec<String>> {
    match v {
        Value::Array(a) => a
            .iter()
            .map(|e| e.as_str().map(|s| s.trim().to_string()))
            .collect(),
        Value::String(s) => Some(vec![s.trim().to_string()]),
        _ => None,
    }
}

fn plan_operation(env: &Envelope, ctx: &mut Ctx) -> Option<OpPlan> {
    let p = &env.payload;
    if env.op_type == "create-entry" {
        let id = ctx.forced_ids.first().cloned();
        let mut plan = OpPlan::default();
        create_into(ctx, p, id, Vec::new(), &mut plan)?;
        return Some(plan);
    }
    let id = env.entity_id.clone().unwrap_or_default();
    let Some((file, pe)) = ctx.vfs.locate(&id) else {
        return ctx.err(
            "ENTITY_NOT_FOUND",
            format!("No entity has id {}", id),
            Some(&id),
        );
    };
    if let Some(base) = env.base_revision {
        if base != pe.entity.revision {
            return ctx.err(
                "REVISION_CONFLICT",
                format!(
                    "Operation was built against revision {}, but {} is at revision {}",
                    base, id, pe.entity.revision
                ),
                Some(&id),
            );
        }
    }
    if let Some(h) = &env.base_content_hash {
        if *h != pe.entity.content_hash {
            return ctx.err(
                "CONTENT_HASH_CONFLICT",
                format!(
                    "The text of {} changed since this operation was planned",
                    id
                ),
                Some(&id),
            );
        }
    }
    let mut plan = OpPlan::default();
    match env.op_type.as_str() {
        "update-entry" => update_entry(ctx, &file, &pe, p, &mut plan)?,
        "set-property" => set_property(ctx, &file, &pe, p, &mut plan)?,
        "add-relation" => add_relation(ctx, &file, &pe, p, &mut plan)?,
        "remove-relation" => remove_relation(ctx, &file, &pe, p, &mut plan)?,
        "add-source" => add_source(ctx, &file, &pe, p, &mut plan)?,
        "remove-source" => remove_source(ctx, &file, &pe, p, &mut plan)?,
        "set-grounding" => set_grounding(ctx, &file, &pe, p, &mut plan)?,
        "archive-entry" => {
            if pe.entity.status == "archived" {
                return ctx.err(
                    "INVALID_LIFECYCLE_STATE",
                    format!("{} is already archived", id),
                    Some(&id),
                );
            }
            mutate(
                ctx,
                &file,
                &pe,
                vec![("status".into(), Some(yv(&"archived")))],
                Vec::new(),
                true,
                &mut plan,
            )?;
        }
        "supersede-entry" => supersede_entry(ctx, &file, &pe, p, &mut plan)?,
        "move-entry" => move_entry(ctx, &file, &pe, p, &mut plan)?,
        _ => return ctx.err("UNKNOWN_OPERATION_TYPE", env.op_type.clone(), None),
    }
    Some(plan)
}

/// Apply metadata field edits (and optional extra edits) to one entity, bumping its revision,
/// then verify the write scope.
fn mutate(
    ctx: &mut Ctx,
    file: &ParsedFile,
    pe: &ParsedEntity,
    mut fields: Fields,
    extra: Vec<Edit>,
    bump: bool,
    plan: &mut OpPlan,
) -> Option<()> {
    let id = pe.entity.id.clone();
    let bump = bump && !(ctx.vfs.bump_once && ctx.vfs.bumped.contains(&id));
    let before = pe.entity.revision;
    let after = if bump { before + 1 } else { before };
    if bump {
        fields.push(("revision".into(), Some(yv(&after))));
    }
    let text = &file.text;
    let eol = dominant_eol(text);
    let mut edits = extra;
    match pe.loc.kind {
        MetadataKind::Implicit => {
            // Materialize frontmatter holding the derived id plus the new keys.
            let mut keys: Fields = vec![("id".into(), Some(yv(&id)))];
            keys.extend(fields.into_iter().filter(|(k, _)| k != "id"));
            let body: Vec<String> = keys
                .iter()
                .filter_map(|(k, v)| v.as_ref().map(|v| render_key_value(k, v, eol)))
                .collect();
            edits.push(Edit {
                start: pe.loc.metadata_start,
                end: pe.loc.metadata_start,
                text: format!("---{eol}{}{eol}---{eol}", body.join(eol)),
                label: format!("frontmatter for {}", id),
            });
        }
        MetadataKind::Marker if pe.loc.inline_attrs => {
            // Rewrite the marker in block form, merging the edits into its keys.
            let mut map = pe.raw.clone();
            for (k, v) in &fields {
                match v {
                    Some(v) => {
                        map.insert(k.clone(), serde_json::to_value(v).unwrap_or(Value::Null));
                    }
                    None => {
                        map.remove(k);
                    }
                }
            }
            let rendered = render_marker(&map, eol);
            let original = &text[pe.loc.metadata_start..pe.loc.metadata_end];
            let trailing = if original.ends_with("\r\n") {
                "\r\n"
            } else if original.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            edits.push(Edit {
                start: pe.loc.metadata_start,
                end: pe.loc.metadata_end,
                text: format!("{}{}", rendered, trailing),
                label: format!("metadata of {}", id),
            });
        }
        _ => {
            for (k, v) in &fields {
                if k == "id" && !pe.raw.contains_key("id") {
                    // Pinning a derived id: the id leads the metadata block.
                    if let Some(v) = v {
                        edits.push(Edit {
                            start: pe.loc.yaml_start,
                            end: pe.loc.yaml_start,
                            text: format!("{}{eol}", render_key_value(k, v, eol)),
                            label: format!("pin id of {}", id),
                        });
                    }
                    continue;
                }
                match v {
                    Some(v) => edits.push(set_key_edit(
                        text,
                        pe.loc.yaml_start,
                        pe.loc.yaml_end,
                        k,
                        v,
                        eol,
                    )),
                    None => {
                        if let Some(e) =
                            remove_key_edit(text, pe.loc.yaml_start, pe.loc.yaml_end, k)
                        {
                            edits.push(e);
                        }
                    }
                }
            }
            // A new key is appended after the last one; when that last key is being removed
            // in the same edit, insert in front of it instead of inside the removed range.
            let removals: Vec<(usize, usize)> = edits
                .iter()
                .filter(|e| e.text.is_empty() && e.end > e.start)
                .map(|e| (e.start, e.end))
                .collect();
            for e in edits.iter_mut().filter(|e| e.start == e.end && !e.text.is_empty()) {
                if let Some((rs, _)) = removals.iter().find(|(rs, re)| e.start > *rs && e.start <= *re) {
                    if let Some(rest) = e.text.strip_prefix(eol) {
                        e.text = format!("{}{}", rest, eol);
                        e.start = *rs;
                        e.end = *rs;
                    }
                }
            }
        }
    }
    commit_edits(ctx, &file.path, text, edits, std::slice::from_ref(&id), &[])?;
    if bump {
        ctx.vfs.bumped.insert(id.clone());
    }
    plan.touch(&file.path);
    if !plan.entity_ids.contains(&id) {
        plan.entity_ids.push(id.clone());
    }
    plan.revisions.push(RevisionChange {
        entity_id: id,
        before,
        after,
    });
    Some(())
}

fn render_marker(map: &Map<String, Value>, eol: &str) -> String {
    let order = ["id", "type", "status", "revision", "title", "summary"];
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort_by_key(|k| {
        (
            order.iter().position(|o| o == k).unwrap_or(order.len()),
            k.to_string(),
        )
    });
    let mut lines = vec!["<!-- kb:entity".to_string()];
    for k in keys {
        let v: serde_yaml::Value = serde_yaml::to_value(&map[k]).unwrap_or(serde_yaml::Value::Null);
        lines.push(render_key_value(k, &v, eol));
    }
    lines.push("-->".into());
    lines.join(eol)
}

/// Apply `edits` to `path`, then re-parse and verify that every entity other than `targets`
/// (and newly `created`) kept its exact content.
fn commit_edits(
    ctx: &mut Ctx,
    path: &str,
    original: &str,
    edits: Vec<Edit>,
    targets: &[String],
    created: &[String],
) -> Option<()> {
    let new_text = match apply_edits(original, &edits) {
        Ok(t) => t,
        Err(e) => {
            ctx.diags.push(diag("WRITE_SCOPE_VIOLATION", e, path));
            return None;
        }
    };
    let before = parse_markdown_file_with(path, original, &ctx.vfs.scope.registry);
    let after = parse_markdown_file_with(path, &new_text, &ctx.vfs.scope.registry);
    let after_hashes: HashMap<&str, &str> = after
        .entities
        .iter()
        .map(|e| (e.entity.id.as_str(), e.entity.content_hash.as_str()))
        .collect();
    for e in &before.entities {
        // An empty file's implicit entity holds no text to preserve.
        if targets.contains(&e.entity.id) || before.entity_text(&e.loc).trim().is_empty() {
            continue;
        }
        match after_hashes.get(e.entity.id.as_str()) {
            Some(h) if *h == e.entity.content_hash => {}
            _ => {
                ctx.diags.push(
                    diag(
                        "WRITE_SCOPE_VIOLATION",
                        format!(
                            "The planned edit of {} would change entity {} outside the operation's scope. Nothing was written",
                            path, e.entity.id
                        ),
                        path,
                    )
                    .for_entity(&e.entity.id),
                );
                return None;
            }
        }
    }
    for id in created {
        if !after_hashes.contains_key(id.as_str()) {
            ctx.diags.push(diag(
                "WRITE_SCOPE_VIOLATION",
                format!(
                    "The planned text of {} does not parse back to entity {}",
                    path, id
                ),
                path,
            ));
            return None;
        }
    }
    for id in targets {
        if before.entity(id).is_some()
            && !after_hashes.contains_key(id.as_str())
            && !created.contains(id)
        {
            // A target may only disappear from a file when it is being moved out.
            if !ctx.moving {
                ctx.diags.push(diag(
                    "WRITE_SCOPE_VIOLATION",
                    format!("The planned edit of {} would lose entity {}", path, id),
                    path,
                ));
                return None;
            }
        }
    }
    ctx.vfs.write(path, new_text);
    Some(())
}

fn verify_groundings(ctx: &mut Ctx, refs: Vec<String>) -> Option<Vec<String>> {
    let Some(gc) = ctx.vfs.graph.as_ref() else {
        if !refs.is_empty() {
            ctx.diags.push(
                diag(
                    "GROUNDINGS_UNCHECKED",
                    "No code graph is built, so the new groundings were written unverified (run 'knobyte graph rebuild')",
                    "graph.db",
                )
                .severity("warning"),
            );
        }
        return Some(refs);
    };
    let mut out = Vec::new();
    let mut bad = Vec::new();
    for r in refs {
        match resolve_grounding_ref(gc, &r) {
            Ok(RefResolution::Resolved(node)) => {
                // Hashed graph ids are written in their readable form.
                if node.id == r {
                    out.push(readable_ref_for(&node));
                } else {
                    out.push(r);
                }
            }
            Ok(RefResolution::Ambiguous(c)) => bad.push(format!(
                "{} (ambiguous: {} symbols, e.g. {})",
                r,
                c.len(),
                readable_ref_for(&c[0])
            )),
            _ => bad.push(r),
        }
    }
    if !bad.is_empty() {
        ctx.diags.push(diag(
            "GROUNDING_UNVERIFIED",
            format!("The code graph cannot resolve: {}", bad.join(", ")),
            "",
        ));
        return None;
    }
    let mut dedup = Vec::new();
    for r in out {
        if !dedup.contains(&r) {
            dedup.push(r);
        }
    }
    Some(dedup)
}

fn resolve_topics(ctx: &mut Ctx, refs: Vec<String>, entity: Option<&str>) -> Option<Vec<String>> {
    let all = ctx.vfs.all_entities();
    let refs_all: Vec<&WikiEntity> = all.iter().collect();
    let index = TopicIndex::build(&refs_all);
    let mut out = Vec::new();
    for r in refs {
        match index.resolve(&r) {
            TopicResolution::Resolved(id) => {
                if !out.contains(&id) {
                    out.push(id)
                }
            }
            TopicResolution::Ambiguous(c) => {
                ctx.diags.push(
                    diag(
                        "AMBIGUOUS_TOPIC_REFERENCE",
                        format!(
                            "\"{}\" matches several topics ({}). Use the topic id",
                            r,
                            c.join(", ")
                        ),
                        "",
                    )
                    .for_entity(entity.unwrap_or("")),
                );
                return None;
            }
            TopicResolution::Unknown => {
                ctx.diags.push(
                    diag("UNKNOWN_TOPIC", format!("No topic matches \"{}\"", r), "")
                        .for_entity(entity.unwrap_or("")),
                );
                return None;
            }
        }
    }
    Some(out)
}

fn validate_sources(ctx: &mut Ctx, sources: &[WikiSource], id: &str) -> bool {
    let probe = WikiEntity {
        entity_key: String::new(),
        id: id.to_string(),
        file: String::new(),
        entity_type: String::new(),
        title: String::new(),
        summary: None,
        body: String::new(),
        status: "promoted".into(),
        revision: 1,
        relations: Vec::new(),
        grounds_to: Vec::new(),
        committed_groundings: Vec::new(),
        topics: Vec::new(),
        sources: sources.to_vec(),
        provenance: None,
        aliases: Vec::new(),
        metadata: None,
        health: None,
        start_line: 0,
        end_line: 0,
        heading_depth: 0,
        content_hash: String::new(),
        metadata_kind: String::new(),
    };
    let errors: Vec<WikiDiagnostic> = source_diagnostics(&probe, None)
        .into_iter()
        .filter(|d| d.severity == "error" || d.code == "DUPLICATE_SOURCE")
        .map(|d| d.severity("error"))
        .collect();
    let ok = errors.is_empty();
    ctx.diags.extend(errors);
    ok
}

fn sources_from(v: Option<&Value>) -> Result<Vec<WikiSource>, String> {
    match v {
        None => Ok(Vec::new()),
        Some(v) => serde_json::from_value::<Vec<WikiSource>>(v.clone()).map_err(|e| e.to_string()),
    }
}

fn grounding_refs_from(v: Option<&Value>) -> Result<Vec<String>, String> {
    let Some(v) = v else { return Ok(Vec::new()) };
    let Some(a) = v.as_array() else {
        return Err("`groundsTo` must be a list".into());
    };
    a.iter()
        .map(|g| match g {
            Value::String(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
            Value::Object(o) => str_field(o, &["node_id", "node", "nodeId", "ref"])
                .ok_or_else(|| "a grounding object needs `node_id`".to_string()),
            _ => Err("a grounding is a reference string".to_string()),
        })
        .collect()
}

fn mint_id(ctx: &mut Ctx, requested: Option<String>, title: &str) -> Result<String, String> {
    if let Some(id) = requested {
        if !is_valid_entity_id(&id) {
            return Err(format!("{:?} is not a valid entity id", id));
        }
        return Ok(id);
    }
    let base = format!("kb_{}", slugify(title));
    let mut candidate = base.clone();
    let mut n = 2;
    while ctx.vfs.id_exists(&candidate) {
        candidate = format!("{}_{}", base, n);
        n += 1;
    }
    Ok(candidate)
}

fn padded_insertion(text: &str, offset: usize, block: &str, eol: &str) -> String {
    let before = &text[..offset];
    let after = &text[offset..];
    let trailing = before.len() - before.trim_end_matches(['\n', '\r']).len();
    let trailing_breaks = before[before.len() - trailing..].matches('\n').count();
    let leading_breaks = after[..after.len() - after.trim_start_matches(['\n', '\r']).len()]
        .matches('\n')
        .count();
    let lead = if before.is_empty() {
        String::new()
    } else {
        eol.repeat(2usize.saturating_sub(trailing_breaks))
    };
    // The block already ends with its own line break; together with the breaks `after`
    // starts with, exactly one blank line separates it from what follows.
    let block_breaks = block[block.trim_end_matches(['\n', '\r']).len()..]
        .matches('\n')
        .count();
    let tail = if after.is_empty() {
        String::new()
    } else {
        eol.repeat(2usize.saturating_sub(leading_breaks + block_breaks))
    };
    let block = if eol == "\n" {
        block.to_string()
    } else {
        block.replace("\r\n", "\n").replace('\n', eol)
    };
    format!("{}{}{}", lead, block, tail)
}

fn insertion_offset(
    ctx: &mut Ctx,
    file: Option<&ParsedFile>,
    insert_at: Option<&Value>,
) -> Result<usize, (String, String)> {
    let Some(file) = file else { return Ok(0) };
    let at = insert_at
        .and_then(|v| v.get("at"))
        .and_then(|v| v.as_str())
        .unwrap_or("end-of-file");
    match at {
        "end-of-file" => Ok(file.text.len()),
        "start-of-file" => {
            let mut cursor = file.doc.frontmatter.as_ref().map(|f| f.end).unwrap_or(file.doc.bom);
            let b = file.text.as_bytes();
            while cursor < b.len() && (b[cursor] == b'\n' || b[cursor] == b'\r') {
                cursor += 1;
            }
            Ok(cursor)
        }
        "before-entity" | "after-entity" => {
            let target = insert_at
                .and_then(|v| v.get("entityId").or_else(|| v.get("entity_id")))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let _ = &ctx;
            match file.entity(target) {
                Some(e) if at == "before-entity" && e.loc.kind == MetadataKind::Marker => Ok(e.loc.metadata_start),
                Some(e) if at == "before-entity" => Ok(e.loc.heading_start),
                Some(e) => Ok(e.loc.body_end),
                None => Err((
                    "ENTITY_NOT_FOUND".into(),
                    format!("Cannot insert {} {}: it is not in {}", at, target, file.path),
                )),
            }
        }
        other => Err((
            "INVALID_OPERATION_PAYLOAD".into(),
            format!(
                "Unknown insertion point \"{}\". Expected start-of-file, end-of-file, before-entity or after-entity",
                other
            ),
        )),
    }
}

/// Fields of a new entity in write order.
#[allow(clippy::too_many_arguments)]
fn new_entity_fields(
    id: &str,
    p: &Value,
    status: &str,
    with_title: bool,
    topics: &[String],
    relations: &[EntityRelation],
    sources: &[WikiSource],
    groundings: &[String],
) -> Fields {
    let mut f: Fields = vec![
        ("id".into(), Some(yv(&id))),
        ("type".into(), p_str(p, &["type"]).map(|t| yv(&t))),
        ("status".into(), Some(yv(&status))),
        ("revision".into(), Some(yv(&1))),
    ];
    if with_title {
        f.push(("title".into(), p_str(p, &["title"]).map(|t| yv(&t))));
    }
    f.push(("summary".into(), p_str(p, &["summary"]).map(|s| yv(&s))));
    if !topics.is_empty() {
        f.push(("topics".into(), Some(yv(&topics))));
    }
    if let Some(a) = p_get(p, &["aliases"])
        .and_then(string_list_value)
        .filter(|a| !a.is_empty())
    {
        f.push(("aliases".into(), Some(yv(&a))));
    }
    if !relations.is_empty() {
        f.push(("relations".into(), Some(yv(&relations))));
    }
    if !sources.is_empty() {
        f.push(("sources".into(), Some(yv(&sources))));
    }
    if let Some(prov) =
        p_get(p, &["provenance"]).and_then(|v| serde_json::from_value::<Provenance>(v.clone()).ok())
    {
        f.push(("provenance".into(), Some(yv(&prov))));
    }
    if !groundings.is_empty() {
        f.push(("grounds_to".into(), Some(yv(&groundings))));
    }
    if let Some(m) =
        p_get(p, &["metadata"]).filter(|m| m.as_object().is_some_and(|o| !o.is_empty()))
    {
        f.push(("metadata".into(), Some(yv(m))));
    }
    f
}

fn render_fields(fields: &Fields, eol: &str) -> String {
    fields
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| render_key_value(k, v, eol)))
        .collect::<Vec<_>>()
        .join(eol)
}

/// Plan a create-entry payload. `extra_relations` are appended (supersede uses this).
fn create_into(
    ctx: &mut Ctx,
    p: &Value,
    forced_id: Option<String>,
    extra_relations: Vec<EntityRelation>,
    plan: &mut OpPlan,
) -> Option<String> {
    let Some(file_rel) = p_str(p, &["file"]) else {
        return ctx.err(
            "INVALID_OPERATION_PAYLOAD",
            "create-entry needs a `file`",
            None,
        );
    };
    if !is_canonical_markdown_path(&file_rel) {
        return ctx.err(
            "INVALID_OPERATION_PAYLOAD",
            format!(
                "File path must be a normalized scaffold-relative Markdown path, got \"{}\"",
                file_rel
            ),
            None,
        );
    }
    if ctx.vfs.scope.is_read_only(&file_rel) {
        ctx.diags.push(diag(
            "WRITE_SCOPE_VIOLATION",
            format!("{} is read-only to the wiki. Nothing was written", file_rel),
            file_rel.clone(),
        ));
        return None;
    }
    let Some(entity_type) = p_str(p, &["type"]) else {
        ctx.diags.push(diag(
            "INVALID_OPERATION_PAYLOAD",
            "create-entry needs a `type`",
            "",
        ));
        return None;
    };
    if !ctx.vfs.scope.registry.has(&entity_type) {
        ctx.diags.push(diag(
            "INVALID_ENTITY_TYPE",
            format!("Unknown entity type {:?}", entity_type),
            "",
        ));
        return None;
    }
    let Some(title) = p_str(p, &["title"]).filter(|t| !t.trim().is_empty()) else {
        ctx.diags.push(diag(
            "MISSING_ENTITY_TITLE",
            "create-entry needs a `title`",
            "",
        ));
        return None;
    };
    let status = p_str(p, &["status"]).unwrap_or_else(|| "in_flight".into());
    if !is_lifecycle_state(&status) {
        ctx.diags.push(diag(
            "INVALID_LIFECYCLE_STATE",
            format!("Unknown lifecycle state {:?}", status),
            "",
        ));
        return None;
    }
    let id = match mint_id(ctx, forced_id.or_else(|| p_str(p, &["id"])), &title) {
        Ok(id) => id,
        Err(e) => {
            ctx.diags.push(diag("INVALID_ENTITY_ID", e, ""));
            return None;
        }
    };
    let existing = ctx.vfs.parsed(&file_rel);
    if existing.as_ref().is_some_and(|f| f.entity(&id).is_some()) {
        // Already created (a resumed or replayed operation): settled.
        plan.created_ids.push(id.clone());
        return Some(id);
    }
    if ctx.vfs.id_exists(&id) {
        ctx.diags.push(diag(
            "DUPLICATE_ENTITY_ID",
            format!("Entity id {} is already claimed", id),
            "",
        ));
        return None;
    }
    // Relations, topics, sources, groundings.
    let mut relations = Vec::new();
    if let Some(v) = p_get(p, &["relations"]) {
        let Some(a) = v.as_array() else {
            ctx.diags.push(diag(
                "INVALID_OPERATION_PAYLOAD",
                "`relations` must be a list",
                "",
            ));
            return None;
        };
        for r in a {
            match relation_from(r) {
                Some(r) => relations.push(r),
                None => {
                    ctx.diags.push(diag(
                        "INVALID_OPERATION_PAYLOAD",
                        "A relation needs `type` and `target`",
                        "",
                    ));
                    return None;
                }
            }
        }
    }
    relations.extend(extra_relations);
    for r in &relations {
        if !is_relation_type(&r.rel_type) {
            ctx.diags.push(diag(
                "INVALID_RELATION_TYPE",
                format!(
                    "Unknown relation type \"{}\" (expected one of {})",
                    r.rel_type,
                    RELATION_TYPES.join(", ")
                ),
                "",
            ));
            return None;
        }
        if !ctx.vfs.id_exists(&r.target_id) {
            ctx.diags.push(diag(
                "INVALID_RELATION_TARGET",
                format!("Relation target {} does not exist", r.target_id),
                "",
            ));
            return None;
        }
    }
    let topics = match p_get(p, &["topics"]).map(string_list_value) {
        None => Vec::new(),
        Some(None) => {
            ctx.diags.push(diag(
                "INVALID_OPERATION_PAYLOAD",
                "`topics` must be a list",
                "",
            ));
            return None;
        }
        Some(Some(t)) => resolve_topics(ctx, t, Some(&id))?,
    };
    let sources = match sources_from(p_get(p, &["sources"])) {
        Ok(s) => s,
        Err(e) => {
            ctx.diags.push(diag("MALFORMED_SOURCE", e, ""));
            return None;
        }
    };
    if !validate_sources(ctx, &sources, &id) {
        return None;
    }
    let refs = match grounding_refs_from(p_get(p, &["groundsTo", "grounds_to"])) {
        Ok(r) => r,
        Err(e) => {
            ctx.diags.push(diag("MALFORMED_GROUNDING", e, ""));
            return None;
        }
    };
    let groundings = verify_groundings(ctx, refs)?;
    let body = p_str(p, &["body"]).unwrap_or_default();
    let body = body.trim_end_matches(['\r', '\n']).to_string();

    let text = existing
        .as_ref()
        .map(|f| f.text.clone())
        .unwrap_or_default();
    let eol = if text.is_empty() {
        "\n"
    } else {
        dominant_eol(&text)
    };
    let file_is_blank = text.trim().is_empty();
    let adopt = p_get(p, &["adopt"]).cloned();
    let heading_depth = p_get(p, &["headingDepth", "heading_depth"])
        .and_then(|v| v.as_u64())
        .map(|d| d as usize);
    let mut edits = Vec::new();

    if let Some(adopt) = adopt {
        let at = adopt.get("at").and_then(|v| v.as_str()).unwrap_or("");
        let Some(file) = existing.as_ref() else {
            ctx.diags.push(diag(
                "AMBIGUOUS_MIGRATION",
                format!("{} does not exist; nothing to adopt", file_rel),
                file_rel.clone(),
            ));
            return None;
        };
        match at {
            "file" => {
                let fe = file.file_entity().cloned();
                if fe.as_ref().is_some_and(|e| e.raw.contains_key("id")) {
                    ctx.diags.push(diag(
                        "INVALID_OPERATION_PAYLOAD",
                        format!("{} already has a file-level entity", file_rel),
                        file_rel.clone(),
                    ));
                    return None;
                }
                let fields = new_entity_fields(
                    &id,
                    p,
                    &status,
                    true,
                    &topics,
                    &relations,
                    &sources,
                    &groundings,
                );
                match &file.doc.frontmatter {
                    Some(fm) => {
                        for (k, v) in &fields {
                            if let Some(v) = v {
                                edits.push(set_key_edit(
                                    &text,
                                    fm.inner_start,
                                    fm.inner_end,
                                    k,
                                    v,
                                    eol,
                                ));
                            }
                        }
                    }
                    None => edits.push(Edit {
                        start: file.doc.bom,
                        end: file.doc.bom,
                        text: format!("---{eol}{}{eol}---{eol}", render_fields(&fields, eol)),
                        label: format!("frontmatter for {}", id),
                    }),
                }
                // The implicit entity becomes the adopted one.
                let mut targets = vec![id.clone()];
                if let Some(fe) = fe {
                    targets.push(fe.entity.id.clone());
                }
                commit_edits(
                    ctx,
                    &file_rel,
                    &text,
                    edits,
                    &targets,
                    std::slice::from_ref(&id),
                )?;
            }
            "heading" => {
                let ordinal = adopt
                    .get("ordinal")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(u64::MAX) as usize;
                let expected = adopt.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let Some(h) = file.doc.headings.get(ordinal) else {
                    ctx.diags.push(diag(
                        "AMBIGUOUS_MIGRATION",
                        format!(
                            "{} has {} heading(s); there is no heading {} to adopt",
                            file_rel,
                            file.doc.headings.len(),
                            ordinal
                        ),
                        file_rel.clone(),
                    ));
                    return None;
                };
                if h.title != expected {
                    ctx.diags.push(diag(
                        "AMBIGUOUS_MIGRATION",
                        format!(
                            "Heading {} of {} reads \"{}\", not \"{}\"",
                            ordinal, file_rel, h.title, expected
                        ),
                        file_rel.clone(),
                    ));
                    return None;
                }
                let fields = new_entity_fields(
                    &id,
                    p,
                    &status,
                    false,
                    &topics,
                    &relations,
                    &sources,
                    &groundings,
                );
                edits.push(Edit {
                    start: h.start,
                    end: h.start,
                    text: format!(
                        "<!-- kb:entity{eol}{}{eol}-->{eol}",
                        render_fields(&fields, eol)
                    ),
                    label: format!("marker for adopted entity {}", id),
                });
                // The enclosing entity's body shrinks to make room: it is a declared target.
                let mut targets = vec![id.clone()];
                for e in &file.entities {
                    if e.loc.body_start <= h.start
                        && h.start < e.loc.body_end.max(e.loc.body_start + 1)
                    {
                        targets.push(e.entity.id.clone());
                    }
                }
                commit_edits(
                    ctx,
                    &file_rel,
                    &text,
                    edits,
                    &targets,
                    std::slice::from_ref(&id),
                )?;
            }
            other => {
                ctx.diags.push(diag(
                    "INVALID_OPERATION_PAYLOAD",
                    format!("Unknown adopt target \"{}\"", other),
                    "",
                ));
                return None;
            }
        }
    } else if heading_depth.is_none() && file_is_blank {
        // New file-level entity.
        let fields = new_entity_fields(
            &id,
            p,
            &status,
            true,
            &topics,
            &relations,
            &sources,
            &groundings,
        );
        let mut doc = format!(
            "---{eol}{}{eol}---{eol}{eol}# {}{eol}",
            render_fields(&fields, eol),
            title
        );
        if !body.is_empty() {
            doc.push_str(&format!("{eol}{}{eol}", body.replace('\n', eol)));
        }
        edits.push(Edit {
            start: 0,
            end: text.len(),
            text: doc,
            label: format!("new file {}", file_rel),
        });
        commit_edits(ctx, &file_rel, &text, edits, &[], std::slice::from_ref(&id))?;
    } else {
        // Section entity (headingDepth defaults to 2 in an existing file).
        let depth = heading_depth.unwrap_or(2).clamp(1, 6);
        let offset =
            match insertion_offset(ctx, existing.as_ref(), p_get(p, &["insertAt", "insert_at"])) {
                Ok(o) => o,
                Err((code, msg)) => {
                    ctx.diags.push(diag(&code, msg, file_rel.clone()));
                    return None;
                }
            };
        let fields = new_entity_fields(
            &id,
            p,
            &status,
            false,
            &topics,
            &relations,
            &sources,
            &groundings,
        );
        let mut block = format!(
            "<!-- kb:entity{eol}{}{eol}-->{eol}{} {}{eol}",
            render_fields(&fields, eol),
            "#".repeat(depth),
            title
        );
        if !body.is_empty() {
            block.push_str(&format!("{eol}{}{eol}", body.replace('\n', eol)));
        }
        edits.push(Edit {
            start: offset,
            end: offset,
            text: padded_insertion(&text, offset, &block, eol),
            label: format!("entity {}", id),
        });
        commit_edits(ctx, &file_rel, &text, edits, &[], std::slice::from_ref(&id))?;
    }
    plan.touch(&file_rel);
    plan.created_ids.push(id.clone());
    plan.revisions.push(RevisionChange {
        entity_id: id.clone(),
        before: 0,
        after: 1,
    });
    Some(id)
}

fn heading_edit(file: &ParsedFile, pe: &ParsedEntity, title: &str) -> Option<Edit> {
    if pe.loc.heading_end <= pe.loc.heading_start {
        return None;
    }
    let text = &file.text;
    let original = &text[pe.loc.heading_start..pe.loc.heading_end];
    let term = if original.ends_with("\r\n") {
        "\r\n"
    } else if original.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    Some(Edit {
        start: pe.loc.heading_start,
        end: pe.loc.heading_end,
        text: format!(
            "{} {}{}",
            "#".repeat(pe.loc.heading_depth.max(1)),
            title,
            term
        ),
        label: format!("heading of {}", pe.entity.id),
    })
}

fn update_entry(
    ctx: &mut Ctx,
    file: &ParsedFile,
    pe: &ParsedEntity,
    p: &Value,
    plan: &mut OpPlan,
) -> Option<()> {
    let title = p_str(p, &["title"]);
    let summary = p_get(p, &["summary"]).cloned();
    let body = p_str(p, &["body"]);
    let append = match sources_from(p_get(p, &["appendSources", "append_sources"])) {
        Ok(s) => s,
        Err(e) => {
            ctx.diags.push(diag("MALFORMED_SOURCE", e, ""));
            return None;
        }
    };
    if title.is_none() && summary.is_none() && body.is_none() && append.is_empty() {
        ctx.diags.push(diag(
            "INVALID_OPERATION_PAYLOAD",
            "update-entry must change title, summary, body, or append evidence",
            "",
        ));
        return None;
    }
    let mut fields: Fields = Vec::new();
    let mut extra = Vec::new();
    if !append.is_empty() {
        let mut sources = pe.entity.sources.clone();
        let mut ids: HashSet<String> = sources.iter().map(|s| s.identity()).collect();
        for s in append {
            if ids.insert(s.identity()) {
                sources.push(s);
            }
        }
        if !validate_sources(ctx, &sources, &pe.entity.id) {
            return None;
        }
        if sources.len() > pe.entity.sources.len() {
            fields.push(("sources".into(), Some(yv(&sources))));
        }
    }
    if let Some(s) = summary {
        match s.as_str() {
            Some(s) => fields.push(("summary".into(), Some(yv(&s)))),
            None => fields.push(("summary".into(), None)),
        }
    }
    if let Some(t) = &title {
        let h = heading_edit(file, pe, t);
        if h.is_none() || pe.raw.contains_key("title") {
            fields.push(("title".into(), Some(yv(t))));
        }
        extra.extend(h);
    }
    if let Some(b) = body {
        let eol = dominant_eol(&file.text);
        let existing = &file.text[pe.loc.body_start..pe.loc.body_end];
        let tail_len = existing.len() - existing.trim_end_matches(['\r', '\n']).len();
        let tail = &existing[existing.len() - tail_len..];
        let trimmed = b
            .trim_end_matches(['\r', '\n'])
            .replace("\r\n", "\n")
            .replace('\n', eol);
        let tail = if tail.is_empty() {
            eol.to_string()
        } else {
            tail.to_string()
        };
        extra.push(Edit {
            start: pe.loc.body_start,
            end: pe.loc.body_end,
            text: if pe.loc.heading_end > pe.loc.heading_start
                || pe.loc.kind == MetadataKind::Frontmatter
            {
                format!("{eol}{}{}", trimmed, tail)
            } else {
                format!("{}{}", trimmed, tail)
            },
            label: format!("body of {}", pe.entity.id),
        });
    }
    mutate(ctx, file, pe, fields, extra, true, plan)
}

fn set_property(
    ctx: &mut Ctx,
    file: &ParsedFile,
    pe: &ParsedEntity,
    p: &Value,
    plan: &mut OpPlan,
) -> Option<()> {
    let property = p_str(p, &["property"]).unwrap_or_default();
    if property == "id" {
        // An id is never changed; a derived (implicit) id may be pinned to its current value
        // so it survives a file rename. `knobyte wiki migrate` uses this.
        let value = p.get("value").and_then(|v| v.as_str()).unwrap_or("");
        if pe.raw.contains_key("id") {
            ctx.diags.push(
                diag(
                    "INVALID_OPERATION_PAYLOAD",
                    format!("{} already has an explicit id; ids never change", pe.entity.id),
                    "",
                )
                .for_entity(&pe.entity.id),
            );
            return None;
        }
        if value != pe.entity.id {
            ctx.diags.push(
                diag(
                    "INVALID_OPERATION_PAYLOAD",
                    format!(
                        "A derived id can only be pinned to its current value {}",
                        pe.entity.id
                    ),
                    "",
                )
                .for_entity(&pe.entity.id),
            );
            return None;
        }
        let id = pe.entity.id.clone();
        return mutate(ctx, file, pe, vec![("id".into(), Some(yv(&id)))], Vec::new(), true, plan);
    }
    if !SETTABLE_PROPERTIES.contains(&property.as_str()) {
        ctx.diags.push(diag(
            "INVALID_OPERATION_PAYLOAD",
            format!(
                "Property \"{}\" is not settable. Settable: {} (id and revision are maintained by Knobyte)",
                property,
                SETTABLE_PROPERTIES.join(", ")
            ),
            "",
        ));
        return None;
    }
    let value = p.get("value").cloned().unwrap_or(Value::Null);
    let id = pe.entity.id.clone();
    let field: (String, Option<serde_yaml::Value>) = match property.as_str() {
        "title" => {
            let Some(t) = value.as_str() else {
                ctx.diags
                    .push(diag("INVALID_FIELD_TYPE", "`title` must be a string", ""));
                return None;
            };
            return update_entry(ctx, file, pe, &json!({ "title": t }), plan);
        }
        "status" => {
            let s = value.as_str().unwrap_or("");
            if !is_lifecycle_state(s) {
                ctx.diags.push(diag(
                    "INVALID_LIFECYCLE_STATE",
                    format!("Unknown lifecycle state {}. Use in_flight, promoted, deprecated or archived", value),
                    "",
                ));
                return None;
            }
            ("status".into(), Some(yv(&s)))
        }
        "type" => {
            let t = value.as_str().unwrap_or("");
            if !ctx.vfs.scope.registry.has(t) {
                ctx.diags.push(diag(
                    "INVALID_ENTITY_TYPE",
                    format!("Unknown entity type {}", value),
                    "",
                ));
                return None;
            }
            ("type".into(), Some(yv(&t)))
        }
        "summary" => match &value {
            Value::Null => ("summary".into(), None),
            Value::String(s) => ("summary".into(), Some(yv(s))),
            _ => {
                ctx.diags.push(diag(
                    "INVALID_FIELD_TYPE",
                    "`summary` must be a string or null",
                    "",
                ));
                return None;
            }
        },
        "topics" => {
            let Some(list) = string_list_value(&value).or_else(|| value.is_null().then(Vec::new))
            else {
                ctx.diags
                    .push(diag("INVALID_FIELD_TYPE", "`topics` must be a list", ""));
                return None;
            };
            let resolved = resolve_topics(ctx, list, Some(&id))?;
            (
                "topics".into(),
                (!resolved.is_empty()).then(|| yv(&resolved)),
            )
        }
        "aliases" => {
            let Some(list) = string_list_value(&value).or_else(|| value.is_null().then(Vec::new))
            else {
                ctx.diags
                    .push(diag("INVALID_FIELD_TYPE", "`aliases` must be a list", ""));
                return None;
            };
            ("aliases".into(), (!list.is_empty()).then(|| yv(&list)))
        }
        "metadata" => match &value {
            Value::Object(o) if !o.is_empty() => ("metadata".into(), Some(yv(&value))),
            Value::Object(_) | Value::Null => ("metadata".into(), None),
            _ => {
                ctx.diags
                    .push(diag("INVALID_FIELD_TYPE", "`metadata` must be a map", ""));
                return None;
            }
        },
        _ => unreachable!("checked against SETTABLE_PROPERTIES"),
    };
    mutate(ctx, file, pe, vec![field], Vec::new(), true, plan)
}

/// Relation edits: shorthand-key relations are edited under their own key.
fn relation_fields(pe: &ParsedEntity, list: &[EntityRelation]) -> Fields {
    let mut fields: Fields = Vec::new();
    let listed: Vec<&EntityRelation> = list.iter().filter(|r| r.origin.is_none()).collect();
    let had_list =
        pe.entity.relations.iter().any(|r| r.origin.is_none()) || pe.raw.contains_key("relations");
    if !listed.is_empty() || had_list {
        let owned: Vec<EntityRelation> = listed.into_iter().cloned().collect();
        fields.push(("relations".into(), (!owned.is_empty()).then(|| yv(&owned))));
    }
    let mut keys: Vec<String> = pe
        .entity
        .relations
        .iter()
        .filter_map(|r| r.origin.clone())
        .collect();
    keys.dedup();
    for key in keys {
        let targets: Vec<String> = list
            .iter()
            .filter(|r| r.origin.as_deref() == Some(key.as_str()))
            .map(|r| r.target_id.clone())
            .collect();
        let before: Vec<String> = pe
            .entity
            .relations
            .iter()
            .filter(|r| r.origin.as_deref() == Some(key.as_str()))
            .map(|r| r.target_id.clone())
            .collect();
        if targets != before {
            fields.push((key, (!targets.is_empty()).then(|| yv(&targets))));
        }
    }
    fields
}

fn would_cycle(ctx: &mut Ctx, superseder: &str, superseded: &str) -> bool {
    let all = ctx.vfs.all_entities();
    let succ: HashMap<&str, Vec<&str>> = all
        .iter()
        .map(|e| {
            (
                e.id.as_str(),
                e.relations
                    .iter()
                    .filter(|r| r.rel_type == "supersedes")
                    .map(|r| r.target_id.as_str())
                    .collect(),
            )
        })
        .collect();
    let mut seen = HashSet::new();
    let mut queue = vec![superseded];
    while let Some(cur) = queue.pop() {
        if cur == superseder {
            return true;
        }
        if !seen.insert(cur) {
            continue;
        }
        if let Some(next) = succ.get(cur) {
            queue.extend(next.iter().copied());
        }
    }
    false
}

fn add_relation(
    ctx: &mut Ctx,
    file: &ParsedFile,
    pe: &ParsedEntity,
    p: &Value,
    plan: &mut OpPlan,
) -> Option<()> {
    // `wiki migrate`'s legacy-edge conversion: several `related_to` relations at once, with
    // the frontmatter `edges` list rewritten to what could not be converted.
    if let Some(list) = p_get(p, &["relations"]).and_then(|v| v.as_array()) {
        return add_relations_from_edges(ctx, file, pe, list, p.get("remainingEdges"), plan);
    }
    let Some(rel) = p_get(p, &["relation"]).and_then(relation_from) else {
        ctx.diags.push(diag(
            "INVALID_OPERATION_PAYLOAD",
            "add-relation needs `relation: {type, target}`",
            "",
        ));
        return None;
    };
    let id = pe.entity.id.clone();
    if !is_relation_type(&rel.rel_type) {
        return ctx.err(
            "INVALID_RELATION_TYPE",
            format!("Unknown relation type \"{}\"", rel.rel_type),
            Some(&id),
        );
    }
    if rel.target_id == id {
        return ctx.err(
            "SELF_RELATION",
            format!("{} cannot relate to itself", id),
            Some(&id),
        );
    }
    if pe
        .entity
        .relations
        .iter()
        .any(|r| r.rel_type == rel.rel_type && r.target_id == rel.target_id)
    {
        return ctx.err(
            "DUPLICATE_RELATION",
            format!(
                "{} already declares {} -> {}",
                id, rel.rel_type, rel.target_id
            ),
            Some(&id),
        );
    }
    if !ctx.vfs.id_exists(&rel.target_id) {
        return ctx.err(
            "INVALID_RELATION_TARGET",
            format!("Relation target {} does not exist", rel.target_id),
            Some(&id),
        );
    }
    if rel.rel_type == "supersedes" && would_cycle(ctx, &id, &rel.target_id) {
        return ctx.err(
            "SUPERSESSION_CYCLE",
            format!(
                "{} cannot supersede {}: that closes a supersession cycle",
                id, rel.target_id
            ),
            Some(&id),
        );
    }
    let mut list = pe.entity.relations.clone();
    list.push(rel);
    let fields = relation_fields(pe, &list);
    mutate(ctx, file, pe, fields, Vec::new(), true, plan)
}

fn add_relations_from_edges(
    ctx: &mut Ctx,
    file: &ParsedFile,
    pe: &ParsedEntity,
    list: &[Value],
    remaining_edges: Option<&Value>,
    plan: &mut OpPlan,
) -> Option<()> {
    let id = pe.entity.id.clone();
    let mut relations = pe.entity.relations.clone();
    for v in list {
        let Some(rel) = relation_from(v) else {
            ctx.diags.push(diag(
                "INVALID_OPERATION_PAYLOAD",
                "add-relation `relations` entries need {type, target}",
                "",
            ));
            return None;
        };
        if !is_relation_type(&rel.rel_type) {
            return ctx.err(
                "INVALID_RELATION_TYPE",
                format!("Unknown relation type \"{}\"", rel.rel_type),
                Some(&id),
            );
        }
        if rel.target_id == id || !ctx.vfs.id_exists(&rel.target_id) {
            return ctx.err(
                "INVALID_RELATION_TARGET",
                format!("Relation target {} does not exist", rel.target_id),
                Some(&id),
            );
        }
        if !relations.iter().any(|r| r.rel_type == rel.rel_type && r.target_id == rel.target_id) {
            relations.push(rel);
        }
    }
    let mut fields = relation_fields(pe, &relations);
    // Kept edges are written back `target` first.
    let remaining = remaining_edges.and_then(|v| v.as_array()).filter(|a| !a.is_empty()).map(|a| {
        let items: Vec<serde_yaml::Value> = a
            .iter()
            .map(|edge| match edge.as_object() {
                Some(o) => {
                    let mut m = serde_yaml::Mapping::new();
                    let mut keys: Vec<&String> = o.keys().collect();
                    keys.sort_by_key(|k| (k.as_str() != "target", k.as_str() != "condition"));
                    for k in keys {
                        m.insert(yv(k), yv(&o[k]));
                    }
                    serde_yaml::Value::Mapping(m)
                }
                None => yv(edge),
            })
            .collect();
        serde_yaml::Value::Sequence(items)
    });
    fields.push(("edges".into(), remaining));
    mutate(ctx, file, pe, fields, Vec::new(), true, plan)
}

fn remove_relation(
    ctx: &mut Ctx,
    file: &ParsedFile,
    pe: &ParsedEntity,
    p: &Value,
    plan: &mut OpPlan,
) -> Option<()> {
    let rel_type = p_str(p, &["type"]).unwrap_or_default();
    let target = p_str(p, &["target", "target_id", "targetId"]).unwrap_or_default();
    let list: Vec<EntityRelation> = pe
        .entity
        .relations
        .iter()
        .filter(|r| !(r.rel_type == rel_type && r.target_id == target))
        .cloned()
        .collect();
    if list.len() == pe.entity.relations.len() {
        return ctx.err(
            "INVALID_RELATION_TARGET",
            format!(
                "{} has no {} relation to {}",
                pe.entity.id, rel_type, target
            ),
            Some(&pe.entity.id),
        );
    }
    let fields = relation_fields(pe, &list);
    mutate(ctx, file, pe, fields, Vec::new(), true, plan)
}

fn add_source(
    ctx: &mut Ctx,
    file: &ParsedFile,
    pe: &ParsedEntity,
    p: &Value,
    plan: &mut OpPlan,
) -> Option<()> {
    let source =
        match p_get(p, &["source"]).map(|v| serde_json::from_value::<WikiSource>(v.clone())) {
            Some(Ok(s)) => s,
            _ => {
                ctx.diags.push(diag(
                    "MALFORMED_SOURCE",
                    "add-source needs `source: {type, ref?, note?}`",
                    "",
                ));
                return None;
            }
        };
    if pe
        .entity
        .sources
        .iter()
        .any(|s| s.identity() == source.identity())
    {
        return ctx.err(
            "DUPLICATE_SOURCE",
            format!("{} already cites {}", pe.entity.id, source.identity()),
            Some(&pe.entity.id),
        );
    }
    let mut sources = pe.entity.sources.clone();
    sources.push(source);
    if !validate_sources(ctx, &sources, &pe.entity.id) {
        return None;
    }
    mutate(
        ctx,
        file,
        pe,
        vec![("sources".into(), Some(yv(&sources)))],
        Vec::new(),
        true,
        plan,
    )
}

fn remove_source(
    ctx: &mut Ctx,
    file: &ParsedFile,
    pe: &ParsedEntity,
    p: &Value,
    plan: &mut OpPlan,
) -> Option<()> {
    let identity = p_str(p, &["sourceIdentity", "source_identity"]).or_else(|| {
        p_get(p, &["source"])
            .and_then(|v| serde_json::from_value::<WikiSource>(v.clone()).ok())
            .map(|s| s.identity())
    });
    let Some(identity) = identity else {
        ctx.diags.push(diag(
            "INVALID_OPERATION_PAYLOAD",
            "remove-source needs `sourceIdentity` (type|repository|ref) or `source`",
            "",
        ));
        return None;
    };
    let remaining: Vec<WikiSource> = pe
        .entity
        .sources
        .iter()
        .filter(|s| s.identity() != identity)
        .cloned()
        .collect();
    if remaining.len() == pe.entity.sources.len() {
        return ctx.err(
            "MALFORMED_SOURCE",
            format!(
                "{} cites no source with identity {}",
                pe.entity.id, identity
            ),
            Some(&pe.entity.id),
        );
    }
    mutate(
        ctx,
        file,
        pe,
        vec![(
            "sources".into(),
            (!remaining.is_empty()).then(|| yv(&remaining)),
        )],
        Vec::new(),
        true,
        plan,
    )
}

/// Legacy anchor comment prefixes rewritten to `<!-- kb-ground:` by `normalizeAnchors`.
pub const LEGACY_ANCHOR_PREFIXES: [&str; 2] = ["<!-- grounds:", "<!-- kb-anchor:"];
pub const ANCHOR_PREFIX: &str = "<!-- kb-ground:";

fn normalize_anchor_text(line: &str) -> String {
    for p in LEGACY_ANCHOR_PREFIXES {
        if let Some(i) = line.find(p) {
            return format!("{}{}{}", &line[..i], ANCHOR_PREFIX, &line[i + p.len()..]);
        }
    }
    line.to_string()
}

type Baselines = HashMap<String, (Option<String>, Option<String>)>;

/// `(ref -> (body_hash, fingerprint))` committed with object entries of a `groundsTo` payload.
fn grounding_baselines_from(v: Option<&Value>) -> Baselines {
    let mut out = HashMap::new();
    for g in v.and_then(|v| v.as_array()).into_iter().flatten() {
        let Value::Object(o) = g else { continue };
        let Some(r) = str_field(o, &["ref", "node_id", "node", "nodeId"]) else {
            continue;
        };
        let body_hash = str_field(o, &["body_hash", "bodyHash"]).filter(|s| !s.is_empty());
        let fingerprint = str_field(o, &["fingerprint"]).filter(|s| !s.is_empty());
        if body_hash.is_some() || fingerprint.is_some() {
            out.insert(r.trim().to_string(), (body_hash, fingerprint));
        }
    }
    out
}

/// `set-grounding`: replace the entity's groundings. Payload: `groundsTo` (references, or
/// `{ref, body_hash, fingerprint}` maps whose committed baseline is kept), `updateAnchors`
/// (rewrite a single anchor to the single new reference), `keepAnchors` (leave inline anchors
/// that are not in `groundsTo` alone instead of dropping them) and `normalizeAnchors` (rewrite
/// legacy `<!-- grounds:` / `<!-- kb-anchor:` comments to `<!-- kb-ground:`). With only
/// `normalizeAnchors`, the metadata is not touched.
fn set_grounding(
    ctx: &mut Ctx,
    file: &ParsedFile,
    pe: &ParsedEntity,
    p: &Value,
    plan: &mut OpPlan,
) -> Option<()> {
    let flag = |keys: &[&str]| p_get(p, keys).and_then(|v| v.as_bool()).unwrap_or(false);
    let update_anchors = flag(&["updateAnchors", "update_anchors"]);
    let keep_anchors = flag(&["keepAnchors", "keep_anchors"]);
    let normalize = flag(&["normalizeAnchors", "normalize_anchors"]);
    let refs_value = p_get(p, &["groundsTo", "grounds_to"]);
    let anchors_only = refs_value.is_none() && normalize;
    let refs = if anchors_only {
        pe.entity.grounds_to.clone()
    } else {
        let refs = match grounding_refs_from(refs_value) {
            Ok(r) => r,
            Err(e) => {
                ctx.diags.push(diag("MALFORMED_GROUNDING", e, ""));
                return None;
            }
        };
        verify_groundings(ctx, refs)?
    };
    let baselines = grounding_baselines_from(refs_value);
    let mut extra = Vec::new();
    // Inline anchors also ground the entity: keep those still wanted, rewrite or drop the rest.
    let anchors: Vec<_> = file
        .doc
        .anchors
        .iter()
        .filter(|a| a.start >= pe.loc.body_start && a.start < pe.loc.body_end)
        .filter(|a| pe.anchor_refs.contains(&a.reference))
        .collect();
    let rewrite_one = update_anchors && pe.entity.grounds_to.len() == 1 && refs.len() == 1;
    let mut changed_anchor = false;
    for a in anchors {
        let line = &file.text[a.start..a.end];
        let kept = refs.contains(&a.reference) || keep_anchors || anchors_only;
        let text = if kept {
            if normalize {
                normalize_anchor_text(line)
            } else {
                line.to_string()
            }
        } else if rewrite_one {
            let t = line.replacen(&a.reference, &refs[0], 1);
            if normalize {
                normalize_anchor_text(&t)
            } else {
                t
            }
        } else {
            String::new()
        };
        if text == line {
            continue;
        }
        changed_anchor = true;
        extra.push(Edit {
            start: a.start,
            end: a.end,
            text,
            label: format!("anchor {}", a.reference),
        });
    }
    if anchors_only {
        if !changed_anchor {
            ctx.diags.push(
                diag(
                    "INVALID_OPERATION_PAYLOAD",
                    format!(
                        "{} has no legacy grounding anchors to normalize",
                        pe.entity.id
                    ),
                    "",
                )
                .for_entity(&pe.entity.id),
            );
            return None;
        }
        return mutate(ctx, file, pe, Vec::new(), extra, true, plan);
    }
    let entries: Vec<serde_yaml::Value> = refs
        .iter()
        .map(|r| match baselines.get(r) {
            Some((body_hash, fingerprint)) => {
                let mut m = serde_yaml::Mapping::new();
                m.insert("ref".into(), r.as_str().into());
                if let Some(h) = body_hash {
                    m.insert("body_hash".into(), h.as_str().into());
                }
                if let Some(f) = fingerprint {
                    m.insert("fingerprint".into(), f.as_str().into());
                }
                serde_yaml::Value::Mapping(m)
            }
            None => serde_yaml::Value::String(r.clone()),
        })
        .collect();
    let field = (
        "grounds_to".to_string(),
        (!entries.is_empty()).then_some(serde_yaml::Value::Sequence(entries)),
    );
    mutate(ctx, file, pe, vec![field], extra, true, plan)
}

fn supersede_entry(
    ctx: &mut Ctx,
    file: &ParsedFile,
    pe: &ParsedEntity,
    p: &Value,
    plan: &mut OpPlan,
) -> Option<()> {
    let old = pe.entity.id.clone();
    let replacement_id = p_str(p, &["replacementId", "replacement_id"]);
    let replacement = p_get(p, &["replacement"]).cloned();
    if replacement_id.is_some() == replacement.is_some() {
        ctx.diags.push(diag(
            "INVALID_OPERATION_PAYLOAD",
            "supersede-entry needs exactly one of `replacementId` (an existing entity) or `replacement` (one to create)",
            "",
        ));
        return None;
    }
    mutate(
        ctx,
        file,
        pe,
        vec![("status".into(), Some(yv(&"deprecated")))],
        Vec::new(),
        true,
        plan,
    )?;
    let note = p_str(p, &["note"]);
    if let Some(rep) = replacement {
        let rel = EntityRelation {
            rel_type: "supersedes".into(),
            target_id: old.clone(),
            note,
            ..Default::default()
        };
        let forced = ctx.forced_ids.first().cloned();
        create_into(ctx, &rep, forced, vec![rel], plan)?;
        return Some(());
    }
    let rid = replacement_id.unwrap_or_default();
    let Some((rfile, rpe)) = ctx.vfs.locate(&rid) else {
        return ctx.err(
            "ENTITY_NOT_FOUND",
            format!("No entity {} to supersede {} with", rid, old),
            Some(&rid),
        );
    };
    if rpe
        .entity
        .relations
        .iter()
        .any(|r| r.rel_type == "supersedes" && r.target_id == old)
    {
        return ctx.err(
            "DUPLICATE_RELATION",
            format!("{} already supersedes {}", rid, old),
            Some(&rid),
        );
    }
    if would_cycle(ctx, &rid, &old) {
        return ctx.err(
            "SUPERSESSION_CYCLE",
            format!(
                "{} cannot supersede {}: that closes a supersession cycle",
                rid, old
            ),
            Some(&rid),
        );
    }
    let mut list = rpe.entity.relations.clone();
    list.push(EntityRelation {
        rel_type: "supersedes".into(),
        target_id: old,
        note,
        ..Default::default()
    });
    let fields = relation_fields(&rpe, &list);
    mutate(ctx, &rfile, &rpe, fields, Vec::new(), true, plan)
}

fn move_entry(
    ctx: &mut Ctx,
    file: &ParsedFile,
    pe: &ParsedEntity,
    p: &Value,
    plan: &mut OpPlan,
) -> Option<()> {
    let id = pe.entity.id.clone();
    if pe.loc.kind != MetadataKind::Marker {
        return ctx.err(
            "INVALID_OPERATION_PAYLOAD",
            format!(
                "{} is a file-level entity; move the file instead (only section entities move)",
                id
            ),
            Some(&id),
        );
    }
    let Some(dest) = p_str(p, &["file"]) else {
        ctx.diags.push(diag(
            "INVALID_OPERATION_PAYLOAD",
            "move-entry needs `file`",
            "",
        ));
        return None;
    };
    if !is_canonical_markdown_path(&dest) {
        ctx.diags.push(diag(
            "INVALID_OPERATION_PAYLOAD",
            format!(
                "\"{}\" is not a normalized scaffold-relative Markdown path",
                dest
            ),
            "",
        ));
        return None;
    }
    if dest == file.path {
        return ctx.err(
            "INVALID_OPERATION_PAYLOAD",
            format!("{} is already in {}", id, dest),
            Some(&id),
        );
    }
    let dest_file = ctx.vfs.parsed(&dest);
    let already = dest_file.as_ref().is_some_and(|f| f.entity(&id).is_some());
    let text = &file.text;
    let block = text[pe.loc.metadata_start..pe.loc.body_end]
        .trim_end_matches(['\r', '\n'])
        .to_string();
    // Remove from the source, including blank lines that separated it from what precedes.
    let mut start = pe.loc.metadata_start;
    let b = text.as_bytes();
    while start > 0 && (b[start - 1] == b'\n' || b[start - 1] == b'\r') {
        start -= 1;
    }
    let eol = dominant_eol(text);
    if start > 0 {
        start = (start + eol.len()).min(pe.loc.metadata_start);
    }
    ctx.moving = true;
    let removed = commit_edits(
        ctx,
        &file.path,
        text,
        vec![Edit {
            start,
            end: pe.loc.body_end,
            text: String::new(),
            label: format!("remove {} from {}", id, file.path),
        }],
        std::slice::from_ref(&id),
        &[],
    );
    ctx.moving = false;
    removed?;
    plan.touch(&file.path);
    if !already {
        let dtext = dest_file
            .as_ref()
            .map(|f| f.text.clone())
            .unwrap_or_default();
        let deol = if dtext.is_empty() {
            eol
        } else {
            dominant_eol(&dtext)
        };
        let offset = match insertion_offset(
            ctx,
            dest_file.as_ref(),
            p_get(p, &["insertAt", "insert_at"]),
        ) {
            Ok(o) => o,
            Err((code, msg)) => {
                ctx.diags.push(diag(&code, msg, dest.clone()));
                return None;
            }
        };
        let block_text = format!("{}{}", block, deol);
        commit_edits(
            ctx,
            &dest,
            &dtext,
            vec![Edit {
                start: offset,
                end: offset,
                text: padded_insertion(&dtext, offset, &block_text, deol),
                label: format!("insert {} into {}", id, dest),
            }],
            &[],
            std::slice::from_ref(&id),
        )?;
        plan.touch(&dest);
    }
    plan.entity_ids.push(id.clone());
    plan.revisions.push(RevisionChange {
        entity_id: id,
        before: pe.entity.revision,
        after: pe.entity.revision,
    });
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_hash_is_key_order_independent() {
        let a = json!({"b": 1, "a": {"y": 2, "x": 1}});
        let b = json!({"a": {"x": 1, "y": 2}, "b": 1});
        assert_eq!(
            payload_hash("set-property", &a),
            payload_hash("set-property", &b)
        );
    }

    #[test]
    fn diff_shows_changed_lines() {
        let d = line_diff("a\nb\nc\n", "a\nB\nc\n");
        assert!(d.contains("-b") && d.contains("+B"));
        assert_eq!(d, "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n");
        assert_eq!(line_diff("a\nb\n", "a\nb\n"), "");
    }

    #[test]
    fn diff_keeps_separate_changes_in_minimal_hunks() {
        // Two edits 12 lines apart inside one block: two hunks, unchanged lines kept as
        // context only (never removed and re-added).
        let old: Vec<String> = (1..=20).map(|i| format!("line{}", i)).collect();
        let mut new = old.clone();
        new[2] = "changed3".into();
        new[15] = "changed16".into();
        new.insert(16, "added".into());
        let d = line_diff(&old.join("\n"), &new.join("\n"));
        assert_eq!(d.matches("@@ ").count(), 2, "{}", d);
        assert!(d.starts_with("@@ -1,6 +1,6 @@\n line1\n line2\n-line3\n+changed3\n line4\n"), "{}", d);
        assert!(d.contains("@@ -13,7 +13,8 @@\n line13\n line14\n line15\n-line16\n+changed16\n+added\n line17\n"), "{}", d);
        assert_eq!(d.lines().filter(|l| l.starts_with('-')).count(), 2);
        assert_eq!(d.lines().filter(|l| l.starts_with('+')).count(), 3);
    }

    #[test]
    fn diff_of_created_and_emptied_files() {
        assert_eq!(line_diff("", "x\ny\n"), "@@ -0,0 +1,2 @@\n+x\n+y\n");
        assert_eq!(line_diff("x\n", ""), "@@ -1,1 +0,0 @@\n-x\n");
        // A pure insertion between distant lines.
        let d = line_diff("a\nb\nc\nd\ne\nf\ng\nh\n", "a\nb\nc\nd\nNEW\ne\nf\ng\nh\n");
        assert_eq!(d, "@@ -2,6 +2,7 @@\n b\n c\n d\n+NEW\n e\n f\n g\n");
    }
}
