use knobyte::config::KnobyteConfig;
use knobyte::drift::checker::run_drift_check;
use knobyte::drift::{grounding_score, sync_groundings, GroundingHealth};
use knobyte::graph::GraphEngine;
use knobyte::setup::run_setup;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn setup_project(root: &Path) -> KnobyteConfig {
    let config = KnobyteConfig::new(root.to_path_buf(), root.join(".knobyte"));
    run_setup(&config, "code-repo", false).unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    config
}

fn rebuild(config: &KnobyteConfig) -> GraphEngine {
    let mut engine = GraphEngine::open(&config.graph_db_path()).unwrap();
    engine.rebuild(&config.project_root).unwrap();
    engine
}

/// `knobyte graph ground --rebaseline`.
fn ground(config: &KnobyteConfig) -> usize {
    let engine = GraphEngine::open(&config.graph_db_path()).unwrap();
    engine.ground_all(&config.project_root).unwrap()
}

fn write_spec(
    config: &KnobyteConfig,
    name: &str,
    refs_frontmatter: &[&str],
    inline: &[&str],
) -> std::path::PathBuf {
    let specs = config.scaffold_root.join("specs");
    fs::create_dir_all(&specs).unwrap();
    let mut doc = format!(
        "---\nid: kb_{}\ntitle: {}\ntype: spec\ngrounds_to:\n",
        name, name
    );
    for r in refs_frontmatter {
        doc.push_str(&format!("  - {}\n", r));
    }
    doc.push_str("---\n# Spec\n\nAll incoming requests pass through token verification.\n\n");
    for r in inline {
        doc.push_str(&format!("<!-- kb-ground: {} -->\n", r));
    }
    let path = specs.join(format!("{}.md", name));
    fs::write(&path, doc).unwrap();
    path
}

#[test]
fn test_drift_check() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let scaffold = root.join(".knobyte");

    let config = KnobyteConfig::new(root, scaffold);
    run_setup(&config, "code-repo", false).unwrap();

    let report = run_drift_check(&config);
    assert_eq!(report.grounding_score, 0.0);
    assert!(!report.nudges.is_empty(), "{:?}", report.nudges);
    assert!(report.file_count >= 1);
    assert_eq!(report.grounding.total, 0);
    assert!(report
        .issues
        .iter()
        .all(|i| !i.message.contains("sync-groundings")));
}

#[test]
fn test_readme_style_readable_grounding_is_intact() {
    let dir = tempdir().unwrap();
    let config = setup_project(dir.path());
    fs::write(
        config.project_root.join("src/auth.rs"),
        "pub struct Auth;\n\nimpl Auth {\n    pub fn validate(&self, t: &str) -> bool {\n        !t.is_empty()\n    }\n}\n\n/// Validate a bearer token.\npub fn validate_token(token: &str) -> bool {\n    token.starts_with(\"Bearer \")\n}\n",
    )
    .unwrap();
    rebuild(&config);

    // Exactly the README example, plus a method referenced as `function` and with its
    // qualified name.
    write_spec(
        &config,
        "auth",
        &[
            "function:src/auth.rs:validate_token",
            "function:src/auth.rs:validate",
        ],
        &[
            "function:src/auth.rs:validate_token",
            "method:src/auth.rs:Auth::validate",
        ],
    );

    // Before any baseline exists, the references resolve but cannot be judged.
    let unbaselined = run_drift_check(&config);
    assert_eq!(unbaselined.grounding.unverified, 4, "{:?}", unbaselined.issues);
    assert!(unbaselined
        .issues
        .iter()
        .filter(|i| i.code.starts_with("GROUNDING_"))
        .all(|i| i.code == "GROUNDING_UNVERIFIED" && i.severity == "info"));

    ground(&config);
    // The anchor of a frontmatter reference is reported (and counted) on its own.
    let report = run_drift_check(&config);
    assert_eq!(report.grounding.total, 4, "{:?}", report.issues);
    assert_eq!(report.grounding.intact, 4, "{:?}", report.issues);
    assert_eq!(report.grounding.missing, 0);
    assert_eq!(report.grounding_score, 100.0);
    assert!(report
        .issues
        .iter()
        .all(|i| !i.code.starts_with("GROUNDING_")));
}

