//! Grounding baselines and drift-check correctness: committed baselines, read-only `check`,
//! exit codes, `check --fix`, exact re-extraction of edited files, freshness gating, wording,
//! and the scaffold walk.

use std::fs;
use std::path::Path;
use std::process::Command;

use knobyte::config::KnobyteConfig;
use knobyte::drift::checker::run_drift_check;
use knobyte::drift::checkers::grounding_shape::check_grounding_shape_in;
use knobyte::drift::{
    find_grounding_relocations, run_drift_check_with, sync_groundings, DriftCheckOptions,
    GraphState,
};
use knobyte::graph::grounding::{
    apply_committed_baselines, extract_doc_refs, parse_fingerprint, scaffold_markdown_files,
    split_anchor, RefOrigin,
};
use knobyte::graph::GraphEngine;
use knobyte::wiki::parser::parse_markdown_entity;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn project() -> (tempfile::TempDir, KnobyteConfig) {
    let dir = tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, ".knobyte/ROUTER.md", "# Router\n");
    write(&root, "CLAUDE.md", "Read .knobyte/ROUTER.md\n");
    let config = KnobyteConfig::new(root.clone(), root.join(".knobyte"));
    (dir, config)
}

fn rebuild(config: &KnobyteConfig) {
    let mut engine = GraphEngine::open(&config.graph_db_path()).unwrap();
    engine.rebuild(&config.project_root).unwrap();
}

fn ground(config: &KnobyteConfig) -> usize {
    let engine = GraphEngine::open(&config.graph_db_path()).unwrap();
    engine.ground_all(&config.project_root).unwrap()
}

fn delete_graph(config: &KnobyteConfig) {
    let db = config.graph_db_path();
    for suffix in ["", "-wal", "-shm"] {
        let _ = fs::remove_file(format!("{}{}", db.display(), suffix));
    }
}

fn spec(refs: &[&str], anchors: &[&str]) -> String {
    let mut s = String::from("---\ntitle: T\nsummary: S\nlast_updated: 2999-01-01\ngrounds_to:\n");
    for r in refs {
        s.push_str(&format!("  - {}\n", r));
    }
    s.push_str("---\n# Doc\n\nProse.\n\n");
    for a in anchors {
        s.push_str(&format!("<!-- kb-ground: {} -->\n", a));
    }
    s
}

fn grounding_codes(report: &knobyte::drift::DriftReport) -> Vec<(String, String)> {
    report
        .issues
        .iter()
        .filter(|i| i.code.starts_with("GROUNDING_"))
        .map(|i| (i.code.clone(), i.severity.clone()))
        .collect()
}

