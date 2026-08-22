//! `knobyte logging [mode]`: the checkout-local, advisory agent logging cadence.
//!
//! Stored in `.knobyte/local/agent-preferences.json` (`{"schemaVersion":1,"mode":"..."}`),
//! never committed. Modes: `significant` (default), `checkpoints`, `manual`. Writes can be
//! guarded with the exact current revision (`sha256:<hex>` of the file, or `none`).

use std::fs;

use serde::Serialize;

use crate::config::KnobyteConfig;
use crate::managed_block::sha256_hex;

pub const LOGGING_MODES: &[&str] = &["significant", "checkpoints", "manual"];
const MAX_BYTES: u64 = 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct LoggingPolicy {
    pub mode: String,
    /// `sha256:<hex>` of the stored preference, `None` when using the default.
    pub revision: Option<String>,
    /// `default` or `local`.
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoggingError {
    /// No scaffold in this checkout.
    NotFound(String),
    /// Stored preference is malformed.
    Invalid(String),
    /// Usage error (unknown mode, bad revision).
    Usage(String),
    /// `--expected-revision` did not match.
    Conflict(String),
    Io(String),
}

impl LoggingError {
    pub fn code(&self) -> &'static str {
        match self {
            LoggingError::NotFound(_) => "NOT_FOUND",
            LoggingError::Invalid(_) => "VALIDATION_FAILED",
            LoggingError::Usage(_) => "INVALID_REQUEST",
            LoggingError::Conflict(_) => "REVISION_CONFLICT",
            LoggingError::Io(_) => "INTERNAL",
        }
    }
    pub fn detail(&self) -> &str {
        match self {
            LoggingError::NotFound(d)
            | LoggingError::Invalid(d)
            | LoggingError::Usage(d)
            | LoggingError::Conflict(d)
            | LoggingError::Io(d) => d,
        }
    }
    pub fn exit_code(&self) -> i32 {
        match self {
            LoggingError::Conflict(_) => 4,
            LoggingError::Usage(_) => 2,
            LoggingError::NotFound(_) => 3,
            _ => 1,
        }
    }
}

fn path(config: &KnobyteConfig) -> std::path::PathBuf {
    config.local_dir().join("agent-preferences.json")
}

pub fn read_policy(config: &KnobyteConfig) -> Result<LoggingPolicy, LoggingError> {
    if !config.scaffold_root.is_dir() {
        return Err(LoggingError::NotFound("Run knobyte logging from an existing Knobyte project.".into()));
    }
    let p = path(config);
    let meta = match fs::symlink_metadata(&p) {
        Ok(m) => m,
        Err(_) => return Ok(LoggingPolicy { mode: "significant".into(), revision: None, source: "default".into() }),
    };
    let invalid = || {
        LoggingError::Invalid(
            "The checkout logging preference is malformed or unsupported. Inspect .knobyte/local/agent-preferences.json before changing it.".into(),
        )
    };
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Err(invalid());
    }
    let bytes = fs::read(&p).map_err(|e| LoggingError::Io(e.to_string()))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    let obj = value.as_object().ok_or_else(invalid)?;
    if obj.len() != 2 || obj.get("schemaVersion").and_then(|v| v.as_u64()) != Some(1) {
        return Err(invalid());
    }
    let mode = obj.get("mode").and_then(|m| m.as_str()).filter(|m| LOGGING_MODES.contains(m)).ok_or_else(invalid)?;
    Ok(LoggingPolicy { mode: mode.to_string(), revision: Some(format!("sha256:{}", sha256_hex(&bytes))), source: "local".into() })
}

/// Set the mode. `expected_revision`: `None` = no guard; `Some("none")` = must be unset;
/// `Some("sha256:...")` = must match exactly.
pub fn set_policy(config: &KnobyteConfig, mode: &str, expected_revision: Option<&str>) -> Result<LoggingPolicy, LoggingError> {
    if !LOGGING_MODES.contains(&mode) {
        return Err(LoggingError::Usage(format!(
            "Unknown logging mode '{}'. Use {}.",
            mode,
            LOGGING_MODES.join(", ")
        )));
    }
    let current = read_policy(config)?;
    if let Some(expected) = expected_revision {
        let expected = if expected == "none" { None } else { Some(expected) };
        if let Some(e) = expected {
            if !e.starts_with("sha256:") || e.len() != 71 {
                return Err(LoggingError::Usage("--expected-revision must be `none` or `sha256:<64 hex>`.".into()));
            }
        }
        if current.revision.as_deref() != expected {
            return Err(LoggingError::Conflict(
                "The logging preference changed. Read it again and retry with its exact revision.".into(),
            ));
        }
    }
    if current.source == "local" && current.mode == mode {
        return Ok(current);
    }
    let doc = format!("{}\n", serde_json::json!({ "schemaVersion": 1, "mode": mode }));
    fs::create_dir_all(config.local_dir()).map_err(|e| LoggingError::Io(e.to_string()))?;
    let p = path(config);
    let tmp = p.with_extension("json.tmp");
    fs::write(&tmp, &doc).map_err(|e| LoggingError::Io(e.to_string()))?;
    fs::rename(&tmp, &p).map_err(|e| LoggingError::Io(e.to_string()))?;
    Ok(LoggingPolicy { mode: mode.into(), revision: Some(format!("sha256:{}", sha256_hex(doc.as_bytes()))), source: "local".into() })
}

/// Human rendering.
pub fn render_policy(policy: &LoggingPolicy, changed: bool) -> String {
    let explanation = match policy.mode.as_str() {
        "significant" => "Record meaningful decisions, risks, and durable discoveries; skip routine progress.",
        "checkpoints" => "Batch useful optional notes at task or session boundaries.",
        _ => "Write optional notes only when the user requests them.",
    };
    format!(
        "{} {} ({}, this checkout only). {} Explicit user log requests and required workflow activity are always honored.",
        if changed { "Saved" } else { "Agent logging:" },
        policy.mode,
        policy.source,
        explanation
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_set_and_conflict() {
        let d = tempfile::tempdir().unwrap();
        let c = KnobyteConfig::new(d.path().to_path_buf(), d.path().join(".knobyte"));
        assert!(matches!(read_policy(&c), Err(LoggingError::NotFound(_))));
        fs::create_dir_all(&c.scaffold_root).unwrap();
        assert_eq!(read_policy(&c).unwrap().source, "default");
        let p = set_policy(&c, "manual", Some("none")).unwrap();
        assert_eq!(p.mode, "manual");
        assert_eq!(set_policy(&c, "checkpoints", Some("none")).unwrap_err().exit_code(), 4);
        let p2 = set_policy(&c, "checkpoints", p.revision.as_deref()).unwrap();
        assert_eq!(read_policy(&c).unwrap(), p2);
        assert!(set_policy(&c, "loud", None).is_err());
    }
}
