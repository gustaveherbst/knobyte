//! `knobyte wiki migrate`: bring a scaffold written by an older Knobyte up to the current
//! entity format, through the typed operation machinery and its audit log.
//!
//! Older Knobyte formats this rewrites (never another tool's format):
//!
//! | legacy shape | rewritten to |
//! |---|---|
//! | `status: accepted` / `draft` / `superseded` / ... | the lifecycle state it was read as |
//! | `type: document` (or no type, inferred from the path) | an explicit registered type (`document` -> `guide`) |
//! | an id derived from the file name or heading | the same id, pinned in the metadata |
//! | `grounds_to` `node_id:` / `node:` mappings | plain references (baselines kept as `{ref, body_hash, fingerprint}`) |
//! | legacy hashed graph ids (`kind:<32 hex>`) | readable `kind:path:qualified_name` (needs the code graph) |
//! | `<!-- grounds: -->` / `<!-- kb-anchor: -->` anchors | `<!-- kb-ground: -->` |
//!
//! The run has three phases: **inventory** (every corpus file, classified), **plan**
//! (one operation per legacy finding, with a deterministic `opId` derived from the finding and
//! the entity's content hash, and a content-hash precondition) and **apply** (the batch, all
//! or nothing, recorded in `events/operations.jsonl`). Re-running an applied migration is a
//! no-op. Anything the migration cannot rewrite safely is an *abstention*: left untouched and
//! reported with its reason.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::graph::grounding::{
    parse_grounding_ref, readable_ref_for, resolve_grounding_ref, ParsedRef, RefResolution,
};
use crate::wiki::diagnostics::{diag, DiagExt};
use crate::wiki::models::{normalize_lifecycle, WikiDiagnostic, GROUNDING_ORIGIN_FRONTMATTER};
use crate::wiki::ops::{
    apply_operations_with, ApplyOptions, ApplyReport, OpActor, LEGACY_ANCHOR_PREFIXES,
};
use crate::wiki::parser::{infer_type_from_path, ParsedEntity, ParsedFile};
use crate::wiki::scope::WikiScope;
use crate::wiki::validate::parse_corpus;

/// The type the legacy catch-all `document` becomes.
pub const DOCUMENT_REPLACEMENT_TYPE: &str = "guide";