fn cli(root: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_knobyte"))
        .args(args)
        .current_dir(root)
        .env("KNOBYTE_NO_AGENT_LAUNCH", "1")
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
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

// ── 1. Committed baselines ──────────────────────────────────────────────────────────────────

#[test]
fn committed_baseline_survives_a_deleted_graph_db() {
    let (_d, config) = project();
    let root = &config.project_root;
    write(
        root,
        "src/calc.rs",
        "pub fn add_two(x: i32) -> i32 {\n    x + 2\n}\n",
    );
    rebuild(&config);
    let doc_path = root.join(".knobyte/context/calc.md");
    write(
        root,
        ".knobyte/context/calc.md",
        &spec(
            &["function:src/calc.rs:add_two"],
            &["function:src/calc.rs:add_two"],
        ),
    );
    assert_eq!(ground(&config), 1);

    // The baseline is in the markdown, in readable form.
    let text = fs::read_to_string(&doc_path).unwrap();
    assert!(
        text.contains("  - ref: function:src/calc.rs:add_two\n"),
        "{}",
        text
    );
    assert!(text.contains("    body_hash: "), "{}", text);
    assert!(text.contains("    fingerprint: mh1:"), "{}", text);
    assert!(
        text.contains("<!-- kb-ground: function:src/calc.rs:add_two #"),
        "{}",
        text
    );
    let refs = extract_doc_refs(&text);
    assert_eq!(refs.len(), 2);
    let fm = &refs[0];
    assert!(parse_fingerprint(fm.committed.fingerprint.as_deref().unwrap()).is_some());
    assert_eq!(refs[1].committed.body_hash, fm.committed.body_hash);
    // The wiki reads the same reference (hash suffix stripped, map form understood).
    let entity = parse_markdown_entity("context/calc.md", &text).unwrap();
    assert_eq!(
        entity.grounds_to,
        vec!["function:src/calc.rs:add_two".to_string()]
    );
    // Idempotent.
    ground(&config);
    assert_eq!(fs::read_to_string(&doc_path).unwrap(), text);

    // Change the body, throw the graph away (fresh clone / rebuild), rebuild, check.
    write(
        root,
        "src/calc.rs",
        "pub fn add_two(x: i32) -> i32 {\n    x * 3\n}\n",
    );
    delete_graph(&config);
    rebuild(&config);
    let report = run_drift_check(&config);
    assert_eq!(report.grounding.changed, 2, "{:?}", report.issues);
    assert_eq!(report.grounding.intact, 0);
    assert_eq!(
        grounding_codes(&report),
        vec![
            ("GROUNDING_DRIFT".to_string(), "warning".to_string()),
            ("GROUNDING_DRIFT".to_string(), "warning".to_string())
        ]
    );
}

#[test]
fn committed_fingerprint_reconciles_a_rename_without_the_cache() {
    let (_d, config) = project();
    let root = &config.project_root;
    write(root, "src/billing.rs", LARGE);
    rebuild(&config);
    write(
        root,
        ".knobyte/context/billing.md",
        &spec(&["function:src/billing.rs:settle_account"], &[]),
    );
    ground(&config);

    fs::remove_file(root.join("src/billing.rs")).unwrap();
    write(
        root,
        "src/accounts/settlement.rs",
        &LARGE.replace("settle_account", "close_account"),
    );
    delete_graph(&config);
    rebuild(&config);
    let res = sync_groundings(&config, false).unwrap();
    assert_eq!(res.relocated_count, 1, "{:?}", res);
    assert_eq!(
        res.proposals[0].new_node_id,
        "function:src/accounts/settlement.rs:close_account"
    );
    let text = fs::read_to_string(root.join(".knobyte/context/billing.md")).unwrap();
    assert!(text.contains("  - ref: function:src/accounts/settlement.rs:close_account\n"));
    // The committed baseline travelled with the reference.
    assert!(text.contains("    fingerprint: mh1:"));
}

#[test]
fn anchor_baseline_comes_from_other_files_and_conflicts_are_ambiguous() {
    let (_d, config) = project();
    let root = &config.project_root;
    write(root, "src/billing.rs", LARGE);
    write(
        root,
        "src/other.rs",
        "pub fn unrelated(a: &str) -> String { let mut s = String::new(); for c in a.chars() { if c.is_alphabetic() { s.push(c); } else { s.push('_'); } } s }\n",
    );
    rebuild(&config);
    let r = "function:src/billing.rs:settle_account";
    write(root, ".knobyte/context/a.md", &spec(&[r], &[]));
    ground(&config);
    let committed_a = fs::read_to_string(root.join(".knobyte/context/a.md")).unwrap();
    // An anchor-only document, never baselined itself.
    write(root, ".knobyte/context/b.md", &spec(&[], &[r]));

    // Move: the anchor takes its baseline from a.md's committed entry, so it reconciles.
    fs::remove_file(root.join("src/billing.rs")).unwrap();
    write(root, "src/moved.rs", LARGE);
    delete_graph(&config);
    rebuild(&config);
    let report = run_drift_check(&config);
    let b: Vec<_> = report
        .issues
        .iter()
        .filter(|i| i.file.ends_with("context/b.md") && i.code.starts_with("GROUNDING_"))
        .collect();
    assert_eq!(b.len(), 1, "{:?}", report.issues);
    assert_eq!(b[0].code, "GROUNDING_DRIFT");
    assert!(
        b[0].message.contains("Inline anchor should move"),
        "{}",
        b[0].message
    );

    // A second file commits a different fingerprint for the same reference: conflict.
    let other_fp = {
        let engine = GraphEngine::open(&config.graph_db_path()).unwrap();
        let n = engine.query_where_defined("unrelated").unwrap()[0].clone();
        knobyte::graph::grounding::current_baseline_values(engine.connection(), root, &n).1
    };
    let fp_a = extract_doc_refs(&committed_a)[0]
        .committed
        .fingerprint
        .clone()
        .unwrap();
    write(
        root,
        ".knobyte/context/c.md",
        &committed_a.replace(&fp_a, &other_fp),
    );
    let report = run_drift_check(&config);
    let b: Vec<_> = report
        .issues
        .iter()
        .filter(|i| i.file.ends_with("context/b.md") && i.code.starts_with("GROUNDING_"))
        .collect();
    assert_eq!(b.len(), 1, "{:?}", report.issues);
    assert_eq!(b[0].code, "GROUNDING_AMBIGUOUS");
    assert!(b[0].message.contains("conflicting committed fingerprints"));
}

#[test]
fn anchor_hash_suffix_and_committed_rewrite_preserve_other_content() {
    assert_eq!(
        split_anchor(" function:src/a.rs:f #abc123 "),
        (
            "function:src/a.rs:f".to_string(),
            Some("abc123".to_string())
        )
    );
    assert_eq!(
        split_anchor(" function:src/a.rs:f "),
        ("function:src/a.rs:f".to_string(), None)
    );

    let doc = "---\ntitle: T\ngrounds_to:\n  - function:src/a.rs:f\n  - node_id: function:src/b.rs:g\n    symbol: g\n  - function:src/c.rs:unresolved\ntags: [x]\n---\n# T\n\nBody\n<!-- kb-ground: function:src/a.rs:f -->\n";
    let mut updates = std::collections::BTreeMap::new();
    updates.insert(
        "function:src/a.rs:f".to_string(),
        ("h1".to_string(), "mh1:3:00".to_string()),
    );
    updates.insert(
        "function:src/b.rs:g".to_string(),
        ("h2".to_string(), "mh1:4:11".to_string()),
    );
    let out = apply_committed_baselines(doc, &updates);
    assert!(out.starts_with("---\ntitle: T\ngrounds_to:\n  - ref: function:src/a.rs:f\n    body_hash: h1\n    fingerprint: mh1:3:00\n"), "{}", out);
    assert!(
        out.contains("  - node_id: function:src/b.rs:g\n    symbol: g\n    body_hash: h2\n"),
        "{}",
        out
    );
    assert!(
        out.contains("  - function:src/c.rs:unresolved\ntags: [x]\n---\n# T\n\nBody\n"),
        "{}",
        out
    );
    assert!(
        out.ends_with("<!-- kb-ground: function:src/a.rs:f #h1 -->\n"),
        "{}",
        out
    );
    let refs = extract_doc_refs(&out);
    assert_eq!(refs.len(), 4);
    assert_eq!(refs[1].committed.body_hash.as_deref(), Some("h2"));
    assert_eq!(refs[3].origin, RefOrigin::Anchor(17));
}

// ── 2. `check` is read-only ─────────────────────────────────────────────────────────────────

#[test]
fn check_never_writes_baselines() {
    let (_d, config) = project();
    let root = &config.project_root;
    write(root, "src/a.rs", "pub fn one() -> u32 { 1 }\n");
    rebuild(&config);
    let text = spec(&["function:src/a.rs:one"], &["function:src/a.rs:one"]);
    write(root, ".knobyte/context/g.md", &text);

    for _ in 0..2 {
        let report = run_drift_check(&config);
        assert_eq!(report.grounding.unverified, 2);
        assert_eq!(
            grounding_codes(&report),
            vec![
                ("GROUNDING_UNVERIFIED".to_string(), "info".to_string()),
                ("GROUNDING_UNVERIFIED".to_string(), "info".to_string())
            ]
        );
        assert!(report.issues[0..]
            .iter()
            .any(|i| i.message.contains("graph ground --rebaseline")));
    }
    assert_eq!(
        fs::read_to_string(root.join(".knobyte/context/g.md")).unwrap(),
        text
    );
    let engine = GraphEngine::open(&config.graph_db_path()).unwrap();
    let rows: i64 = engine
        .connection()
        .query_row("SELECT COUNT(*) FROM _knobyte_grounded_source", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0);
}

// ── 3 + 4. Exit codes and `check --fix` ─────────────────────────────────────────────────────

#[test]
fn check_exits_non_zero_on_errors_and_fix_runs_the_repair_flow() {
    let (_d, config) = project();
    let root = &config.project_root;
    write(root, "src/a.rs", "pub fn one() -> u32 { 1 }\n");
    rebuild(&config);
    write(
        root,
        ".knobyte/context/g.md",
        &spec(&["function:src/a.rs:one"], &[]),
    );
    ground(&config);

    let (code, out) = cli(root, &["check"]);
    assert_eq!(code, 0, "{}", out);

    // An error: the grounded node disappears.
    write(root, "src/a.rs", "pub fn other() -> u32 { 1 }\n");
    rebuild(&config);
    for args in [&["check"][..], &["check", "--quiet"], &["check", "--json"]] {
        let (code, out) = cli(root, args);
        assert_eq!(code, 1, "{:?}: {}", args, out);
    }
    let (_, json) = cli(root, &["check", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["issues"][0]["code"], "GROUNDING_GONE");

    // --fix: errors remain after relocation, so the sync flow runs; non-interactive, it prints
    // the repair brief instead of launching an agent.
    let (code, out) = cli(root, &["check", "--fix"]);
    assert_eq!(code, 1, "{}", out);
    assert!(out.contains("COPY BELOW THIS LINE"), "{}", out);
    assert!(out.contains("function:src/a.rs:one"), "{}", out);
}

/// `check --fix` prints the exact planned rewrites and writes only with consent: `--dry-run`
/// previews, a non-interactive run (or `--json`) without `--yes` refuses (exit 5), `--yes`
/// applies.
#[test]
fn check_fix_plans_and_needs_consent_to_write() {
    let (_d, config) = project();
    let root = &config.project_root;
    let body = "pub fn calculate_tax(amount: f64) -> f64 {\n    amount * 0.2\n}\n";
    write(root, "src/old.rs", body);
    rebuild(&config);
    let doc = root.join(".knobyte/context/t.md");
    write(root, ".knobyte/context/t.md", &spec(&["function:src/old.rs:calculate_tax"], &[]));
    ground(&config);
    fs::remove_file(root.join("src/old.rs")).unwrap();
    write(root, "src/new.rs", body);
    rebuild(&config);
    let before = fs::read_to_string(&doc).unwrap();
    let planned = "function:src/old.rs:calculate_tax -> function:src/new.rs:calculate_tax";

    let (code, out) = cli(root, &["check", "--fix", "--dry-run"]);
    assert_eq!(code, 0, "{}", out);
    assert!(out.contains("Planned grounding changes (dry run, nothing is written) (1):"), "{}", out);
    assert!(out.contains("context/t.md") && out.contains(planned), "{}", out);
    assert_eq!(fs::read_to_string(&doc).unwrap(), before);

    let (code, out) = cli(root, &["check", "--fix"]);
    assert_eq!(code, 5, "{}", out);
    assert!(out.contains(planned) && out.contains("--yes"), "{}", out);
    assert_eq!(fs::read_to_string(&doc).unwrap(), before);

    let (code, out) = cli(root, &["check", "--fix", "--json"]);
    assert_eq!(code, 5, "{}", out);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["fix"]["applied"], false);
    assert_eq!(v["fix"]["planned"][0]["file"], "context/t.md");
    assert_eq!(v["fix"]["planned"][0]["oldRef"], "function:src/old.rs:calculate_tax");
    assert_eq!(v["fix"]["planned"][0]["newRef"], "function:src/new.rs:calculate_tax");
    assert_eq!(fs::read_to_string(&doc).unwrap(), before);

    let (code, out) = cli(root, &["check", "--dry-run"]);
    assert_eq!(code, 2, "--dry-run requires --fix: {}", out);

    let (code, out) = cli(root, &["check", "--fix", "--yes"]);
    assert!(out.contains("Relocated 1 grounding anchor(s)"), "{}", out);
    assert_eq!(code, 0, "{}", out);
    let after = fs::read_to_string(&doc).unwrap();
    assert!(after.contains("function:src/new.rs:calculate_tax") && !after.contains("src/old.rs"), "{}", after);

    // Nothing left to plan: plain check is clean and read-only.
    let (code, out) = cli(root, &["check", "--fix", "--json"]);
    assert_eq!(code, 0, "{}", out);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["fix"]["planned"], serde_json::json!([]));
}

