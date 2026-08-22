//! Reconciler parity: MinHash/LSH fingerprints and neighbour evidence decide MOVED / GONE /
//! AMBIGUOUS for grounded symbols that were renamed or moved; exact body-hash and readable
//! reference strategies still run first.

use knobyte::config::KnobyteConfig;
use knobyte::drift::checker::run_drift_check;
use knobyte::drift::sync::{reconcile_missing_ref, MoveEvidence, Reconciliation};
use knobyte::drift::sync_groundings;
use knobyte::graph::fingerprint::MinHash;
use knobyte::graph::reconcile::{reconcile, BaselineFingerprint, Verdict};
use knobyte::graph::GraphEngine;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn config(root: &Path) -> KnobyteConfig {
    KnobyteConfig::new(root.to_path_buf(), root.join(".knobyte"))
}

fn rebuild(root: &Path) -> GraphEngine {
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    engine.rebuild(root).unwrap();
    engine
}

/// `knobyte graph ground --rebaseline`: cache and commit the baselines.
fn ground(root: &Path) -> usize {
    let engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    engine.ground_all(root).unwrap()
}

fn spec(root: &Path, name: &str, reference: &str) {
    write(
        root,
        &format!(".knobyte/specs/{}.md", name),
        &format!(
            "---\nid: kb_{n}\ntitle: {n}\ntype: spec\ngrounds_to:\n  - {r}\n---\n# {n}\n\nSee {r}.\n",
            n = name,
            r = reference
        ),
    );
}

const LARGE: &str = r#"pub fn settle_account(balance: i64, payments: &[i64], fee: i64) -> i64 {
    let mut total = balance;
    for p in payments {
        if *p > 0 {
            total -= *p;
        } else {
            total += fee;
        }
    }
    if total < 0 {
        return 0;
    }
    total
}
"#;

#[test]
fn minhash_sketches_ignore_identifier_spelling() {
    let a = MinHash::of_body(LARGE);
    let renamed = MinHash::of_body(&LARGE.replace("settle_account", "close_ledger").replace("total", "acc"));
    assert_eq!(a.similarity(&renamed), 1.0);
    let different = MinHash::of_body("pub fn x() -> String { format!(\"{}\", 1) }");
    assert!(a.similarity(&different) < 0.3);
    assert_eq!(a.band_hashes(), renamed.band_hashes());
    assert!(a.token_count >= 30);
}

#[test]
fn renamed_and_moved_large_function_is_moved_by_fingerprint() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/billing.rs", LARGE);
    write(
        root,
        "src/api.rs",
        "use crate::billing::settle_account;\npub fn checkout() -> i64 { settle_account(10, &[1], 2) }\n",
    );
    rebuild(root);
    let reference = "function:src/billing.rs:settle_account";
    spec(root, "billing", reference);
    let cfg = config(root);
    // Rebaselining records the baseline (body + neighbours).
    assert_eq!(ground(root), 1);
    let first = run_drift_check(&cfg);
    assert_eq!(first.grounding.intact, 1, "{:?}", first.issues);

    // Rename and move: the name strategy cannot find it, the body hash differs (name is in it).
    fs::remove_file(root.join("src/billing.rs")).unwrap();
    write(root, "src/accounts/settlement.rs", &LARGE.replace("settle_account", "close_account"));
    write(
        root,
        "src/api.rs",
        "use crate::accounts::settlement::close_account;\npub fn checkout() -> i64 { close_account(10, &[1], 2) }\n",
    );
    let engine = rebuild(root);
    match reconcile_missing_ref(engine.connection(), "specs/billing.md", reference) {
        Reconciliation::Moved { proposal, evidence } => {
            assert_eq!(evidence, MoveEvidence::Fingerprint);
            assert_eq!(proposal.new_node_id, "function:src/accounts/settlement.rs:close_account");
            assert!(proposal.confidence >= 0.85, "{}", proposal.confidence);
        }
        other => panic!("expected MOVED, got {:?}", other),
    }
    // `knobyte sync` relocation rewrites the anchor.
    let result = sync_groundings(&cfg, false).unwrap();
    assert_eq!(result.relocated_count, 1, "{:?}", result.proposals);
    let doc = fs::read_to_string(root.join(".knobyte/specs/billing.md")).unwrap();
    assert!(doc.contains("function:src/accounts/settlement.rs:close_account"), "{}", doc);
}