/// Finding kinds, in the order their operations are applied to one entity.
pub const MIGRATION_KINDS: [&str; 7] = [
    "implicit_id",
    "implicit_type",
    "legacy_type",
    "legacy_status",
    "legacy_edges",
    "grounding_shape",
    "legacy_anchor",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MigrationItem {
    pub kind: String,
    pub file: String,
    pub entity_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    pub op_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Abstention {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    pub reason: String,
}

/// One inventoried file: `current`, `legacy` (has findings), `abstained` (only abstentions),
/// `read_only` (team-owned or `wiki.readOnly`; never rewritten) or `unparseable`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InventoryFile {
    pub file: String,
    pub class: String,
    pub entities: usize,
    pub findings: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MigrationPlan {
    pub files_scanned: usize,
    pub entities_scanned: usize,
    pub inventory: Vec<InventoryFile>,
    pub items: Vec<MigrationItem>,
    pub abstentions: Vec<Abstention>,
    /// Operation envelopes, in application order.
    pub operations: Vec<Value>,
    /// The planned batch does not apply cleanly (see diagnostics); nothing would be written.
    pub blocked: bool,
    /// Files the batch would change.
    pub changed_files: Vec<String>,
    pub diagnostics: Vec<WikiDiagnostic>,
}

impl MigrationPlan {
    pub fn is_current(&self) -> bool {
        self.items.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationResult {
    pub applied: bool,
    pub dry_run: bool,
    pub plan: MigrationPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<ApplyReport>,
}

pub struct MigrationOptions<'a> {
    pub scope: &'a WikiScope,
    /// Code graph database; needed to convert legacy hashed grounding ids.
    pub graph_db: Option<&'a Path>,
}

/// The actor every migration operation is recorded under.
pub fn migration_actor() -> OpActor {
    OpActor {
        kind: "system".into(),
        id: "migration".into(),
        session_id: None,
    }
}

/// Deterministic operation id of one finding: the same finding on the same entity text always
/// yields the same id, so a re-run replays instead of re-applying.
pub fn migration_op_id(kind: &str, file: &str, entity_id: &str, content_hash: &str) -> String {
    let mut h = Sha256::new();
    h.update(
        format!("knobyte-migration\u{0}{kind}\u{0}{file}\u{0}{entity_id}\u{0}{content_hash}")
            .as_bytes(),
    );
    format!("mig_{}", &hex::encode(h.finalize())[..40])
}

struct EntityPlan<'a> {
    file: &'a ParsedFile,
    pe: &'a ParsedEntity,
    items: Vec<(MigrationItem, Value)>,
}

impl EntityPlan<'_> {
    fn push(
        &mut self,
        kind: &str,
        from: Option<String>,
        to: Option<String>,
        payload: Value,
        op_type: &str,
    ) {
        let e = &self.pe.entity;
        let op_id = migration_op_id(kind, &self.file.path, &e.id, &e.content_hash);
        let item = MigrationItem {
            kind: kind.to_string(),
            file: self.file.path.clone(),
            entity_id: e.id.clone(),
            from,
            to,
            op_id: op_id.clone(),
        };
        let envelope = json!({
            "opId": op_id,
            "type": op_type,
            "entityId": e.id,
            "actor": { "kind": "system", "id": "migration" },
            "reason": format!("wiki migrate: {}", kind),
            "payload": payload,
        });
        self.items.push((item, envelope));
    }
}

fn raw_str(pe: &ParsedEntity, key: &str) -> Option<String> {
    pe.raw.get(key).and_then(|v| match v {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Null => None,
        other => Some(other.to_string()),
    })
}

/// Grounding findings of one entity: `Ok(Some(payload))` to rewrite, `Ok(None)` when current,
/// `Err(reason)` to abstain.
fn plan_groundings(
    pe: &ParsedEntity,
    graph: Option<&Connection>,
) -> Result<Option<(Value, String)>, String> {
    let Some(Value::Array(entries)) = pe.raw.get("grounds_to") else {
        return Ok(None);
    };
    let mut needs = false;
    let mut out: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let committed: HashMap<&str, _> = pe
        .entity
        .committed_groundings
        .iter()
        .filter(|c| c.origin == GROUNDING_ORIGIN_FRONTMATTER)
        .map(|c| (c.reference.as_str(), c))
        .collect();
    let mut converted = Vec::new();
    for entry in entries {
        let (reference, mapped) = match entry {
            Value::String(s) => (s.trim().to_string(), false),
            Value::Object(o) => {
                let mapped = o.get("ref").and_then(|v| v.as_str()).is_none();
                let r = ["ref", "node_id", "node"]
                    .iter()
                    .find_map(|k| o.get(*k).and_then(|v| v.as_str()))
                    .unwrap_or("")
                    .trim()
                    .to_string();
                (r, mapped)
            }
            _ => return Err("a `grounds_to` entry is malformed; fix it by hand".into()),
        };
        if reference.is_empty() {
            return Err("a `grounds_to` entry has no reference; fix it by hand".into());
        }
        needs |= mapped;
        let readable = match parse_grounding_ref(&reference) {
            ParsedRef::LegacyId(_) => {
                needs = true;
                let Some(gc) = graph else {
                    return Err(format!(
                        "legacy grounding id {} needs the code graph to convert (run `knobyte graph rebuild`)",
                        reference
                    ));
                };
                match resolve_grounding_ref(gc, &reference) {
                    Ok(RefResolution::Resolved(node)) => {
                        let r = readable_ref_for(&node);
                        converted.push(format!("{} -> {}", reference, r));
                        r
                    }
                    _ => {
                        return Err(format!(
                            "legacy grounding id {} does not resolve in the code graph (run `knobyte sync`)",
                            reference
                        ))
                    }
                }
            }
            _ => reference.clone(),
        };
        if !seen.insert(readable.clone()) {
            continue;
        }
        let baseline = committed.get(reference.as_str());
        match baseline.filter(|c| c.body_hash.is_some() || c.fingerprint.is_some()) {
            Some(c) => {
                let mut m = serde_json::Map::new();
                m.insert("ref".into(), json!(readable));
                if let Some(h) = &c.body_hash {
                    m.insert("body_hash".into(), json!(h));
                }
                if let Some(f) = &c.fingerprint {
                    m.insert("fingerprint".into(), json!(f));
                }
                out.push(Value::Object(m));
            }
            None => out.push(json!(readable)),
        }
    }
    if !needs {
        return Ok(None);
    }
    // Every reference written by an operation must resolve when a graph exists.
    if let Some(gc) = graph {
        for v in &out {
            let r = v
                .as_str()
                .or_else(|| v.get("ref").and_then(|x| x.as_str()))
                .unwrap_or("");
            if !matches!(resolve_grounding_ref(gc, r), Ok(RefResolution::Resolved(_))) {
                return Err(format!(
                    "grounding {} does not resolve in the code graph; fix or re-ground it before migrating its shape",
                    r
                ));
            }
        }
    }
    let summary = if converted.is_empty() {
        "node_id mappings".to_string()
    } else {
        converted.join(", ")
    };
    Ok(Some((
        json!({ "groundsTo": out, "keepAnchors": true }),
        summary,
    )))
}

/// Normalize a scaffold-relative path: drop `./` and `..` segments (clamped at the root).
fn normalize_rel(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

/// The file whose entity a legacy `edges[].target` path names: scaffold-relative (the legacy
/// convention, optionally prefixed `.knobyte/`), else relative to the declaring file.
fn resolve_edge_target<'a>(
    target: &str,
    declaring_file: &str,
    file_ids: &'a HashMap<String, String>,
) -> Option<&'a String> {
    let t = target.trim();
    let t = t.split('#').next().unwrap_or(t);
    let lookup = |p: String| {
        let p = p.strip_prefix(".knobyte/").map(str::to_string).unwrap_or(p);
        file_ids.get(&p)
    };
    let dir = declaring_file.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    lookup(normalize_rel(t)).or_else(|| lookup(normalize_rel(&format!("{}/{}", dir, t))))
}