#[test]
fn test_body_change_reports_changed_and_ground_accepts() {
    let dir = tempdir().unwrap();
    let config = setup_project(dir.path());
    let code = config.project_root.join("src/tax.rs");
    fs::write(&code, "pub fn calculate_tax(amount: f64) -> f64 {\n    amount * 0.2\n}\n\npub fn unrelated() {}\n").unwrap();
    rebuild(&config);
    let spec = write_spec(&config, "tax", &["function:src/tax.rs:calculate_tax"], &[]);

    // `check` is read-only: no baseline, so the grounding is unverified (info) and the
    // document is not touched.
    let before = fs::read_to_string(&spec).unwrap();
    let first = run_drift_check(&config);
    assert_eq!(first.grounding.unverified, 1, "{:?}", first.issues);
    assert_eq!(first.grounding.intact, 0);
    assert_eq!(fs::read_to_string(&spec).unwrap(), before);
    assert_eq!(run_drift_check(&config).grounding.unverified, 1);
    assert_eq!(ground(&config), 1);
    assert_eq!(run_drift_check(&config).grounding.intact, 1);

    // Unrelated change: still intact (baselines are per grounded symbol). Rebuild must keep
    // the baseline table.
    fs::write(&code, "pub fn calculate_tax(amount: f64) -> f64 {\n    amount * 0.2\n}\n\npub fn unrelated() { let _x = 1; }\n").unwrap();
    rebuild(&config);
    let second = run_drift_check(&config);
    assert_eq!(second.grounding.intact, 1, "{:?}", second.issues);

    // Change the grounded body.
    fs::write(&code, "pub fn calculate_tax(amount: f64) -> f64 {\n    amount * 0.25\n}\n\npub fn unrelated() { let _x = 1; }\n").unwrap();
    let engine = rebuild(&config);
    let changed = run_drift_check(&config);
    assert_eq!(changed.grounding.changed, 1, "{:?}", changed.issues);
    assert_eq!(changed.grounding.intact, 0);
    assert_eq!(changed.grounding_score, 0.0);
    assert!(changed
        .issues
        .iter()
        .any(|i| i.symbol.as_deref() == Some("function:src/tax.rs:calculate_tax")));

    // `graph ground` accepts the current code, and only baselines grounded refs.
    let grounded = engine.ground_all(&config.project_root).unwrap();
    assert_eq!(grounded, 1);
    let accepted = run_drift_check(&config);
    assert_eq!(accepted.grounding.intact, 1);
    assert_eq!(accepted.grounding.changed, 0);
    assert_eq!(accepted.grounding_score, 100.0);
}

#[test]
fn test_moved_function_is_relocated_with_readable_ref() {
    let dir = tempdir().unwrap();
    let config = setup_project(dir.path());
    let src = config.project_root.join("src");
    let old_file = src.join("old_math.rs");
    let body = "pub fn calculate_tax(amount: f64) -> f64 {\n    amount * 0.2\n}\n";
    fs::write(&old_file, body).unwrap();
    rebuild(&config);

    let old_ref = "function:src/old_math.rs:calculate_tax";
    let spec_file = config.scaffold_root.join("specs").join("tax.md");
    fs::create_dir_all(spec_file.parent().unwrap()).unwrap();
    let spec = format!(
        "---\nid: kb_tax\ntitle: Tax Spec\ngrounds_to:\n  - {old}\n---\n# Tax Spec\n\nHistorically lived at {old} (prose, must not change).\n\n<!-- kb-ground: {old} -->\n",
        old = old_ref
    );
    fs::write(&spec_file, &spec).unwrap();
    ground(&config);
    let spec = fs::read_to_string(&spec_file).unwrap();

    let initial = run_drift_check(&config);
    assert_eq!(initial.grounding_score, 100.0);

    // Move the function to another file.
    fs::remove_file(&old_file).unwrap();
    fs::write(src.join("new_math.rs"), body).unwrap();
    rebuild(&config);

    let drifted = run_drift_check(&config);
    assert_eq!(drifted.grounding.missing, 2);
    assert_eq!(drifted.grounding.moved, 2);
    assert!(drifted.grounding_score < 100.0);
    // A clean move of the frontmatter grounding adds no issue; the anchor should move.
    let grounding: Vec<_> = drifted
        .issues
        .iter()
        .filter(|i| i.code.starts_with("GROUNDING_"))
        .collect();
    assert_eq!(grounding.len(), 1, "{:?}", grounding);
    assert!(grounding[0].line.is_some());
    assert!(grounding[0].message.contains("Inline anchor should move"));

    // Dry run: accurate proposal, nothing written.
    let dry = sync_groundings(&config, true).unwrap();
    assert!(dry.dry_run);
    assert_eq!(dry.relocated_count, 0);
    assert_eq!(dry.proposals.len(), 1);
    assert_eq!(fs::read_to_string(&spec_file).unwrap(), spec);

    let res = sync_groundings(&config, false).unwrap();
    assert_eq!(res.relocated_count, 1);
    assert_eq!(res.proposals.len(), 1);
    let p = &res.proposals[0];
    assert_eq!(p.symbol_name, "calculate_tax");
    assert_eq!(p.new_file, "src/new_math.rs");
    assert_eq!(p.confidence, 1.0); // exact body hash from the committed baseline
    let new_ref = "function:src/new_math.rs:calculate_tax";
    assert_eq!(p.new_node_id, new_ref);

    let updated = fs::read_to_string(&spec_file).unwrap();
    assert!(updated.contains(&format!("  - ref: {}\n", new_ref)), "{}", updated);
    assert!(updated.contains(&format!("<!-- kb-ground: {} #", new_ref)), "{}", updated);
    assert!(updated.contains(&format!("Historically lived at {} (prose", old_ref)));
    // Never a hashed id.
    let hashed = regex::Regex::new(r"[a-z_]+:[0-9a-f]{32}").unwrap();
    assert!(!hashed.is_match(&updated), "{}", updated);

    let healed = run_drift_check(&config);
    assert_eq!(healed.grounding_score, 100.0);
    assert_eq!(healed.grounding.missing, 0);
    assert_eq!(healed.grounding.intact, 2);

    // Nothing left to relocate; resolving refs are untouched by sync.
    let again = sync_groundings(&config, false).unwrap();
    assert_eq!(again.relocated_count, 0);
    assert!(again.proposals.is_empty());
    assert_eq!(fs::read_to_string(&spec_file).unwrap(), updated);
}

