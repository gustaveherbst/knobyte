//! UNDOCUMENTED_SCRIPT: package.json scripts the scaffold never mentions.

use super::command::load_package_scripts;
use super::CheckContext;
use crate::drift::types::{codes, DriftIssue, SEVERITY_WARNING};

/// npm lifecycle hooks that need no documentation.
const IGNORED_SCRIPTS: &[&str] = &[
    "preinstall",
    "install",
    "postinstall",
    "preuninstall",
    "uninstall",
    "postuninstall",
    "prepublish",
    "prepublishOnly",
    "publish",
    "postpublish",
    "prepack",
    "pack",
    "postpack",
    "prepare",
    "preshrinkwrap",
    "shrinkwrap",
    "postshrinkwrap",
];

/// `scaffold_text` is the concatenated content of every scaffold file.
pub fn check_script_coverage(scaffold_text: &str, ctx: &CheckContext) -> Vec<DriftIssue> {
    let Some(scripts) = load_package_scripts(ctx) else {
        return Vec::new();
    };
    if scripts.is_empty() {
        return Vec::new();
    }
    let has = |s: &str| scripts.iter().any(|x| x == s);
    let mut issues = Vec::new();
    for script in &scripts {
        if IGNORED_SCRIPTS.contains(&script.as_str()) {
            continue;
        }
        if (script.starts_with("pre") && has(&script[3..]))
            || (script.starts_with("post") && has(&script[4..]))
        {
            continue;
        }
        if let Some((base, _)) = script.split_once(':') {
            if scaffold_text.contains(base) {
                continue;
            }
        }
        if !scaffold_text.contains(script.as_str()) {
            issues.push(DriftIssue::new(
                codes::UNDOCUMENTED_SCRIPT,
                SEVERITY_WARNING,
                "package.json",
                None,
                format!(
                    "Script \"{}\" exists in package.json but is not mentioned in any scaffold file",
                    script
                ),
            ));
        }
    }
    issues
}