// ── 5. Edited files are re-extracted, not blanket-unverified ────────────────────────────────

#[test]
fn stale_graph_judges_edited_files_exactly() {
    let (_d, config) = project();
    let root = &config.project_root;
    write(
        root,
        "src/a.rs",
        "pub fn one() -> u32 {\n    1\n}\n\npub fn two() -> u32 {\n    2\n}\n",
    );
    rebuild(&config);
    write(
        root,
        ".knobyte/context/g.md",
        &spec(&["function:src/a.rs:one", "function:src/a.rs:two"], &[]),
    );
    ground(&config);

    // Edit `one` only; no refresh.
    write(
        root,
        "src/a.rs",
        "pub fn one() -> u32 {\n    100\n}\n\npub fn two() -> u32 {\n    2\n}\n",
    );
    let report = run_drift_check(&config);
    assert_eq!(report.graph.as_ref().unwrap().status, GraphState::Stale);
    assert_eq!(report.grounding.changed, 1, "{:?}", report.issues);
    assert_eq!(report.grounding.intact, 1);
    assert_eq!(report.grounding.unverified, 0);
    let drift: Vec<_> = report
        .issues
        .iter()
        .filter(|i| i.code == "GROUNDING_DRIFT")
        .collect();
    assert_eq!(drift.len(), 1);
    assert_eq!(drift[0].symbol.as_deref(), Some("function:src/a.rs:one"));

    // Deleting the file cannot be settled without a refresh.
    fs::remove_file(root.join("src/a.rs")).unwrap();
    let report = run_drift_check(&config);
    assert_eq!(report.grounding.unverified, 2, "{:?}", report.issues);
}