#[test]
fn test_moved_and_changed_method_keeps_changed_status() {
    let dir = tempdir().unwrap();
    let config = setup_project(dir.path());
    let src = config.project_root.join("src");
    fs::write(
        src.join("a.rs"),
        "pub struct Ledger;\nimpl Ledger {\n    pub fn post_entry(&self) -> u32 {\n        1\n    }\n}\n",
    )
    .unwrap();
    rebuild(&config);
    let spec = write_spec(&config, "ledger", &["function:src/a.rs:post_entry"], &[]);
    ground(&config);
    assert_eq!(run_drift_check(&config).grounding.intact, 1);

    // Move AND change the method body: relocated by name (method matches `function`).
    fs::remove_file(src.join("a.rs")).unwrap();
    fs::write(
        src.join("b.rs"),
        "pub struct Ledger;\nimpl Ledger {\n    pub fn post_entry(&self) -> u32 {\n        2\n    }\n}\n",
    )
    .unwrap();
    rebuild(&config);

    let res = sync_groundings(&config, false).unwrap();
    assert_eq!(res.relocated_count, 1, "{:?}", res);
    assert_eq!(
        res.proposals[0].new_node_id,
        "function:src/b.rs:Ledger::post_entry"
    );
    assert!(fs::read_to_string(&spec)
        .unwrap()
        .contains("  - ref: function:src/b.rs:Ledger::post_entry\n"));

    // The baseline moved with the ref, so the body change is still visible.
    let report = run_drift_check(&config);
    assert_eq!(report.grounding.changed, 1, "{:?}", report.issues);
    assert_eq!(report.grounding.missing, 0);
}

#[test]
fn test_legacy_hashed_id_still_resolves_and_relocates_to_readable_ref() {
    let dir = tempdir().unwrap();
    let config = setup_project(dir.path());
    let src = config.project_root.join("src");
    let body = "pub fn calculate_tax(amount: f64) -> f64 {\n    amount * 0.2\n}\n";
    fs::write(src.join("old_math.rs"), body).unwrap();
    let engine = rebuild(&config);
    let legacy_id = engine.query_where_defined("calculate_tax").unwrap()[0]
        .id
        .clone();

    let spec = write_spec(&config, "legacy", &[&legacy_id], &[&legacy_id]);
    ground(&config);
    let report = run_drift_check(&config);
    assert_eq!(report.grounding.intact, 2, "{:?}", report.issues);

    fs::remove_file(src.join("old_math.rs")).unwrap();
    fs::write(src.join("new_math.rs"), body).unwrap();
    rebuild(&config);

    let res = sync_groundings(&config, false).unwrap();
    assert_eq!(res.relocated_count, 1);
    let content = fs::read_to_string(&spec).unwrap();
    assert!(!content.contains(&legacy_id));
    assert_eq!(
        content
            .matches("function:src/new_math.rs:calculate_tax")
            .count(),
        2
    );
    assert_eq!(run_drift_check(&config).grounding_score, 100.0);
}

#[test]
fn test_score_is_intact_share_without_issue_penalty() {
    assert_eq!(
        grounding_score(&GroundingHealth {
            intact: 3,
            changed: 0,
            missing: 1,
            total: 4,
            ..Default::default()
        }),
        75.0
    );
    assert_eq!(grounding_score(&GroundingHealth::default()), 0.0);

    let dir = tempdir().unwrap();
    let config = setup_project(dir.path());
    fs::write(
        config.project_root.join("src/a.rs"),
        "pub fn one() {}\npub fn two() {}\npub fn three() {}\n",
    )
    .unwrap();
    rebuild(&config);
    write_spec(
        &config,
        "score",
        &[
            "function:src/a.rs:one",
            "function:src/a.rs:two",
            "function:src/a.rs:three",
            "function:src/a.rs:missing_one",
        ],
        &[],
    );
    ground(&config);
    let report = run_drift_check(&config);
    assert_eq!(report.grounding.total, 4);
    assert_eq!(report.grounding.intact, 3);
    assert_eq!(report.grounding.missing, 1);
    let grounding: Vec<_> = report
        .issues
        .iter()
        .filter(|i| i.code.starts_with("GROUNDING_"))
        .collect();
    assert_eq!(grounding.len(), 1);
    assert_eq!(grounding[0].code, "GROUNDING_GONE");
    assert_eq!(grounding[0].severity, "error");
    // The grounding metric is the intact share (3/4); the headline score deducts per issue.
    assert_eq!(report.grounding_score, 75.0);
    assert_eq!(
        report.score,
        knobyte::drift::compute_score(&report.issues) as f64
    );
    assert_eq!(report.grounding.gone, 1);
}
