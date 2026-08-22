//! RFC 9457 problem details for every Hub error response
//! (`application/problem+json`).

use axum::{
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};

use crate::team::{ErrorCode, TeamError};

pub const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

/// One problem response.
#[derive(Debug, Clone)]
pub struct Problem {
    pub status: StatusCode,
    pub code: &'static str,
    pub title: String,
    pub detail: String,
    pub active_job_id: Option<Box<str>>,
    pub extra: Option<Box<Value>>,
}

impl Problem {
    pub fn new(status: StatusCode, code: &'static str, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Problem { status, code, title: title.into(), detail: detail.into(), active_job_id: None, extra: None }
    }
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "INVALID_REQUEST", "Invalid request", detail)
    }
    pub fn validation(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "VALIDATION_FAILED", "Validation failed", detail)
    }
    pub fn not_found(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "NOT_FOUND", "Resource not found", detail)
    }
    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "REVISION_CONFLICT", "Revision conflict", detail)
    }
    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Authentication required", detail)
    }
    pub fn forbidden(code: &'static str, title: &str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, title, detail)
    }
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "CAPABILITY_UNAVAILABLE", "Capability unavailable", detail)
    }
    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR", "Internal error", detail)
    }
    pub fn job_running(active: &str) -> Self {
        let mut p = Self::new(
            StatusCode::CONFLICT,
            "JOB_ALREADY_RUNNING",
            "Job already running",
            "An index job is already active for this project; wait for it or cancel it.",
        );
        p.active_job_id = Some(active.into());
        p
    }
    pub fn with_extra(mut self, extra: Value) -> Self {
        self.extra = Some(Box::new(extra));
        self
    }

    /// Classify a status code into a problem code (legacy string errors).
    pub fn from_status(status: StatusCode, detail: impl Into<String>) -> Self {
        let (code, title) = match status.as_u16() {
            400 => ("INVALID_REQUEST", "Invalid request"),
            401 => ("UNAUTHORIZED", "Authentication required"),
            403 => ("ORIGIN_REJECTED", "Request rejected"),
            404 => ("NOT_FOUND", "Resource not found"),
            405 => ("INVALID_REQUEST", "Method not allowed"),
            409 => ("REVISION_CONFLICT", "Conflict"),
            413 => ("INVALID_REQUEST", "Request too large"),
            422 => ("VALIDATION_FAILED", "Validation failed"),
            503 => ("CAPABILITY_UNAVAILABLE", "Capability unavailable"),
            _ => ("INTERNAL_ERROR", "Internal error"),
        };
        Self::new(status, code, title, detail)
    }

    pub fn body(&self) -> Value {
        let mut v = json!({
            "type": "about:blank",
            "title": self.title,
            "status": self.status.as_u16(),
            "code": self.code,
            "detail": self.detail,
        });
        if let Some(id) = &self.active_job_id {
            v["activeJobId"] = json!(id);
        }
        if let (Some(extra), Some(obj)) = (&self.extra, v.as_object_mut()) {
            if let Some(e) = extra.as_object() {
                for (k, val) in e {
                    obj.entry(k.clone()).or_insert(val.clone());
                }
            }
        }
        v
    }
}

fn team_code(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::ValidationFailed => "VALIDATION_FAILED",
        ErrorCode::InvalidRequest => "INVALID_REQUEST",
        ErrorCode::NotFound => "NOT_FOUND",
        ErrorCode::RevisionConflict => "REVISION_CONFLICT",
        ErrorCode::OperationInterrupted => "OPERATION_INTERRUPTED",
        ErrorCode::Unauthorized => "UNAUTHORIZED_ACTOR",
        ErrorCode::PathOutsideProject => "PATH_OUTSIDE_PROJECT",
        ErrorCode::InternalError => "INTERNAL_ERROR",
    }
}

/// Problem code for an author approving their own proposal without opting in. The Hub maps it
/// to its "approve my own proposal" checkbox instead of the CLI's `--self-approve` flag.
pub const SELF_APPROVAL_REQUIRED: &str = "SELF_APPROVAL_REQUIRED";

impl From<TeamError> for Problem {
    fn from(e: TeamError) -> Self {
        let status = StatusCode::from_u16(e.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let self_approval = e.detail.starts_with(crate::team::inbox::SELF_APPROVAL_REQUIRED)
            || e.detail.starts_with("You authored this proposal");
        if e.code == ErrorCode::Unauthorized && self_approval {
            return Problem::new(
                status,
                SELF_APPROVAL_REQUIRED,
                e.title,
                "You authored this proposal. Ask a teammate to review it, or tick \"Approve my own proposal without teammate review\" to self-approve.",
            );
        }
        Problem::new(status, team_code(e.code), e.title, e.detail)
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let body = serde_json::to_vec(&self.body()).unwrap_or_default();
        let mut resp = (self.status, body).into_response();
        resp.headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_CONTENT_TYPE));
        resp
    }
}

pub type ApiResult<T> = Result<T, Problem>;

/// Drop-in replacement for `axum::extract::Query` whose rejection is a
/// problem+json `400 INVALID_REQUEST` instead of axum's `text/plain` body
/// (e.g. `?limit=abc`, `?limit=-3`, or a missing required parameter).
#[derive(Debug, Clone, Copy, Default)]
pub struct Query<T>(pub T);

impl<T, S> axum::extract::FromRequestParts<S> for Query<T>
where
    T: serde::de::DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Problem;

    async fn from_request_parts(parts: &mut axum::http::request::Parts, state: &S) -> Result<Self, Self::Rejection> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(v)) => Ok(Query(v)),
            Err(rejection) => Err(Problem::bad_request(format!("Invalid query string: {}", rejection.body_text()))),
        }
    }
}
