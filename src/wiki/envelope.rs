//! The wiki's answer shape and exit-code table, defined once for the CLI and the MCP tools.
//!
//! Every `--json` answer of `knobyte wiki ...` (and every wiki MCP tool) is one envelope:
//! `{schemaVersion, ok, data, diagnostics}`. `ok` is not a free field: it is false exactly when
//! a diagnostic has error severity, and the exit status is derived from the envelope, so a
//! failure can never be paired with exit 0. Keys are camelCase throughout; values that are
//! user data (entity `metadata`, operation `payload`s, type-keyed maps) keep their own keys.

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::wiki::diagnostics::{definition, sort_diagnostics};
use crate::wiki::models::WikiDiagnostic;

/// Bumped only when the envelope's shape changes.
pub const WIKI_SCHEMA_VERSION: u32 = 1;

/// Exit codes of `knobyte wiki ...`, one table derived from diagnostic codes.
pub mod exit_code {
    /// The command answered (warnings may be present).
    pub const OK: i32 = 0;
    /// The command ran and found error-severity problems (`wiki validate` in CI).
    pub const DIAGNOSTICS: i32 = 1;
    /// Bad flag, unparseable operation file, missing argument.
    pub const USAGE: i32 = 2;
    /// No index, or one this build cannot read.
    pub const INDEX: i32 = 3;
    /// A precondition did not hold (revision / content-hash conflict, corpus moved).
    pub const PRECONDITION: i32 = 4;
    /// The write was refused (outside the scaffold or a read-only path).
    pub const REFUSED: i32 = 5;
}

/// The exit code one error-severity diagnostic code maps to.
pub fn exit_code_of(code: &str) -> i32 {
    match code {
        "WRITE_SCOPE_VIOLATION" | "PATH_OUTSIDE_SCAFFOLD" => exit_code::REFUSED,
        "REVISION_CONFLICT"
        | "CONTENT_HASH_CONFLICT"
        | "PLAN_HANDLE_INVALID"
        | "OPERATION_INTERRUPTED"
        | "WIKI_INDEX_BUSY" => exit_code::PRECONDITION,
        "WIKI_INDEX_MISSING" | "WIKI_INDEX_REBUILD_REQUIRED" | "WIKI_INDEX_CORRUPT" => {
            exit_code::INDEX
        }
        "INVALID_OPERATION_ENVELOPE" | "INVALID_REQUEST" => exit_code::USAGE,
        _ => exit_code::DIAGNOSTICS,
    }
}

/// Exit status for a set of error-severity diagnostic codes: the most actionable wins.
pub fn exit_code_for(codes: &[&str]) -> i32 {
    let precedence = [
        exit_code::REFUSED,
        exit_code::PRECONDITION,
        exit_code::INDEX,
        exit_code::USAGE,
        exit_code::DIAGNOSTICS,
    ];
    let present: Vec<i32> = codes.iter().map(|c| exit_code_of(c)).collect();
    if present.is_empty() {
        return exit_code::OK;
    }
    precedence
        .into_iter()
        .find(|p| present.contains(p))
        .unwrap_or(exit_code::DIAGNOSTICS)
}

/// One answer, ready to serialize.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WikiEnvelope {
    pub schema_version: u32,
    pub ok: bool,
    pub data: Value,
    pub diagnostics: Vec<Value>,
}