#[test]
fn renamed_small_function_is_moved_by_neighbours_and_reported() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let lib = |name: &str| {
        format!(
            "pub fn {n}(x: i32) -> i32 {{ x + 1 }}\npub fn first() -> i32 {{ {n}(1) }}\npub fn second() -> i32 {{ {n}(2) }}\npub fn third() -> i32 {{ {n}(3) }}\n",
            n = name
        )
    };
    write(root, "src/math.rs", &lib("bump"));
    rebuild(root);
    let reference = "function:src/math.rs:bump";
    spec(root, "math", reference);
    let cfg = config(root);
    ground(root);
    assert_eq!(run_drift_check(&cfg).grounding.intact, 1);

    write(root, "src/math.rs", &lib("increment"));
    let engine = rebuild(root);
    match reconcile_missing_ref(engine.connection(), "specs/math.md", reference) {
        Reconciliation::Moved { proposal, evidence } => {
            assert_eq!(evidence, MoveEvidence::Neighbors);
            assert_eq!(proposal.new_node_id, "function:src/math.rs:increment");
        }
        other => panic!("expected MOVED by neighbours, got {:?}", other),
    }
    let report = run_drift_check(&cfg);
    let moved: Vec<_> = report
        .issues
        .iter()
        .filter(|i| i.code == "GROUNDING_MOVED_BY_NEIGHBORS")
        .collect();
    assert_eq!(moved.len(), 1, "{:?}", report.issues);
    // The notice names the evidence that decided it.
    assert!(
        moved[0].message.contains("matched by callers and callees, not body"),
        "{}",
        moved[0].message
    );
}

#[test]
fn deleted_function_is_gone_and_twins_are_ambiguous() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/billing.rs", LARGE);
    rebuild(root);
    spec(root, "billing", "function:src/billing.rs:settle_account");
    ground(root);

    // Deleted outright.
    write(root, "src/billing.rs", "pub fn unrelated() -> u8 { 1 }\n");
    let engine = rebuild(root);
    assert!(matches!(
        reconcile_missing_ref(engine.connection(), "specs/billing.md", "function:src/billing.rs:settle_account"),
        Reconciliation::Gone
    ));

    // Two renamed copies with the same body: identity cannot be decided.
    write(root, "src/a.rs", &LARGE.replace("settle_account", "settle_a"));
    write(root, "src/b.rs", &LARGE.replace("settle_account", "settle_b"));
    let engine = rebuild(root);
    let fp = BaselineFingerprint {
        minhash: MinHash::of_body(LARGE),
        neighbors: Vec::new(),
        body_hash: None,
        kind: Some("function".into()),
    };
    match reconcile(engine.connection(), &fp, None) {
        Verdict::Ambiguous { candidate, .. } => assert!(candidate.starts_with("function:")),
        other => panic!("expected AMBIGUOUS, got {:?}", other),
    }
    match reconcile_missing_ref(engine.connection(), "specs/billing.md", "function:src/billing.rs:settle_account") {
        Reconciliation::Ambiguous(c) => assert!(!c.is_empty()),
        other => panic!("expected AMBIGUOUS, got {:?}", other),
    }
}

#[test]
fn exact_body_hash_still_wins_first() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    write(root, "src/billing.rs", LARGE);
    rebuild(root);
    spec(root, "billing", "function:src/billing.rs:settle_account");
    ground(root);
    // Pure move (same text): strategy A (exact body hash) decides.
    fs::remove_file(root.join("src/billing.rs")).unwrap();
    write(root, "src/moved.rs", LARGE);
    let engine = rebuild(root);
    match reconcile_missing_ref(engine.connection(), "specs/billing.md", "function:src/billing.rs:settle_account") {
        Reconciliation::Moved { evidence, proposal } => {
            assert_eq!(evidence, MoveEvidence::Body);
            assert_eq!(proposal.confidence, 1.0);
        }
        other => panic!("expected MOVED by body hash, got {:?}", other),
    }
}
