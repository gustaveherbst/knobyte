//! Versioned JSON Schema catalogs for the Inbox and Relay request files.

use serde_json::{json, Value};

use crate::team::envelope::TeamError;
use crate::team::inbox::{KNOWLEDGE_KINDS, SPEC_KINDS};

pub const CONTRACT_VERSION: u32 = 1;
const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

fn string(max: usize) -> Value {
    json!({ "type": "string", "minLength": 1, "maxLength": max })
}

fn string_list(max_items: usize, max_len: usize) -> Value {
    json!({ "type": "array", "maxItems": max_items, "items": string(max_len) })
}

fn revision_expectation() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["path", "revision"],
        "properties": {
            "path": string(512),
            "revision": { "type": ["string", "null"], "pattern": "^sha256:[0-9a-f]{64}$" }
        }
    })
}

fn evidence_ref() -> Value {
    json!({
        "oneOf": [
            { "type": "object", "additionalProperties": false, "required": ["kind", "id"], "properties": { "kind": { "const": "entity" }, "id": string(128), "entityKind": string(64) } },
            { "type": "object", "additionalProperties": false, "required": ["kind", "symbolId"], "properties": { "kind": { "const": "code" }, "symbolId": string(1024) } },
            { "type": "object", "additionalProperties": false, "required": ["kind", "hash"], "properties": { "kind": { "const": "commit" }, "hash": { "type": "string", "pattern": "^[0-9a-fA-F]{4,64}$" } } },
            { "type": "object", "additionalProperties": false, "required": ["kind", "path"], "properties": { "kind": { "const": "file" }, "path": string(1024) } },
            { "type": "object", "additionalProperties": false, "required": ["kind", "uri"], "properties": { "kind": { "const": "external" }, "uri": string(2048), "label": string(256) } },
            { "type": "object", "additionalProperties": false, "required": ["kind", "note"], "properties": { "kind": { "const": "manual" }, "note": string(2048) } }
        ]
    })
}

fn code_ref() -> Value {
    json!({
        "oneOf": [
            { "type": "object", "additionalProperties": false, "required": ["kind", "symbolId"], "properties": { "kind": { "const": "symbol" }, "symbolId": string(1024) } },
            { "type": "object", "additionalProperties": false, "required": ["kind", "path"], "properties": { "kind": { "const": "file" }, "path": string(1024) } }
        ]
    })
}

fn patch() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "minProperties": 1,
        "properties": { "title": string(512), "summary": { "type": "string", "maxLength": 2048 }, "body": string(16384) }
    })
}

fn target() -> Value {
    json!({ "type": "object", "additionalProperties": false, "required": ["id"], "properties": { "id": string(128), "kind": string(64), "title": string(512) } })
}

fn change() -> Value {
    let create = |kind: &str, kinds: &[&str], relation: bool| {
        let mut props = json!({
            "kind": { "const": kind },
            "entityKind": { "enum": kinds },
            "title": string(512),
            "body": string(16384),
            "summary": { "type": "string", "maxLength": 2048 },
            "status": { "enum": ["in_flight", "promoted"] },
            "topics": string_list(64, 128)
        });
        if relation {
            props["relation"] = json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["type", "target"],
                "properties": {
                    "type": { "enum": ["derived_from", "refines", "constrained_by", "verified_by"] },
                    "target": target()
                },
                "description": "requirement: derived_from spec | refines requirement | constrained_by constraint; acceptance_criterion: verified_by requirement/spec | constrained_by constraint; spec/constraint: constrained_by constraint"
            });
        }
        json!({ "type": "object", "additionalProperties": false, "required": ["kind", "entityKind", "title", "body"], "properties": props })
    };
    let update = |kind: &str| {
        json!({ "type": "object", "additionalProperties": false, "required": ["kind", "target", "patch"], "properties": { "kind": { "const": kind }, "target": target(), "patch": patch() } })
    };
    json!({ "oneOf": [
        create("knowledge.create", KNOWLEDGE_KINDS, false),
        update("knowledge.update"),
        create("spec.create", SPEC_KINDS, true),
        update("spec.update"),
    ] })
}

fn inbox_draft_input() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["rationale"],
        "description": "Either a typed `change`, or a legacy Markdown edit with `target` + `content` (+ `mode`).",
        "properties": {
            "title": string(512),
            "change": change(),
            "target": string(512),
            "content": string(65536),
            "mode": { "enum": ["append", "replace"] },
            "rationale": string(8192),
            "evidence": { "type": "array", "maxItems": 64, "items": evidence_ref() },
            "targetRevisions": { "type": "array", "maxItems": 64, "items": revision_expectation() }
        }
    })
}