/// Legacy frontmatter `edges: [{target: context/x.md, condition: ...}]` become
/// `related_to` relations to the entity at that path (the condition kept as the note).
/// Targets that name no wiki entity stay in `edges`. `None` when there is nothing to convert.
fn plan_legacy_edges(
    f: &ParsedFile,
    pe: &ParsedEntity,
    file_ids: &HashMap<String, String>,
) -> Option<(Value, String)> {
    let Some(Value::Array(edges)) = pe.raw.get("edges") else {
        return None;
    };
    let mut relations: Vec<Value> = Vec::new();
    let mut remaining: Vec<Value> = Vec::new();
    let mut targets: Vec<String> = Vec::new();
    for edge in edges {
        let target = edge
            .get("target")
            .and_then(|v| v.as_str())
            .or_else(|| edge.as_str())
            .unwrap_or("");
        let resolved = resolve_edge_target(target, &f.path, file_ids)
            .filter(|id| **id != pe.entity.id);
        let Some(id) = resolved else {
            remaining.push(edge.clone());
            continue;
        };
        if targets.contains(id)
            || pe.entity.relations.iter().any(|r| r.rel_type == "related_to" && r.target_id == *id)
        {
            continue;
        }
        targets.push(id.clone());
        let mut rel = serde_json::Map::new();
        rel.insert("type".into(), json!("related_to"));
        rel.insert("target_id".into(), json!(id));
        if let Some(c) = edge.get("condition").and_then(|v| v.as_str()).filter(|c| !c.trim().is_empty()) {
            rel.insert("note".into(), json!(c.trim()));
        }
        relations.push(Value::Object(rel));
    }
    // Nothing convertible and nothing to drop: leave the edges as they are.
    if relations.is_empty() && remaining.len() == edges.len() {
        return None;
    }
    let summary = if targets.is_empty() {
        "edges (already related)".to_string()
    } else {
        format!("edges -> related_to {}", targets.join(", "))
    };
    Some((json!({ "relations": relations, "remainingEdges": remaining }), summary))
}

