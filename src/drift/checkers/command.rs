//! DEAD_COMMAND: npm/yarn/pnpm/bun scripts, make targets and `swift run` executables named in
//! the scaffold must exist.

use std::collections::HashSet;
use std::fs;
use std::sync::OnceLock;

use regex::Regex;

use super::{read_json, CheckContext};
use crate::drift::types::{codes, Claim, ClaimKind, DriftIssue, SEVERITY_ERROR};

pub(crate) fn npm_script_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:npm\s+run|yarn|pnpm|bun\s+run)\s+(\S+)").unwrap())
}

fn make_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^make\s+(\S+)").unwrap())
}

/// `swift run [options] <executable> [arguments]`.
fn swift_run_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^swift\s+run\s+(?:(?:-c|--configuration|--package-path|--scratch-path)\s+\S+\s+|--?[\w-]+\s+)*([A-Za-z_][\w-]*)").unwrap())
}

fn make_target_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\w[\w-]*):").unwrap())
}

/// Script names in the root `package.json`, `None` when there is no readable manifest.
pub(crate) fn load_package_scripts(ctx: &CheckContext) -> Option<Vec<String>> {
    let pkg = read_json(&ctx.project_root.join("package.json"))?;
    Some(
        pkg.get("scripts")
            .and_then(|s| s.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default(),
    )
}

fn load_make_targets(ctx: &CheckContext) -> Option<HashSet<String>> {
    let content = fs::read_to_string(ctx.project_root.join("Makefile")).ok()?;
    Some(
        content
            .lines()
            .filter_map(|l| make_target_re().captures(l).map(|c| c[1].to_string()))
            .collect(),
    )
}

/// Check that claimed commands exist.
pub fn check_commands(claims: &[Claim], ctx: &CheckContext) -> Vec<DriftIssue> {
    let scripts: Option<HashSet<String>> =
        load_package_scripts(ctx).map(|v| v.into_iter().collect());
    let targets = load_make_targets(ctx);
    let swift_exes = fs::read_to_string(ctx.project_root.join("Package.swift"))
        .ok()
        .map(|m| super::dependency::swift_executables(&m));
    let mut issues = Vec::new();

    for claim in claims
        .iter()
        .filter(|c| c.kind == ClaimKind::Command && !c.negated)
    {
        let cmd = claim.value.trim();
        if let Some(caps) = npm_script_re().captures(cmd) {
            let script = &caps[1];
            if let Some(scripts) = &scripts {
                if !scripts.contains(script) {
                    issues.push(DriftIssue::from_claim(
                        codes::DEAD_COMMAND,
                        SEVERITY_ERROR,
                        claim,
                        format!("Script \"{}\" not found in package.json scripts", script),
                    ));
                }
            }
            continue;
        }
        if let Some(caps) = swift_run_re().captures(cmd) {
            let exe = &caps[1];
            if let Some(exes) = swift_exes.as_ref().filter(|e| !e.is_empty()) {
                if !exes.iter().any(|e| e == exe) {
                    issues.push(DriftIssue::from_claim(
                        codes::DEAD_COMMAND,
                        SEVERITY_ERROR,
                        claim,
                        format!("Executable \"{}\" not found in Package.swift", exe),
                    ));
                }
            }
            continue;
        }
        if let Some(caps) = make_re().captures(cmd) {
            let target = &caps[1];
            if let Some(targets) = &targets {
                if !targets.contains(target) {
                    issues.push(DriftIssue::from_claim(
                        codes::DEAD_COMMAND,
                        SEVERITY_ERROR,
                        claim,
                        format!("Make target \"{}\" not found in Makefile", target),
                    ));
                }
            }
        }
    }
    issues
}