// ── 6. Freshness consistent with `graph status`; no reconciling against stale snapshots ──────

fn set_metadata(config: &KnobyteConfig, key: &str, value: &str) {
    let conn = rusqlite::Connection::open(config.graph_db_path()).unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO project_metadata (key, value, updated_at) VALUES (?1, ?2, 0)",
        rusqlite::params![key, value],
    )
    .unwrap();
}

#[test]
fn freshness_detects_extractor_rebuild_marker_and_corruption() {
    let (_d, config) = project();
    let root = &config.project_root;
    write(root, "src/a.rs", "pub fn one() -> u32 { 1 }\n");
    rebuild(&config);
    write(
        root,
        ".knobyte/context/g.md",
        &spec(&["function:src/a.rs:one"], &[]),
    );
    ground(&config);
    assert_eq!(run_drift_check(&config).grounding.intact, 1);

    set_metadata(&config, "extractor_version", "0-old");
    let report = run_drift_check(&config);
    let graph = report.graph.as_ref().unwrap();
    assert_eq!(graph.status, GraphState::Stale);
    assert!(graph.whole_graph.is_some());
    assert_eq!(report.grounding.unverified, 1);
    assert_eq!(
        grounding_codes(&report),
        vec![("GROUNDING_UNVERIFIED".to_string(), "warning".to_string())]
    );

    rebuild(&config);
    set_metadata(&config, "rebuild_required", "1");
    let report = run_drift_check(&config);
    assert_eq!(
        report.graph.as_ref().unwrap().status,
        GraphState::RebuildRequired
    );
    assert_eq!(report.grounding.unverified, 1);
    assert_eq!(
        knobyte::drift::inspect_graph(&config).status,
        GraphState::RebuildRequired
    );

    delete_graph(&config);
    fs::write(
        config.graph_db_path(),
        b"this is not a sqlite database at all",
    )
    .unwrap();
    assert_eq!(
        knobyte::drift::inspect_graph(&config).status,
        GraphState::Corrupt
    );
}