fn has_legacy_grounding_shape(pe: &ParsedEntity) -> bool {
    let Some(Value::Array(entries)) = pe.raw.get("grounds_to") else {
        return false;
    };
    entries.iter().any(|e| match e {
        Value::Object(o) => o.get("ref").is_none(),
        Value::String(s) => matches!(parse_grounding_ref(s), ParsedRef::LegacyId(_)),
        _ => false,
    })
}

fn has_legacy_anchor(file: &ParsedFile, pe: &ParsedEntity) -> bool {
    file.doc.anchors.iter().any(|a| {
        a.start >= pe.loc.body_start
            && a.start < pe.loc.body_end
            && LEGACY_ANCHOR_PREFIXES
                .iter()
                .any(|p| file.text[a.start..a.end].contains(p))
    })
}

/// Knobyte-owned files that hold no project knowledge (`knobyte update` refreshes them in
/// place, `knobyte pattern add` appends to the pattern index): never rewritten.
pub fn is_infrastructure_file(rel: &str) -> bool {
    crate::setup::templates::INFRASTRUCTURE_FILES.contains(&rel) || rel == "patterns/INDEX.md"
}

/// Inventory the corpus and plan the migration (nothing is written). The planned batch is
/// dry-run through the operation planner, so `blocked` and `changedFiles` are exact.
pub fn plan_migration(opts: &MigrationOptions) -> MigrationPlan {
    let (files, discovery_diags) = parse_corpus(opts.scope);
    let graph = opts
        .graph_db
        .filter(|p| p.exists())
        .and_then(|p| Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY).ok());
    let mut plan = MigrationPlan {
        files_scanned: files.len(),
        diagnostics: discovery_diags,
        ..Default::default()
    };
    // File path -> file-level entity id, for legacy `edges` targets.
    let file_ids: HashMap<String, String> = files
        .iter()
        .filter_map(|f| f.file_entity().map(|pe| (f.path.clone(), pe.entity.id.clone())))
        .collect();
    // Duplicate ids cannot be addressed by an operation without ambiguity.
    let mut id_count: HashMap<&str, usize> = HashMap::new();
    for f in &files {
        for pe in &f.entities {
            *id_count.entry(pe.entity.id.as_str()).or_default() += 1;
        }
    }

    for f in &files {
        plan.entities_scanned += f.entities.len();
        let mut inv = InventoryFile {
            file: f.path.clone(),
            class: "current".into(),
            entities: f.entities.len(),
            findings: 0,
        };
        if is_infrastructure_file(&f.path) {
            inv.class = "infrastructure".into();
            plan.inventory.push(inv);
            continue;
        }
        if opts.scope.is_read_only(&f.path) {
            inv.class = "read_only".into();
            plan.inventory.push(inv);
            continue;
        }
        if f.diagnostics.iter().any(|d| d.severity == "error") {
            inv.class = "unparseable".into();
            plan.abstentions.push(Abstention {
                file: f.path.clone(),
                entity_id: None,
                reason: "the file has parse errors (see `knobyte wiki validate`); fix them first"
                    .into(),
            });
            plan.inventory.push(inv);
            continue;
        }
        let mut abstained = 0;
        for pe in &f.entities {
            let e = &pe.entity;
            if f.entity_text(&pe.loc).trim().is_empty() {
                continue;
            }
            if id_count.get(e.id.as_str()).copied().unwrap_or(0) > 1 {
                abstained += 1;
                plan.abstentions.push(Abstention {
                    file: f.path.clone(),
                    entity_id: Some(e.id.clone()),
                    reason: format!(
                        "entity id {} is claimed more than once; give the copy a new id first",
                        e.id
                    ),
                });
                continue;
            }
            let mut ep = EntityPlan {
                file: f,
                pe,
                items: Vec::new(),
            };
            if !pe.raw.contains_key("id") {
                ep.push(
                    "implicit_id",
                    None,
                    Some(e.id.clone()),
                    json!({ "property": "id", "value": e.id }),
                    "set-property",
                );
            }
            match raw_str(pe, "type") {
                Some(t) if t == "document" => ep.push(
                    "legacy_type",
                    Some(t),
                    Some(DOCUMENT_REPLACEMENT_TYPE.into()),
                    json!({ "property": "type", "value": DOCUMENT_REPLACEMENT_TYPE }),
                    "set-property",
                ),
                Some(_) => {}
                None => {
                    let inferred = infer_type_from_path(&f.path);
                    let to = if inferred == "document" {
                        DOCUMENT_REPLACEMENT_TYPE.to_string()
                    } else {
                        inferred.clone()
                    };
                    ep.push(
                        "implicit_type",
                        Some(inferred),
                        Some(to.clone()),
                        json!({ "property": "type", "value": to }),
                        "set-property",
                    );
                }
            }
            if let Some(raw) = raw_str(pe, "status") {
                match normalize_lifecycle(&raw) {
                    Some((state, true)) => ep.push(
                        "legacy_status",
                        Some(raw),
                        Some(state.into()),
                        json!({ "property": "status", "value": state }),
                        "set-property",
                    ),
                    Some((_, false)) => {}
                    None => {
                        abstained += 1;
                        plan.abstentions.push(Abstention {
                            file: f.path.clone(),
                            entity_id: Some(e.id.clone()),
                            reason: format!(
                                "status {:?} is not a recognizable lifecycle state; set in_flight, promoted, deprecated or archived by hand",
                                raw
                            ),
                        });
                    }
                }
            }
            if let Some((payload, summary)) = plan_legacy_edges(f, pe, &file_ids) {
                ep.push("legacy_edges", Some(summary), Some("relations".into()), payload, "add-relation");
            }
            let legacy_anchor = has_legacy_anchor(f, pe);
            match plan_groundings(pe, graph.as_ref()) {
                Ok(Some((mut payload, summary))) => {
                    if legacy_anchor {
                        payload["normalizeAnchors"] = json!(true);
                    }
                    ep.push(
                        "grounding_shape",
                        Some(summary),
                        None,
                        payload,
                        "set-grounding",
                    );
                }
                Ok(None) if legacy_anchor => ep.push(
                    "legacy_anchor",
                    Some("<!-- grounds: / kb-anchor:".into()),
                    Some("<!-- kb-ground:".into()),
                    json!({ "normalizeAnchors": true }),
                    "set-grounding",
                ),
                Ok(None) => {}
                Err(reason) => {
                    abstained += 1;
                    plan.abstentions.push(Abstention {
                        file: f.path.clone(),
                        entity_id: Some(e.id.clone()),
                        reason,
                    });
                    if legacy_anchor {
                        ep.push(
                            "legacy_anchor",
                            Some("<!-- grounds: / kb-anchor:".into()),
                            Some("<!-- kb-ground:".into()),
                            json!({ "normalizeAnchors": true }),
                            "set-grounding",
                        );
                    }
                }
            }
            // The first operation on an entity carries its content-hash precondition; later
            // ones in the batch see the entity as the earlier ones left it.
            for (i, (item, mut env)) in ep.items.into_iter().enumerate() {
                if i == 0 {
                    env["baseContentHash"] = json!(e.content_hash);
                }
                inv.findings += 1;
                plan.items.push(item);
                plan.operations.push(env);
            }
        }
        if inv.findings > 0 {
            inv.class = "legacy".into();
        } else if abstained > 0 {
            inv.class = "abstained".into();
        }
        plan.inventory.push(inv);
    }
    for a in &plan.abstentions {
        let mut d = diag(
            "MIGRATION_ABSTAINED",
            format!(
                "Migration left {} for review: {}",
                a.entity_id.as_deref().unwrap_or(&a.file),
                a.reason
            ),
            a.file.clone(),
        );
        if let Some(id) = &a.entity_id {
            d = d.for_entity(id);
        }
        plan.diagnostics.push(d);
    }
    if !plan.operations.is_empty() {
        let report = run(opts, &plan.operations, true);
        plan.blocked = !report.ok;
        plan.changed_files = report.changed_files.clone();
        plan.diagnostics.extend(report.diagnostics);
    }
    plan
}

