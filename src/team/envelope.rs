use serde::{Deserialize, Serialize};

pub const TEAM_CLI_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProblemDetails {
    pub code: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamCliEnvelope<T> {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub command: String,
    pub mode: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    pub diagnostics: Vec<Diagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<ProblemDetails>,
}

impl<T> TeamCliEnvelope<T> {
    pub fn success(command: &str, mode: &str, data: T) -> Self {
        Self {
            schema_version: TEAM_CLI_SCHEMA_VERSION,
            command: command.to_string(),
            mode: mode.to_string(),
            ok: true,
            data: Some(data),
            diagnostics: Vec::new(),
            problem: None,
        }
    }

    pub fn problem(command: &str, mode: &str, code: &str, detail: &str) -> Self {
        Self {
            schema_version: TEAM_CLI_SCHEMA_VERSION,
            command: command.to_string(),
            mode: mode.to_string(),
            ok: false,
            data: None,
            diagnostics: vec![Diagnostic {
                code: code.to_string(),
                message: detail.to_string(),
                path: None,
            }],
            problem: Some(ProblemDetails {
                code: code.to_string(),
                detail: detail.to_string(),
                instance: None,
            }),
        }
    }
}