#[test]
fn relocation_refuses_a_stale_graph_and_sync_does_not_claim_clean() {
    let (_d, config) = project();
    let root = &config.project_root;
    let body = "pub fn calculate_tax(amount: f64) -> f64 {\n    amount * 0.2\n}\n";
    write(root, "src/old.rs", body);
    rebuild(&config);
    write(
        root,
        ".knobyte/context/t.md",
        &spec(&["function:src/old.rs:calculate_tax"], &[]),
    );
    ground(&config);
    fs::remove_file(root.join("src/old.rs")).unwrap();
    write(root, "src/new.rs", body);
    rebuild(&config);
    // Fresh: a proposal. Then an unrelated edit makes the graph stale: none.
    assert_eq!(find_grounding_relocations(&config).unwrap().len(), 1);
    write(root, "src/unrelated.rs", "pub fn z() {}\n");
    assert!(find_grounding_relocations(&config).unwrap().is_empty());
    let res = sync_groundings(&config, false).unwrap();
    assert_eq!(res.relocated_count, 0);
    assert!(res.skipped.is_some());

    // No graph at all: the grounding cannot be checked, so sync must not report "no drift".
    delete_graph(&config);
    let (_, out) = cli(root, &["sync", "--dry-run"]);
    assert!(!out.contains("No drift detected"), "{}", out);
    assert!(out.contains("could not be verified"), "{}", out);
}