fn run(opts: &MigrationOptions, ops: &[Value], dry_run: bool) -> ApplyReport {
    // One revision per migrated entity, however many findings it had.
    apply_operations_with(
        ops,
        &ApplyOptions {
            scope: opts.scope,
            graph_db: opts.graph_db,
            dry_run,
            default_actor: migration_actor(),
        },
        true,
    )
}

/// Plan, then (unless `dry_run`) apply the migration batch. A blocked plan is never applied.
pub fn migrate(opts: &MigrationOptions, dry_run: bool) -> MigrationResult {
    let plan = plan_migration(opts);
    if dry_run || plan.blocked || plan.operations.is_empty() {
        return MigrationResult {
            applied: false,
            dry_run,
            report: None,
            plan,
        };
    }
    let report = run(opts, &plan.operations, false);
    MigrationResult {
        applied: report.ok,
        dry_run,
        report: Some(report),
        plan,
    }
}

/// Whether the corpus still holds legacy formats (the index state `migration_required`).
pub fn migration_required(scope: &WikiScope) -> bool {
    let (files, _) = parse_corpus(scope);
    let file_ids: HashMap<String, String> = files
        .iter()
        .filter_map(|f| f.file_entity().map(|pe| (f.path.clone(), pe.entity.id.clone())))
        .collect();
    files.iter().any(|f| {
        !scope.is_read_only(&f.path)
            && !is_infrastructure_file(&f.path)
            && !f.diagnostics.iter().any(|d| d.severity == "error")
            && f.entities.iter().any(|pe| {
                !f.entity_text(&pe.loc).trim().is_empty()
                    && (!pe.raw.contains_key("id")
                        || !pe.raw.contains_key("type")
                        || raw_str(pe, "type").as_deref() == Some("document")
                        || raw_str(pe, "status")
                            .and_then(|s| normalize_lifecycle(&s))
                            .is_some_and(|(_, legacy)| legacy)
                        || has_legacy_anchor(f, pe)
                        || has_legacy_grounding_shape(pe)
                        || plan_legacy_edges(f, pe, &file_ids).is_some())
            })
    })
}

/// Human summary lines of a plan.
pub fn render_plan(plan: &MigrationPlan) -> Vec<String> {
    let mut out = Vec::new();
    for i in &plan.items {
        let change = match (&i.from, &i.to) {
            (Some(f), Some(t)) => format!("{} -> {}", f, t),
            (Some(f), None) => f.clone(),
            (None, Some(t)) => t.clone(),
            (None, None) => String::new(),
        };
        out.push(format!(
            "{} {} [{}] {}",
            i.file, i.entity_id, i.kind, change
        ));
    }
    for a in &plan.abstentions {
        out.push(format!(
            "{} {} [abstained] {}",
            a.file,
            a.entity_id.as_deref().unwrap_or("-"),
            a.reason
        ));
    }
    out
}

/// Attach the abstention diagnostics' entity locations (used by setup's summary).
pub fn abstention_messages(plan: &MigrationPlan) -> Vec<String> {
    plan.abstentions
        .iter()
        .map(|a| format!("Wiki migration left {} for review: {}", a.file, a.reason))
        .collect()
}
