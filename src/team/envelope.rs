//! Shared machine envelope, problem details, exit codes and paging for team commands.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::team::store::short_hash;

pub const TEAM_CLI_SCHEMA_VERSION: u32 = 1;

/// Stable process statuses for every team command.
pub mod exit {
    pub const OK: i32 = 0;
    pub const VALIDATION: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const UNAVAILABLE: i32 = 3;
    pub const CONFLICT: i32 = 4;
    pub const REFUSED: i32 = 5;
}

pub const DEFAULT_PAGE_SIZE: usize = 50;
pub const MAX_PAGE_SIZE: usize = 100;
const MAX_CURSOR_BYTES: usize = 4 * 1024;

/// Machine error codes (mirrors the problem codes used by the Hub and MCP surfaces).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    ValidationFailed,
    InvalidRequest,
    NotFound,
    RevisionConflict,
    OperationInterrupted,
    Unauthorized,
    PathOutsideProject,
    InternalError,
}

impl ErrorCode {
    /// Wire spelling used in JSON envelopes (matches the serde representation).
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::ValidationFailed => "VALIDATION_FAILED",
            ErrorCode::InvalidRequest => "INVALID_REQUEST",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::RevisionConflict => "REVISION_CONFLICT",
            ErrorCode::OperationInterrupted => "OPERATION_INTERRUPTED",
            ErrorCode::Unauthorized => "UNAUTHORIZED",
            ErrorCode::PathOutsideProject => "PATH_OUTSIDE_PROJECT",
            ErrorCode::InternalError => "INTERNAL_ERROR",
        }
    }

    pub fn exit_code(self) -> i32 {
        match self {
            ErrorCode::ValidationFailed | ErrorCode::InternalError => exit::VALIDATION,
            ErrorCode::InvalidRequest => exit::USAGE,
            ErrorCode::NotFound => exit::UNAVAILABLE,
            ErrorCode::RevisionConflict | ErrorCode::OperationInterrupted => exit::CONFLICT,
            ErrorCode::Unauthorized | ErrorCode::PathOutsideProject => exit::REFUSED,
        }
    }

    pub fn http_status(self) -> u16 {
        match self {
            ErrorCode::ValidationFailed => 422,
            ErrorCode::InvalidRequest => 400,
            ErrorCode::NotFound => 404,
            ErrorCode::RevisionConflict | ErrorCode::OperationInterrupted => 409,
            ErrorCode::Unauthorized | ErrorCode::PathOutsideProject => 403,
            ErrorCode::InternalError => 500,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Diagnostic {
    pub code: String,
    pub severity: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl Diagnostic {
    pub fn warning(code: &str, message: impl Into<String>) -> Self {
        Diagnostic { code: code.to_string(), severity: "warning".to_string(), message: message.into(), path: None }
    }
    pub fn info(code: &str, message: impl Into<String>) -> Self {
        Diagnostic { code: code.to_string(), severity: "info".to_string(), message: message.into(), path: None }
    }
    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }
}

/// RFC 9457-style problem body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Problem {
    pub title: String,
    pub status: u16,
    pub code: ErrorCode,
    pub detail: String,
}

/// Error type of every team operation. `Display` yields the human detail so
/// legacy `Result<_, String>` callers keep their messages.
#[derive(Debug, Clone, PartialEq)]
pub struct TeamError {
    pub code: ErrorCode,
    pub title: String,
    pub detail: String,
}

impl TeamError {
    pub fn new(code: ErrorCode, title: impl Into<String>, detail: impl Into<String>) -> Self {
        TeamError { code, title: title.into(), detail: detail.into() }
    }
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::ValidationFailed, "Validation failed", detail)
    }
    pub fn usage(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, "Invalid team command request", detail)
    }
    pub fn not_found(label: &str, id: &str) -> Self {
        Self::new(ErrorCode::NotFound, format!("{} not found", label), format!("{} '{}' not found", label, id))
    }
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::RevisionConflict, "Revision conflict", detail)
    }
    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::Unauthorized, "Not allowed", detail)
    }
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(ErrorCode::InternalError, "Team command failed", detail)
    }
    pub fn problem(&self) -> Problem {
        Problem { title: self.title.clone(), status: self.code.http_status(), code: self.code, detail: self.detail.clone() }
    }
    pub fn exit_code(&self) -> i32 {
        self.code.exit_code()
    }
}

impl std::fmt::Display for TeamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

impl std::error::Error for TeamError {}

impl From<TeamError> for String {
    fn from(e: TeamError) -> String {
        e.detail
    }
}

