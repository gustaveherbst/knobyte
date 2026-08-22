//! CROSS_FILE_CONFLICT: contradictory version claims, or one script run through different
//! package managers in different files.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use regex::Regex;

use super::command::npm_script_re;
use crate::drift::types::{codes, Claim, ClaimKind, DriftIssue, SEVERITY_ERROR, SEVERITY_WARNING};

pub(crate) fn version_claim_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(.+?)\s+v?(\d[\d.]*\S*)$").unwrap())
}

/// Insertion-ordered grouping.
fn group<'a>(items: impl Iterator<Item = (String, &'a Claim)>) -> Vec<(String, Vec<&'a Claim>)> {
    let mut groups: Vec<(String, Vec<&Claim>)> = Vec::new();
    for (key, claim) in items {
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(claim),
            None => groups.push((key, vec![claim])),
        }
    }
    groups
}

pub fn check_cross_file(claims: &[Claim]) -> Vec<DriftIssue> {
    let mut issues = Vec::new();

    let versions = group(
        claims
            .iter()
            .filter(|c| c.kind == ClaimKind::Version && !c.negated)
            .filter_map(|c| {
                version_claim_re()
                    .captures(&c.value)
                    .map(|caps| (caps[1].trim().to_lowercase(), c))
            }),
    );
    for (dep, list) in versions {
        if list.len() < 2 {
            continue;
        }
        let unique: BTreeSet<&str> = list.iter().map(|c| c.value.as_str()).collect();
        if unique.len() > 1 {
            let sources = list
                .iter()
                .map(|c| format!("{}:{} says \"{}\"", c.source, c.line, c.value))
                .collect::<Vec<_>>()
                .join(", ");
            issues.push(DriftIssue::new(
                codes::CROSS_FILE_CONFLICT,
                SEVERITY_ERROR,
                list[0].source.clone(),
                Some(list[0].line),
                format!("Conflicting versions for \"{}\": {}", dep, sources),
            ));
        }
    }

    let commands = group(
        claims
            .iter()
            .filter(|c| c.kind == ClaimKind::Command && !c.negated)
            .filter_map(|c| {
                npm_script_re()
                    .captures(&c.value)
                    .map(|caps| (caps[1].to_string(), c))
            }),
    );
    for (script, list) in commands {
        if list.len() < 2 {
            continue;
        }
        let files: BTreeSet<&str> = list.iter().map(|c| c.source.as_str()).collect();
        if files.len() < 2 {
            continue;
        }
        let mut managers: Vec<&str> = Vec::new();
        for c in &list {
            let m = c.value.split_whitespace().next().unwrap_or("");
            if !managers.contains(&m) {
                managers.push(m);
            }
        }
        if managers.len() > 1 {
            issues.push(DriftIssue::new(
                codes::CROSS_FILE_CONFLICT,
                SEVERITY_WARNING,
                list[0].source.clone(),
                Some(list[0].line),
                format!(
                    "Script \"{}\" referenced with different package managers across files: {}",
                    script,
                    managers.join(", ")
                ),
            ));
        }
    }
    issues
}