fn relay_draft_input() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary"],
        "properties": {
            "title": string(512),
            "summary": string(8192),
            "audience": { "enum": ["team", "members"] },
            "recipients": { "type": "array", "maxItems": 32, "uniqueItems": true, "items": string(128) },
            "completed": string_list(64, 4096),
            "inProgress": string_list(64, 4096),
            "decisions": string_list(64, 4096),
            "blockers": string_list(64, 4096),
            "unresolvedQuestions": string_list(64, 4096),
            "changedFiles": string_list(64, 1024),
            "code": { "type": "array", "maxItems": 64, "items": code_ref() },
            "evidence": { "type": "array", "maxItems": 64, "items": evidence_ref() },
            "nextActions": string_list(64, 4096),
            "progress": string_list(64, 4096),
            "workstream": string(128)
        }
    })
}

fn command_schema(action: Value) -> Value {
    json!({
        "$schema": DIALECT,
        "type": "object",
        "additionalProperties": false,
        "required": ["action"],
        "properties": {
            "operationId": { "type": "string", "pattern": "^[A-Za-z0-9][A-Za-z0-9_.:-]{0,127}$" },
            "action": action,
            "expectedRevisions": { "type": "array", "maxItems": 64, "items": revision_expectation() }
        }
    })
}

fn action(kind: &str, required: &[&str], props: Value) -> Value {
    let mut p = props;
    p["kind"] = json!({ "const": kind });
    let mut req = vec!["kind"];
    req.extend_from_slice(required);
    json!({ "type": "object", "additionalProperties": false, "required": req, "properties": p })
}

fn inbox_actions() -> Vec<(&'static str, Value)> {
    let pid = || json!({ "type": "string", "pattern": "^[A-Za-z0-9_.-]{1,128}$" });
    vec![
        ("inbox.draft.save", action("inbox.draft.save", &["draft"], json!({ "draftId": pid(), "draft": inbox_draft_input() }))),
        ("inbox.draft.delete", action("inbox.draft.delete", &["draftId"], json!({ "draftId": pid() }))),
        ("inbox.publish", action("inbox.publish", &["draftId"], json!({ "draftId": pid() }))),
        ("inbox.approve", action("inbox.approve", &["proposalId"], json!({ "proposalId": pid(), "rationale": string(8192), "selfApprove": { "type": "boolean" } }))),
        ("inbox.reject", action("inbox.reject", &["proposalId"], json!({ "proposalId": pid(), "rationale": string(8192) }))),
        ("inbox.withdraw", action("inbox.withdraw", &["proposalId"], json!({ "proposalId": pid(), "rationale": string(8192) }))),
        ("inbox.mark-stale", action("inbox.mark-stale", &["proposalId", "rationale"], json!({ "proposalId": pid(), "rationale": string(8192) }))),
        ("inbox.repair", action("inbox.repair", &["proposalId", "replacement"], json!({ "proposalId": pid(), "replacement": inbox_draft_input() }))),
    ]
}

fn relay_actions() -> Vec<(&'static str, Value)> {
    let id = || json!({ "type": "string", "pattern": "^[A-Za-z0-9_.-]{1,128}$" });
    vec![
        ("relay.draft.save", action("relay.draft.save", &["draft"], json!({ "draftId": id(), "draft": relay_draft_input() }))),
        ("relay.draft.delete", action("relay.draft.delete", &["draftId"], json!({ "draftId": id() }))),
        ("relay.publish", action("relay.publish", &["draftId"], json!({ "draftId": id() }))),
        ("relay.acknowledge", action("relay.acknowledge", &["relayId"], json!({ "relayId": id() }))),
        ("relay.close", action("relay.close", &["relayId"], json!({ "relayId": id() }))),
    ]
}

fn git_aliases() -> Value {
    json!({
        "type": "array",
        "maxItems": 16,
        "items": { "type": "object", "additionalProperties": false, "properties": { "name": string(256), "email": string(256) } }
    })
}

