//! Planned wiki operations behind opaque, signed handles (the MCP plan/apply pair).
//!
//! `plan` dry-runs a batch of operation envelopes, pins every operation's preconditions
//! (`opId`, `timestamp`, and the target entity's current `baseRevision` / `baseContentHash`)
//! and keeps the pinned batch in a process-lifetime registry under an HMAC-SHA256 handle keyed
//! by a per-process random secret. The handle serializes no paths or Markdown and cannot be
//! forged or replayed across processes. `apply` accepts only a handle this process issued,
//! still unexpired and unused, and applies exactly the planned batch; when an entity moved on
//! since planning, the preconditions fail (`REVISION_CONFLICT` / `CONTENT_HASH_CONFLICT`) and
//! nothing is written. A handle is single-use.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

use crate::wiki::diagnostics::diag;
use crate::wiki::models::WikiDiagnostic;
use crate::wiki::ops::{apply_operations, ApplyOptions, ApplyReport, OpActor};
use crate::wiki::scope::WikiScope;
use crate::wiki::session::canonical_json;

/// How long a planned handle stays valid.
pub const PLAN_TTL: Duration = Duration::from_secs(15 * 60);
/// Plans kept per process (oldest evicted first).
pub const MAX_PLANS: usize = 64;
/// Operations in one planned batch.
pub const MAX_PLAN_OPERATIONS: usize = 100;

type HmacSha256 = Hmac<Sha256>;

struct StoredPlan {
    scaffold_root: PathBuf,
    operations: Vec<Value>,
    created: Instant,
}

struct Registry {
    key: [u8; 32],
    plans: HashMap<String, StoredPlan>,
    order: VecDeque<String>,
}

fn registry() -> &'static Mutex<Registry> {
    static REG: OnceLock<Mutex<Registry>> = OnceLock::new();
    REG.get_or_init(|| {
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Mutex::new(Registry {
            key,
            plans: HashMap::new(),
            order: VecDeque::new(),
        })
    })
}

fn sign(key: &[u8; 32], root: &Path, operations: &[Value]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(b"knobyte-wiki-plan-v1\0");
    mac.update(root.to_string_lossy().as_bytes());
    mac.update(b"\0");
    mac.update(canonical_json(&Value::Array(operations.to_vec())).as_bytes());
    format!("wph1_{}", hex::encode(mac.finalize().into_bytes()))
}

fn verify(key: &[u8; 32], handle: &str, root: &Path, operations: &[Value]) -> bool {
    let Some(sig) = handle
        .strip_prefix("wph1_")
        .and_then(|h| hex::decode(h).ok())
    else {
        return false;
    };
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(b"knobyte-wiki-plan-v1\0");
    mac.update(root.to_string_lossy().as_bytes());
    mac.update(b"\0");
    mac.update(canonical_json(&Value::Array(operations.to_vec())).as_bytes());
    mac.verify_slice(&sig).is_ok()
}

/// The result of planning: the dry-run report and, when it applies cleanly, the handle.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlannedOperation {
    /// Opaque handle for [`apply_planned`]; None when the plan does not apply cleanly.
    pub handle: Option<String>,
    pub expires_in_seconds: u64,
    pub report: ApplyReport,
}

