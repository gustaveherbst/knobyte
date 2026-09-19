use tempfile::tempdir;
use std::fs;
use knobyte::config::KnobyteConfig;
use knobyte::drift::checker::run_drift_check;
use knobyte::drift::sync_groundings;
use knobyte::graph::GraphEngine;
use knobyte::setup::run_setup;

#[test]
fn test_drift_check() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let scaffold = root.join(".knobyte");

    let config = KnobyteConfig::new(root, scaffold);
    run_setup(&config, "code-repo", false).unwrap();

    let report = run_drift_check(&config);
    assert_eq!(report.score, 0.0);
    assert_eq!(report.status, "unmeasured");
    assert!(report.file_count >= 1);
    assert_eq!(report.grounding.total, 0);
}

#[test]
fn test_grounding_relocation_and_sync() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let scaffold = root.join(".knobyte");

    let config = KnobyteConfig::new(root.clone(), scaffold.clone());
    run_setup(&config, "code-repo", false).unwrap();

    // 1. Create source file in src/old_math.rs
    let src_dir = root.join("src");
    fs::create_dir_all(&src_dir).unwrap();
    let old_file = src_dir.join("old_math.rs");
    fs::write(&old_file, "pub fn calculate_tax(amount: f64) -> f64 {\n    amount * 0.2\n}\n").unwrap();

    // 2. Build code graph & ground all
    let mut engine = GraphEngine::open(&config.graph_db_path()).unwrap();
    let summary = engine.rebuild(&root).unwrap();
    assert!(summary.nodes_indexed >= 1);
    let grounded = engine.ground_all(&root).unwrap();
    assert!(grounded >= 1);

    let nodes = engine.query_where_defined("calculate_tax").unwrap();
    assert_eq!(nodes.len(), 1);
    let old_node_id = nodes[0].id.clone();

    // 3. Create a spec file referencing this node via frontmatter AND inline anchor
    let specs_dir = scaffold.join("specs");
    fs::create_dir_all(&specs_dir).unwrap();
    let spec_file = specs_dir.join("tax.md");
    let initial_spec = format!(
        r#"---
id: kb_tax
title: Tax Spec
type: spec
grounds_to:
  - {}
---
# Tax Spec

<!-- kb-ground: {} -->

Handles progressive taxation calculations.
"#,
        old_node_id, old_node_id
    );
    fs::write(&spec_file, initial_spec).unwrap();

    // 4. Verify initial drift check is 100% clean
    let initial_report = run_drift_check(&config);
    assert_eq!(initial_report.score, 100.0);
    assert_eq!(initial_report.grounding.missing, 0);
    assert!(initial_report.grounding.intact >= 1);

    // 5. Simulate code refactoring: move calculate_tax to src/new_math.rs
    fs::remove_file(&old_file).unwrap();
    let new_file = src_dir.join("new_math.rs");
    fs::write(&new_file, "pub fn calculate_tax(amount: f64) -> f64 {\n    amount * 0.2\n}\n").unwrap();

    // 6. Rebuild graph with new code location
    engine.rebuild(&root).unwrap();

    // 7. Verify drift is detected (node missing from old path)
    let drifted_report = run_drift_check(&config);
    assert!(drifted_report.grounding.missing >= 1);
    assert!(drifted_report.score < 100.0);

    // 8. Run sync_groundings to automatically relocate anchors!
    let sync_res = sync_groundings(&config, false).unwrap();
    assert_eq!(sync_res.relocated_count, 1);
    assert_eq!(sync_res.proposals.len(), 1);
    assert_eq!(sync_res.proposals[0].symbol_name, "calculate_tax");
    assert_eq!(sync_res.proposals[0].new_file, "src/new_math.rs");
    assert_eq!(sync_res.proposals[0].confidence, 1.0); // Exact AST body hash match

    let new_node_id = sync_res.proposals[0].new_node_id.clone();
    assert_ne!(old_node_id, new_node_id);

    // 9. Verify the spec file on disk was rewritten with the new node ID
    let updated_spec_content = fs::read_to_string(&spec_file).unwrap();
    assert!(updated_spec_content.contains(&new_node_id));
    assert!(!updated_spec_content.contains(&old_node_id));

    // 10. Verify drift is completely healed and back to 100% score!
    let healed_report = run_drift_check(&config);
    assert_eq!(healed_report.score, 100.0);
    assert_eq!(healed_report.grounding.missing, 0);
    assert!(healed_report.grounding.intact >= 1);
}