/// Classify a legacy string error.
impl From<String> for TeamError {
    fn from(msg: String) -> Self {
        let lower = msg.to_lowercase();
        let code = if lower.contains("not found") {
            ErrorCode::NotFound
        } else if lower.contains("changed since") || lower.contains("conflict") || lower.contains("already exists") {
            ErrorCode::RevisionConflict
        } else if lower.contains("not allowed") || lower.contains("only the") || lower.contains("not a named recipient") || lower.contains("not the sender") {
            ErrorCode::Unauthorized
        } else if lower.contains("escapes") || lower.contains("outside") {
            ErrorCode::PathOutsideProject
        } else {
            ErrorCode::ValidationFailed
        };
        TeamError::new(code, "Team command failed", msg)
    }
}

impl From<&str> for TeamError {
    fn from(msg: &str) -> Self {
        TeamError::from(msg.to_string())
    }
}

/// One bounded machine envelope for team reads, previews and applies.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamEnvelope {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    pub command: String,
    pub mode: String,
    pub ok: bool,
    pub data: Value,
    pub diagnostics: Vec<Diagnostic>,
    pub problem: Option<Problem>,
}

impl TeamEnvelope {
    pub fn ok(command: &str, mode: &str, data: Value, diagnostics: Vec<Diagnostic>) -> Self {
        let ok = !diagnostics.iter().any(|d| d.severity == "error");
        TeamEnvelope {
            schema_version: TEAM_CLI_SCHEMA_VERSION,
            command: command.to_string(),
            mode: mode.to_string(),
            ok,
            data,
            diagnostics,
            problem: None,
        }
    }

    pub fn error(command: &str, mode: &str, err: &TeamError) -> Self {
        TeamEnvelope {
            schema_version: TEAM_CLI_SCHEMA_VERSION,
            command: command.to_string(),
            mode: mode.to_string(),
            ok: false,
            data: Value::Null,
            diagnostics: Vec::new(),
            problem: Some(err.problem()),
        }
    }

    pub fn exit_code(&self) -> i32 {
        if self.ok {
            exit::OK
        } else if let Some(p) = &self.problem {
            p.code.exit_code()
        } else {
            exit::VALIDATION
        }
    }

    pub fn render(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }
}

// ---------------------------------------------------------------------------
// Paging
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    #[serde(rename = "nextCursor")]
    pub next_cursor: Option<String>,
    pub truncated: bool,
    pub total: usize,
    #[serde(rename = "deterministicRevision")]
    pub deterministic_revision: String,
}

/// Validate a `--limit` value (1..=100, default 50).
pub fn page_limit(limit: Option<usize>) -> Result<usize, TeamError> {
    match limit {
        None => Ok(DEFAULT_PAGE_SIZE),
        Some(n) if (1..=MAX_PAGE_SIZE).contains(&n) => Ok(n),
        Some(_) => Err(TeamError::usage(format!("--limit must be an integer from 1 to {}", MAX_PAGE_SIZE))),
    }
}

/// Page an already filtered and ordered list. The cursor binds the ordered
/// corpus (by `key`) and the filter; it is refused once either changes.
pub fn paginate<T>(
    all: Vec<T>,
    key: impl Fn(&T) -> String,
    filter_key: &str,
    cursor: Option<&str>,
    limit: Option<usize>,
) -> Result<Page<T>, TeamError> {
    let limit = page_limit(limit)?;
    let mut basis = String::from(filter_key);
    for item in &all {
        basis.push('\n');
        basis.push_str(&key(item));
    }
    let revision = short_hash(&basis);
    let offset = match cursor {
        None => 0,
        Some(c) => decode_cursor(c, &revision)?,
    };
    let total = all.len();
    if offset > total {
        return Err(TeamError::conflict("The cursor is past the end of the list; restart without --cursor"));
    }
    let items: Vec<T> = all.into_iter().skip(offset).take(limit).collect();
    let next = offset + items.len();
    let next_cursor = if next < total { Some(format!("c1_{}_{}", next, revision)) } else { None };
    Ok(Page { truncated: next_cursor.is_some(), next_cursor, items, total, deterministic_revision: revision })
}

fn decode_cursor(cursor: &str, revision: &str) -> Result<usize, TeamError> {
    if cursor.is_empty()
        || cursor.len() > MAX_CURSOR_BYTES
        || !cursor.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(TeamError::usage("--cursor must be a cursor returned by a previous page"));
    }
    let parts: Vec<&str> = cursor.split('_').collect();
    if parts.len() != 3 || parts[0] != "c1" {
        return Err(TeamError::usage("--cursor must be a cursor returned by a previous page"));
    }
    let offset: usize = parts[1]
        .parse()
        .map_err(|_| TeamError::usage("--cursor must be a cursor returned by a previous page"))?;
    if parts[2] != revision {
        return Err(TeamError::conflict(
            "The list or its filters changed since this cursor was issued; restart without --cursor",
        ));
    }
    Ok(offset)
}