fn member_actions() -> Vec<(&'static str, Value)> {
    let id = || json!({ "type": "string", "pattern": "^[A-Za-z0-9_.-]{1,128}$" });
    let member = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["id", "displayName"],
        "properties": { "id": id(), "displayName": string(256), "email": { "type": "string", "maxLength": 256 }, "role": { "type": "string", "maxLength": 128 }, "gitAliases": git_aliases() }
    });
    let patch = json!({
        "type": "object",
        "additionalProperties": false,
        "minProperties": 1,
        "description": "An empty email or role clears it.",
        "properties": { "displayName": string(256), "email": { "type": "string", "maxLength": 256 }, "role": { "type": "string", "maxLength": 128 }, "gitAliases": git_aliases() }
    });
    vec![
        ("member.add", action("member.add", &["member"], json!({ "member": member }))),
        ("member.update", action("member.update", &["memberId", "patch"], json!({ "memberId": id(), "patch": patch }))),
        ("member.deactivate", action("member.deactivate", &["memberId"], json!({ "memberId": id() }))),
        ("member.reactivate", action("member.reactivate", &["memberId"], json!({ "memberId": id() }))),
        ("member.select", action("member.select", &["memberId"], json!({ "memberId": id() }))),
        ("member.clear", action("member.clear", &[], json!({}))),
    ]
}

fn workstream_actions() -> Vec<(&'static str, Value)> {
    let id = || json!({ "type": "string", "pattern": "^[A-Za-z0-9_.-]{1,128}$" });
    let list = || string_list(64, 512);
    let text = || json!({ "type": "string", "maxLength": 2048 });
    let input = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["title"],
        "properties": {
            "id": id(), "title": string(512), "description": text(), "goal": text(), "summary": text(),
            "state": { "enum": ["planned", "active", "blocked", "done"] },
            "owners": list(), "contributors": list(), "paths": list(), "code": list(), "topics": list(), "components": list(), "related": list(),
            "nextMilestone": text()
        }
    });
    let patch = json!({
        "type": "object",
        "additionalProperties": false,
        "minProperties": 1,
        "properties": {
            "title": string(512), "description": text(), "goal": text(), "summary": text(),
            "state": { "enum": ["planned", "active", "blocked", "done"] },
            "owners": list(), "contributors": list(), "paths": list(), "code": list(), "topics": list(), "components": list(), "related": list(),
            "blockers": list(), "currentState": text(), "nextMilestone": text()
        }
    });
    vec![
        ("workstream.create", action("workstream.create", &["workstream"], json!({ "workstream": input }))),
        ("workstream.update", action("workstream.update", &["workstreamId", "patch"], json!({ "workstreamId": id(), "patch": patch }))),
        ("workstream.archive", action("workstream.archive", &["workstreamId"], json!({ "workstreamId": id() }))),
        ("workstream.step.update", action("workstream.step.update", &["workstreamId", "stepId", "status"], json!({
            "workstreamId": id(),
            "stepId": id(),
            "status": { "enum": crate::team::workstreams::STEP_STATUSES },
            "evidence": { "type": "string", "maxLength": 4096 },
            "filesTouched": list()
        }))),
    ]
}

fn activity_actions() -> Vec<(&'static str, Value)> {
    let subject = json!({
        "oneOf": [
            { "type": "object", "additionalProperties": false, "required": ["kind", "id", "entityKind"], "properties": { "kind": { "const": "entity" }, "id": string(128), "entityKind": string(64), "title": string(512) } },
            { "type": "object", "additionalProperties": false, "required": ["kind", "symbolId"], "properties": { "kind": { "const": "code" }, "symbolId": string(1024) } },
            { "type": "object", "additionalProperties": false, "required": ["kind", "path"], "properties": { "kind": { "const": "file" }, "path": string(1024) } },
            { "type": "object", "additionalProperties": false, "required": ["kind", "hash"], "properties": { "kind": { "const": "commit" }, "hash": { "type": "string", "pattern": "^[0-9a-fA-F]{4,64}$" } } }
        ]
    });
    let input = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["action", "summary"],
        "properties": {
            "action": { "type": "string", "pattern": "^[A-Za-z0-9._-]{1,64}$" },
            "summary": string(2048),
            "entityKind": string(64), "entityId": string(128), "entityTitle": string(512),
            "subjects": { "type": "array", "items": subject },
            "workstream": string(128),
            "label": string(200)
        }
    });
    vec![("activity.record", action("activity.record", &["activity"], json!({ "activity": input })))]
}

