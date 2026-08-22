//! Drift score: starts at 100 and deducts per issue (error 10, warning 3, info 1).

use crate::drift::types::{codes, DriftIssue, SEVERITY_ERROR, SEVERITY_INFO, SEVERITY_WARNING};

/// Notices that are reported but are not drift, so they cost nothing. A move decided by
/// neighbours is a correct rebind that would otherwise be silent.
pub const UNSCORED_CODES: &[&str] = &[codes::GROUNDING_MOVED_BY_NEIGHBORS];

/// Points deducted for one issue of `severity`.
pub fn severity_cost(severity: &str) -> i64 {
    match severity {
        SEVERITY_ERROR => 10,
        SEVERITY_WARNING => 3,
        SEVERITY_INFO => 1,
        _ => 0,
    }
}

/// Compute the drift score (0-100) from the reported issues.
pub fn compute_score(issues: &[DriftIssue]) -> i64 {
    let mut score: i64 = 100;
    for issue in issues {
        if UNSCORED_CODES.contains(&issue.code.as_str()) {
            continue;
        }
        score -= severity_cost(&issue.severity);
    }
    score.clamp(0, 100)
}