// ── 7. Wording and shapes ───────────────────────────────────────────────────────────────────

#[test]
fn committed_maps_are_not_a_mixed_shape() {
    let doc = "---\ngrounds_to:\n  - function:src/a.rs:f\n  - ref: function:src/b.rs:g\n    body_hash: h\n---\n# T\n";
    assert!(check_grounding_shape_in(doc, "x.md").is_empty());
    let mixed =
        "---\ngrounds_to:\n  - function:src/a.rs:f\n  - node_id: function:src/b.rs:g\n---\n# T\n";
    assert_eq!(
        check_grounding_shape_in(mixed, "x.md")[0].code,
        "GROUNDING_MIXED_SHAPE"
    );
}

// ── 8. Scaffold walk ────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
fn root_layout_scaffold_skips_build_trees_and_follows_symlinks() {
    let dir = tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write(&root, "Cargo.toml", "[package]\nname = \"x\"\n");
    write(&root, "ROUTER.md", "# Router\n");
    write(&root, "context/a.md", "# A\n");
    write(&root, "node_modules/pkg/README.md", "# dep\n");
    write(&root, "target/doc/x.md", "# built\n");
    write(&root, ".git/info.md", "# vcs\n");
    write(&root, "docs/nested/.cache/y.md", "# hidden\n");
    write(&root, "shared/linked.md", "# Linked\n");
    std::os::unix::fs::symlink(root.join("shared/linked.md"), root.join("context/link.md"))
        .unwrap();
    std::os::unix::fs::symlink(root.join("context"), root.join("context_alias")).unwrap();

    let files: Vec<String> = scaffold_markdown_files(&root)
        .into_iter()
        .map(|(r, _)| r)
        .collect();
    assert!(files.contains(&"ROUTER.md".to_string()), "{:?}", files);
    assert!(files.contains(&"context/a.md".to_string()), "{:?}", files);
    assert!(
        files.contains(&"context/link.md".to_string())
            || files.contains(&"shared/linked.md".to_string()),
        "{:?}",
        files
    );
    assert!(files.iter().all(|f| !f.starts_with("node_modules")
        && !f.starts_with("target")
        && !f.starts_with(".git")
        && !f.contains(".cache")));
    // A file reached through two paths is listed once.
    assert_eq!(
        files.iter().filter(|f| f.ends_with("a.md")).count(),
        1,
        "{:?}",
        files
    );

    // The reported file count matches the verbose "scanned" line.
    let config = KnobyteConfig::new(root.clone(), root.clone());
    let report = run_drift_check_with(
        &config,
        &DriftCheckOptions {
            verbose: true,
            ..Default::default()
        },
    );
    let log = report.verbose_log.unwrap();
    assert_eq!(
        log[0],
        format!("Scaffold files scanned: {}", report.file_count)
    );
}