fn playbook_actions() -> Vec<(&'static str, Value)> {
    let id = || json!({ "type": "string", "pattern": "^[A-Za-z0-9_.-]{1,128}$" });
    let text = |max: usize| json!({ "type": "string", "maxLength": max });
    let step = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["title"],
        "properties": {
            "id": id(), "title": string(512), "description": text(8192),
            "requiredChecks": string_list(64, 1024), "expectedEvidence": string_list(64, 1024)
        }
    });
    let steps = json!({ "type": "array", "maxItems": crate::team::playbooks::MAX_STEPS, "items": step });
    let input = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["title"],
        "properties": {
            "id": id(), "title": string(512), "summary": text(4096), "trigger": text(2048),
            "state": { "enum": ["draft", "active"] },
            "owners": string_list(64, 128), "topics": string_list(64, 128), "prerequisites": string_list(64, 1024), "related": string_list(64, 512),
            "steps": steps.clone()
        }
    });
    let patch = json!({
        "type": "object",
        "additionalProperties": false,
        "minProperties": 1,
        "properties": {
            "title": string(512), "summary": text(4096), "trigger": text(2048),
            "state": { "enum": ["draft", "active"] },
            "owners": string_list(64, 128), "topics": string_list(64, 128), "prerequisites": string_list(64, 1024), "related": string_list(64, 512),
            "steps": steps
        }
    });
    vec![
        ("playbook.create", action("playbook.create", &["playbook"], json!({ "playbook": input }))),
        ("playbook.update", action("playbook.update", &["playbookId", "patch"], json!({ "playbookId": id(), "patch": patch }))),
        ("playbook.archive", action("playbook.archive", &["playbookId"], json!({ "playbookId": id() }))),
        ("playbook.run.start", action("playbook.run.start", &["playbookId"], json!({ "playbookId": id(), "workstream": id(), "title": text(512) }))),
        ("playbook.run.complete-step", action("playbook.run.complete-step", &["runId", "stepId"], json!({
            "runId": id(), "stepId": id(),
            "evidence": { "type": "array", "maxItems": 64, "items": evidence_ref() },
            "note": text(4096)
        }))),
        ("playbook.run.abandon", action("playbook.run.abandon", &["runId", "reason"], json!({ "runId": id(), "reason": string(4096) }))),
    ]
}

fn catchup_actions() -> Vec<(&'static str, Value)> {
    vec![
        ("catchup.mark", action("catchup.mark", &[], json!({ "at": { "type": "string", "format": "date-time" } }))),
        ("catchup.reset", action("catchup.reset", &[], json!({ "to": string(64), "clear": { "type": "boolean" } }))),
    ]
}

fn catalog(family: &str, actions: Vec<(&'static str, Value)>, only: Option<&str>) -> Result<Value, TeamError> {
    let mut commands = serde_json::Map::new();
    for (name, a) in actions {
        if only.map(|o| o == name).unwrap_or(true) {
            commands.insert(name.to_string(), json!({
                "request": command_schema(a),
                "apply": "Pass the complete envelope printed by `--preview --json` to `--apply <file>`.",
                "mutatesCanonical": !name.contains(".draft.") && !name.starts_with("catchup.") && !matches!(name, "member.select" | "member.clear"),
            }));
        }
    }
    if commands.is_empty() {
        return Err(TeamError::usage(format!("Unknown {} action '{}'", family, only.unwrap_or(""))));
    }
    Ok(json!({
        "contractVersion": CONTRACT_VERSION,
        "family": family,
        "dialect": DIALECT,
        "humanOnly": if family == "inbox" { json!(["inbox.approve", "inbox.reject"]) } else { json!([]) },
        "previewMaxAgeSeconds": crate::team::workflow::MAX_PREVIEW_AGE_SECS,
        "commands": commands,
    }))
}

/// Inbox JSON Schema catalog (optionally one action).
pub fn inbox_contract(only: Option<&str>) -> Result<Value, TeamError> {
    catalog("inbox", inbox_actions(), only)
}

/// Relay JSON Schema catalog (optionally one action).
pub fn relay_contract(only: Option<&str>) -> Result<Value, TeamError> {
    catalog("relay", relay_actions(), only)
}

/// Member JSON Schema catalog (optionally one action).
pub fn member_contract(only: Option<&str>) -> Result<Value, TeamError> {
    catalog("member", member_actions(), only)
}

/// Workstream JSON Schema catalog (optionally one action).
pub fn workstream_contract(only: Option<&str>) -> Result<Value, TeamError> {
    catalog("workstream", workstream_actions(), only)
}

/// Activity JSON Schema catalog (optionally one action).
pub fn activity_contract(only: Option<&str>) -> Result<Value, TeamError> {
    catalog("activity", activity_actions(), only)
}

/// Playbook JSON Schema catalog (optionally one action).
pub fn playbook_contract(only: Option<&str>) -> Result<Value, TeamError> {
    catalog("playbook", playbook_actions(), only)
}

/// Catch-up cursor JSON Schema catalog (optionally one action).
pub fn catchup_contract(only: Option<&str>) -> Result<Value, TeamError> {
    catalog("catchup", catchup_actions(), only)
}