impl WikiEnvelope {
    /// The exit status this envelope implies: 0 when `ok`, else the most actionable error.
    pub fn exit_code(&self) -> i32 {
        if self.ok {
            return exit_code::OK;
        }
        let codes: Vec<&str> = self
            .diagnostics
            .iter()
            .filter(|d| d["severity"] == "error")
            .filter_map(|d| d["code"].as_str())
            .collect();
        match exit_code_for(&codes) {
            exit_code::OK => exit_code::DIAGNOSTICS,
            c => c,
        }
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// A diagnostic as it crosses the wire: camelCase, remediation from the registry.
pub fn project_diagnostic(d: &WikiDiagnostic) -> Value {
    let mut o = Map::new();
    o.insert("code".into(), json!(d.code));
    o.insert("severity".into(), json!(d.severity));
    o.insert("message".into(), json!(d.message));
    if !d.file.is_empty() {
        o.insert("file".into(), json!(d.file));
    }
    if let Some(l) = d.line {
        o.insert("line".into(), json!(l));
    }
    if let Some(e) = d.entity_id.as_deref().filter(|e| !e.is_empty()) {
        o.insert("entityId".into(), json!(e));
    }
    if let Some(p) = &d.path {
        o.insert("path".into(), json!(p));
    }
    if let Some(l) = &d.location {
        o.insert("location".into(), json!(l));
    }
    let remediation = d
        .remediation
        .clone()
        .or_else(|| definition(&d.code).map(|(_, r)| r.to_string()))
        .filter(|r| !r.is_empty());
    if let Some(r) = remediation {
        o.insert("remediation".into(), json!(r));
    }
    Value::Object(o)
}

/// Build the envelope for `data` (camelized) and `diagnostics` (sorted, projected).
pub fn envelope_for<T: Serialize>(data: &T, diagnostics: &[WikiDiagnostic]) -> WikiEnvelope {
    let mut sorted = diagnostics.to_vec();
    sort_diagnostics(&mut sorted);
    WikiEnvelope {
        schema_version: WIKI_SCHEMA_VERSION,
        ok: !sorted.iter().any(|d| d.severity == "error"),
        data: camelize(serde_json::to_value(data).unwrap_or(Value::Null)),
        diagnostics: sorted.iter().map(project_diagnostic).collect(),
    }
}

/// A failed answer carrying only diagnostics (`data` is null).
pub fn failure(diagnostics: &[WikiDiagnostic]) -> WikiEnvelope {
    envelope_for(&Value::Null, diagnostics)
}

/// `snake_case` -> `camelCase` (keys that are already camelCase pass through).
pub fn camel_key(k: &str) -> String {
    if !k.contains('_') || k.starts_with('_') {
        return k.to_string();
    }
    let mut out = String::with_capacity(k.len());
    let mut upper = false;
    for c in k.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Keys whose value is user data and is passed through verbatim.
const VERBATIM_KEYS: &[&str] = &["metadata", "payload", "raw", "frontmatter", "prompt"];
/// Keys whose value is a map keyed by data (entity types, file paths): its keys are kept,
/// its values camelized.
const DATA_KEYED_MAPS: &[&str] = &["nodes", "byType", "byFile", "tables"];

/// Recursively camelize object keys, sparing user data.
pub fn camelize(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, val) in m {
                let key = camel_key(&k);
                let val = if VERBATIM_KEYS.contains(&key.as_str()) {
                    val
                } else if DATA_KEYED_MAPS.contains(&key.as_str()) && val.is_object() {
                    match val {
                        Value::Object(inner) => Value::Object(
                            inner.into_iter().map(|(k, v)| (k, camelize(v))).collect(),
                        ),
                        other => other,
                    }
                } else {
                    camelize(val)
                };
                out.insert(key, val);
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.into_iter().map(camelize).collect()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wiki::diagnostics::diag;

    #[test]
    fn ok_tracks_error_severity_and_exit_codes_rank() {
        let e = envelope_for(&json!({"entity_type": "x", "metadata": {"a_b": 1}}), &[]);
        assert!(e.ok);
        assert_eq!(e.exit_code(), 0);
        assert_eq!(e.data["entityType"], "x");
        assert_eq!(e.data["metadata"]["a_b"], 1);
        let e = failure(&[
            diag("ENTITY_NOT_FOUND", "x", ""),
            diag("WIKI_INDEX_MISSING", "y", ""),
        ]);
        assert!(!e.ok);
        assert_eq!(e.exit_code(), exit_code::INDEX);
        assert!(e.diagnostics[0]["remediation"].is_string());
        let warn = envelope_for(&Value::Null, &[diag("GROUNDING_STALE", "w", "a.md")]);
        assert!(warn.ok);
        assert_eq!(warn.exit_code(), 0);
        assert_eq!(
            exit_code_for(&["WRITE_SCOPE_VIOLATION", "REVISION_CONFLICT"]),
            5
        );
    }
}