/// Pin `opId`, `timestamp`, `actor` and the current revision / content hash of each
/// operation's target entity.
fn pin(scope: &WikiScope, raw: &[Value], actor: &OpActor) -> Vec<Value> {
    let (files, _) = crate::wiki::validate::parse_corpus(scope);
    let mut current: HashMap<String, (i64, String)> = HashMap::new();
    for f in &files {
        for pe in &f.entities {
            current
                .entry(pe.entity.id.clone())
                .or_insert((pe.entity.revision, pe.entity.content_hash.clone()));
        }
    }
    let now = chrono::Utc::now().to_rfc3339();
    raw.iter()
        .map(|v| {
            let mut v = v.clone();
            if let Some(o) = v.as_object_mut() {
                if !o.contains_key("opId") && !o.contains_key("op_id") {
                    o.insert(
                        "opId".into(),
                        json!(format!("op_{}", uuid::Uuid::new_v4().simple())),
                    );
                }
                o.entry("timestamp").or_insert_with(|| json!(now));
                o.entry("actor").or_insert_with(|| json!(actor));
                let id = o
                    .get("entityId")
                    .or_else(|| o.get("entity_id"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if let Some((rev, hash)) = id.and_then(|id| current.get(&id).cloned()) {
                    if !o.contains_key("baseRevision") && !o.contains_key("base_revision") {
                        o.insert("baseRevision".into(), json!(rev));
                    }
                    if !o.contains_key("baseContentHash") && !o.contains_key("base_content_hash") {
                        o.insert("baseContentHash".into(), json!(hash));
                    }
                }
            }
            v
        })
        .collect()
}

/// Dry-run `raw` and, when it applies cleanly, register it under a signed handle.
pub fn plan(
    scope: &WikiScope,
    graph_db: Option<&Path>,
    raw: &[Value],
    actor: OpActor,
) -> Result<PlannedOperation, Box<WikiDiagnostic>> {
    if raw.is_empty() || raw.len() > MAX_PLAN_OPERATIONS {
        return Err(Box::new(diag(
            "INVALID_REQUEST",
            format!("A plan holds 1-{} operations", MAX_PLAN_OPERATIONS),
            "",
        )));
    }
    let operations = pin(scope, raw, &actor);
    let report = apply_operations(
        &operations,
        &ApplyOptions {
            scope,
            graph_db,
            dry_run: true,
            default_actor: actor,
        },
    );
    if !report.ok {
        return Ok(PlannedOperation {
            handle: None,
            expires_in_seconds: 0,
            report,
        });
    }
    let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    let handle = sign(&reg.key, &scope.scaffold_root, &operations);
    reg.plans.retain(|_, p| p.created.elapsed() < PLAN_TTL);
    reg.order.retain(|h| h != &handle);
    while reg.order.len() >= MAX_PLANS {
        if let Some(old) = reg.order.pop_front() {
            reg.plans.remove(&old);
        }
    }
    reg.plans.insert(
        handle.clone(),
        StoredPlan {
            scaffold_root: scope.scaffold_root.clone(),
            operations,
            created: Instant::now(),
        },
    );
    reg.order.push_back(handle.clone());
    Ok(PlannedOperation {
        handle: Some(handle),
        expires_in_seconds: PLAN_TTL.as_secs(),
        report,
    })
}

fn invalid_handle(msg: &str) -> Box<WikiDiagnostic> {
    Box::new(diag("PLAN_HANDLE_INVALID", msg.to_string(), ""))
}

/// Apply a batch planned by [`plan`] in this process. Single use; preconditions re-checked.
pub fn apply_planned(
    scope: &WikiScope,
    graph_db: Option<&Path>,
    handle: &str,
    actor: OpActor,
) -> Result<ApplyReport, Box<WikiDiagnostic>> {
    let stored = {
        let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        let Some(stored) = reg.plans.remove(handle) else {
            return Err(invalid_handle(
                "The plan handle is unknown, expired or already used. Plan the operation again.",
            ));
        };
        reg.order.retain(|h| h != handle);
        if !verify(&reg.key, handle, &stored.scaffold_root, &stored.operations) {
            return Err(invalid_handle("The plan handle does not verify."));
        }
        stored
    };
    if stored.created.elapsed() >= PLAN_TTL {
        return Err(invalid_handle(
            "The plan handle expired. Plan the operation again.",
        ));
    }
    if stored.scaffold_root != scope.scaffold_root {
        return Err(invalid_handle(
            "The plan handle belongs to another project.",
        ));
    }
    Ok(apply_operations(
        &stored.operations,
        &ApplyOptions {
            scope,
            graph_db,
            dry_run: false,
            default_actor: actor,
        },
    ))
}
