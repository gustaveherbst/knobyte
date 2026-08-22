//! `graph ground`: proposals for ungrounded wiki entities (--dry-run / --apply), agent prompt,
//! and the re-baseline default; plus CLI smoke tests of the protocol v3 commands.

use knobyte::graph::ground::{apply_groundings, ground_prompt, propose_groundings};
use knobyte::graph::GraphEngine;
use std::fs;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn fixture(root: &Path) {
    write(
        root,
        "src/tax.rs",
        "/// Sales tax in cents.\npub fn compute_sales_tax(amount_cents: i64) -> i64 {\n    amount_cents * 8 / 100\n}\n",
    );
    write(
        root,
        "src/invoice.rs",
        "use crate::tax::compute_sales_tax;\npub fn finalize_invoice(amount: i64) -> i64 {\n    amount + compute_sales_tax(amount)\n}\n",
    );
    write(root, "src/logging.rs", "pub fn log_line(s: &str) { println!(\"{}\", s); }\n");
    write(
        root,
        ".knobyte/specs/tax.md",
        "---\nid: kb_tax\ntitle: Sales tax\ntype: spec\n---\n# Sales tax\n\nThe `compute_sales_tax` function applies an 8 percent sales tax to invoice amounts.\n",
    );
    write(
        root,
        ".knobyte/specs/grounded.md",
        "---\nid: kb_grounded\ntitle: Invoices\ntype: spec\ngrounds_to:\n  - function:src/invoice.rs:finalize_invoice\n---\n# Invoices\n",
    );
    write(root, ".knobyte/AGENTS.md", "---\nid: kb_agents\ntitle: Agents\n---\n# Agents\nTax and invoices.\n");
}

#[test]
fn proposes_and_applies_groundings_for_ungrounded_entities() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let mut engine = GraphEngine::open(&root.join(".knobyte/graph.db")).unwrap();
    engine.rebuild(root).unwrap();
    let scaffold = root.join(".knobyte");

    let proposals = propose_groundings(engine.connection(), &scaffold, 3);
    // Only the ungrounded, non-broad document gets a proposal.
    assert_eq!(proposals.len(), 1, "{:#?}", proposals);
    let p = &proposals[0];
    assert_eq!(p.doc, "specs/tax.md");
    assert_eq!(p.refs[0].reference, "function:src/tax.rs:compute_sales_tax", "{:#?}", p.refs);
    assert!(p.refs.iter().all(|r| r.kind != "file" && r.kind != "parameter"));

    // Dry run writes nothing; apply writes frontmatter and is idempotent.
    let before = fs::read_to_string(scaffold.join("specs/tax.md")).unwrap();
    assert!(!before.contains("grounds_to"));
    assert_eq!(apply_groundings(&scaffold, &proposals).unwrap(), 1);
    let after = fs::read_to_string(scaffold.join("specs/tax.md")).unwrap();
    assert!(after.contains("grounds_to:\n  - \"function:src/tax.rs:compute_sales_tax\""), "{}", after);
    assert!(after.ends_with("applies an 8 percent sales tax to invoice amounts.\n"), "prose preserved");
    assert_eq!(apply_groundings(&scaffold, &proposals).unwrap(), 0);
    assert!(propose_groundings(engine.connection(), &scaffold, 3).is_empty());

    let prompt = ground_prompt(".knobyte");
    assert!(prompt.contains("READ BROAD, GROUND TIGHT"));
    assert!(prompt.contains("knobyte graph scope"));
}

fn run(root: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_knobyte"))
        .args(args)
        .current_dir(root)
        .env("KNOBYTE_NO_AGENT_LAUNCH", "1")
        .output()
        .unwrap();
    (
        out.status.success(),
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
    )
}

#[test]
fn cli_ground_modes_and_protocol_commands() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let mut engine = GraphEngine::open(&root.join(".knobyte/graph.db")).unwrap();
    engine.rebuild(root).unwrap();
    drop(engine);

    let (ok, out) = run(root, &["graph", "ground", "--dry-run", "--json"]);
    assert!(ok, "{}", out);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["dryRun"], true);
    assert_eq!(v["proposals"][0]["doc"], "specs/tax.md");
    assert!(!fs::read_to_string(root.join(".knobyte/specs/tax.md")).unwrap().contains("grounds_to"));

    let (ok, out) = run(root, &["graph", "ground", "--agent", "--dry-run"]);
    assert!(ok && out.contains("retro-grounding"), "{}", out);

    let (ok, out) = run(root, &["graph", "ground", "--apply", "--json"]);
    assert!(ok, "{}", out);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["documentsChanged"], 1);
    let baselined = v["baselined"].as_u64().unwrap();
    let proposed = v["proposals"][0]["refs"].as_array().unwrap().len() as u64;
    assert_eq!(baselined, proposed + 1, "proposed refs plus the existing grounding");

    // Default mode keeps the re-baseline behaviour.
    let (ok, out) = run(root, &["graph", "ground"]);
    assert!(ok && out.contains(&format!("Re-baselined {} grounded references", baselined)), "{}", out);
    let (ok, out) = run(root, &["graph", "ground", "--rebaseline", "--json"]);
    assert!(ok, "{}", out);
    assert!(out.contains(&format!("\"baselined\":{}", baselined)), "{}", out);

    // Protocol v3 over the CLI.
    let (ok, out) = run(root, &["graph", "scope", "sales", "tax", "--jsonl", "--max-output-tokens", "2500"]);
    assert!(ok, "{}", out);
    let lines: Vec<serde_json::Value> = out.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines[0]["type"], "meta");
    assert_eq!(lines[0]["maxOutputTokens"], 2500);
    assert_eq!(lines.last().unwrap()["type"], "summary");
    let (ok, out) = run(root, &["graph", "scope", "sales", "tax", "--jsonl", "--wiki"]);
    assert!(ok, "{}", out);
    let (ok, out) = run(root, &["graph", "query", "who-calls", "compute_sales_tax", "--detail", "source"]);
    assert!(ok, "{}", out);
    assert!(out.contains("\"type\":\"result\"") && out.contains("\"type\":\"source\""), "{}", out);
    let (ok, out) = run(root, &["impact", "compute_sales_tax", "--jsonl", "--max-nodes", "1"]);
    assert!(ok, "{}", out);
    assert!(out.contains("\"truncated\":true"), "{}", out);
    let (ok, out) = run(root, &["graph", "get", "function:src/tax.rs:compute_sales_tax", "--jsonl"]);
    assert!(ok && out.contains("compute_sales_tax(amount_cents"), "{}", out);
    let (ok, out) = run(root, &["graph", "scope", "tax", "--detail", "verbose"]);
    assert!(!ok && out.contains("Unknown --detail"), "{}", out);
}
