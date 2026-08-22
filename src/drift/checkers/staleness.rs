//! STALE_FILE: scaffold files that have not changed in a long time (git age or commits since
//! the last change), or whose `last_updated` frontmatter date is old.

use std::path::Path;
use std::process::Command;

use chrono::{NaiveDate, Utc};

use crate::config::StalenessThresholds;
use crate::drift::types::{codes, DriftIssue, SEVERITY_ERROR, SEVERITY_INFO, SEVERITY_WARNING};

struct Signal {
    severity: &'static str,
    message: String,
}

fn rank(severity: &str) -> u8 {
    match severity {
        SEVERITY_ERROR => 2,
        SEVERITY_WARNING => 1,
        _ => 0,
    }
}

fn days_signal(days: i64, t: &StalenessThresholds) -> Option<Signal> {
    if days >= t.error_days {
        Some(Signal {
            severity: SEVERITY_ERROR,
            message: format!(
                "File hasn't been updated in {} days (threshold: {}d)",
                days, t.error_days
            ),
        })
    } else if days >= t.warn_days {
        Some(Signal {
            severity: SEVERITY_WARNING,
            message: format!(
                "File hasn't been updated in {} days (threshold: {}d)",
                days, t.warn_days
            ),
        })
    } else {
        None
    }
}

fn commits_signal(commits: i64, t: &StalenessThresholds) -> Option<Signal> {
    if commits >= t.error_commits {
        Some(Signal {
            severity: SEVERITY_ERROR,
            message: format!(
                "{} commits since file was last updated (threshold: {})",
                commits, t.error_commits
            ),
        })
    } else if commits >= t.warn_commits {
        Some(Signal {
            severity: SEVERITY_WARNING,
            message: format!(
                "{} commits since file was last updated (threshold: {})",
                commits, t.warn_commits
            ),
        })
    } else {
        None
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Hash and unix commit time of the last commit touching `file` (project-relative).
fn last_commit(file: &str, cwd: &Path) -> Option<(String, i64)> {
    let out = git(cwd, &["log", "-1", "--format=%H %ct", "--", file])?;
    let (hash, ts) = out.split_once(' ')?;
    Some((hash.to_string(), ts.trim().parse().ok()?))
}

/// Whole days since `file` was last changed in git; `None` when untracked or not a repository.
pub fn days_since_last_change(file: &str, cwd: &Path) -> Option<i64> {
    let (_, ts) = last_commit(file, cwd)?;
    Some((Utc::now().timestamp() - ts).div_euclid(86_400))
}

/// Commits made since `file` was last changed; `None` when untracked or not a repository.
pub fn commits_since_last_change(file: &str, cwd: &Path) -> Option<i64> {
    let (hash, _) = last_commit(file, cwd)?;
    git(cwd, &["rev-list", "--count", &format!("{}..HEAD", hash)])?
        .parse()
        .ok()
}

/// Whole days since a `YYYY-MM-DD` frontmatter date. Placeholders, other formats and future
/// dates yield `None`.
pub fn days_since_frontmatter_date(value: Option<&str>, today: NaiveDate) -> Option<i64> {
    let value = value?.trim();
    if value.contains('[') || value.contains(']') {
        return None;
    }
    if value.len() != 10
        || !value.chars().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                c == '-'
            } else {
                c.is_ascii_digit()
            }
        })
    {
        return None;
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    let days = (today - date).num_days();
    (days >= 0).then_some(days)
}

/// Combine age signals for one file into at most one STALE_FILE issue at the highest severity.
/// `days`/`commits` come from git (pass `None` when unknown).
pub fn staleness_issue(
    source: &str,
    days: Option<i64>,
    commits: Option<i64>,
    last_updated_days: Option<i64>,
    t: &StalenessThresholds,
) -> Option<DriftIssue> {
    let mut signals = Vec::new();
    if let Some(s) = days.and_then(|d| days_signal(d, t)) {
        signals.push(s);
    }
    if let Some(s) = commits.and_then(|c| commits_signal(c, t)) {
        signals.push(s);
    }
    if let Some(field_days) = last_updated_days {
        if let Some(s) = days_signal(field_days, t) {
            let threshold = if s.severity == SEVERITY_ERROR {
                t.error_days
            } else {
                t.warn_days
            };
            signals.push(Signal {
                severity: s.severity,
                message: format!(
                    "last_updated is {} days old (threshold: {}d)",
                    field_days, threshold
                ),
            });
        }
    }
    if signals.is_empty() {
        return None;
    }
    let severity = signals
        .iter()
        .map(|s| s.severity)
        .fold(
            SEVERITY_INFO,
            |acc, s| if rank(s) > rank(acc) { s } else { acc },
        );
    let message = signals
        .iter()
        .map(|s| s.message.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    Some(DriftIssue::new(
        codes::STALE_FILE,
        severity,
        source,
        None,
        message,
    ))
}

/// Check how stale one scaffold file is. `source` is its project-relative path.
pub fn check_staleness(
    source: &str,
    project_root: &Path,
    thresholds: &StalenessThresholds,
    last_updated: Option<&str>,
) -> Vec<DriftIssue> {
    let days = days_since_last_change(source, project_root);
    let commits = commits_since_last_change(source, project_root);
    let field_days = days_since_frontmatter_date(last_updated, Utc::now().date_naive());
    staleness_issue(source, days, commits, field_days, thresholds)
        .into_iter()
        .collect()
}
