//! Agent protocol v3: framing, budget truncation determinism, scope ranking goldens, get /
//! query / impact records.

use knobyte::graph::agent::{run_get, run_impact, run_query, run_scope, ScopeExtras};
use knobyte::graph::protocol::{estimate_tokens, AgentOptionsInput, DetailLevel};
use knobyte::graph::{GraphEngine, ImpactOptions};
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// A small billing service: an invoice pipeline across files, an unrelated logger, and tests.
fn fixture(root: &Path) -> GraphEngine {
    write(
        root,
        "src/invoice.rs",
        r#"use crate::tax::compute_tax;
use crate::ledger::record_entry;

/// An invoice for a customer.
pub struct Invoice {
    pub customer: String,
    pub amount_cents: i64,
}

/// Finalize an invoice: compute tax, then record it in the ledger.
pub fn finalize_invoice(invoice: &Invoice) -> i64 {
    let tax = compute_tax(invoice.amount_cents);
    let total = invoice.amount_cents + tax;
    record_entry(&invoice.customer, total);
    total
}
"#,
    );
    write(
        root,
        "src/tax.rs",
        r#"/// Sales tax in cents for an amount (8 percent, rounded down).
pub fn compute_tax(amount_cents: i64) -> i64 {
    amount_cents * 8 / 100
}
"#,
    );
    write(
        root,
        "src/ledger.rs",
        r#"/// Append a ledger entry for a customer.
pub fn record_entry(customer: &str, total_cents: i64) {
    let line = format!("{} {}", customer, total_cents);
    persist_line(&line);
}

fn persist_line(line: &str) {
    let _ = line.len();
}
"#,
    );
    write(
        root,
        "src/logging.rs",
        "/// Print a message.\npub fn log_message(msg: &str) {\n    println!(\"{}\", msg);\n}\n",
    );
    write(
        root,
        "tests/invoice_test.rs",
        "use app::invoice::finalize_invoice;\n\nfn finalizes_with_tax() {\n    let inv = app::invoice::Invoice { customer: \"a\".into(), amount_cents: 100 };\n    assert_eq!(finalize_invoice(&inv), 108);\n}\n",
    );
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    engine.rebuild(root).unwrap();
    engine
}

fn of_type<'a>(records: &'a [Value], t: &str) -> Vec<&'a Value> {
    records.iter().filter(|r| r["type"] == t).collect()
}

fn check_framing(records: &[Value]) {
    assert_eq!(records.first().unwrap()["type"], "meta");
    assert_eq!(records.first().unwrap()["protocolVersion"], 3);
    let summary = records.last().unwrap();
    assert_eq!(summary["type"], "summary");
    let total: usize = records.iter().map(estimate_tokens).sum();
    assert_eq!(summary["estimatedOutputTokens"].as_u64().unwrap() as usize, total, "honest estimate");
    assert!(total <= summary["maxOutputTokens"].as_u64().unwrap() as usize, "hard ceiling");
}

#[test]
fn scope_ranks_the_invoice_pipeline_with_flow_source_and_tests() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let engine = fixture(root);
    let records = run_scope(
        &engine,
        root,
        "how is the tax added when an invoice is finalized",
        &AgentOptionsInput::default(),
        &ScopeExtras::default(),
    );
    check_framing(&records);
    let summary = records.last().unwrap();
    assert_eq!(summary["status"], "ok", "{:#?}", summary);
    assert!(summary["evidenceStrength"] == "strong" || summary["evidenceStrength"] == "moderate");
    // Golden: the pipeline files lead, the unrelated logger is not returned.
    let files: Vec<&str> = summary["returnedFiles"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(files[0], "src/invoice.rs", "{:?}", files);
    assert!(files.contains(&"src/tax.rs"), "{:?}", files);
    assert!(!files.contains(&"src/logging.rs"));
    // A directed flow finalize_invoice -> compute_tax.
    let flows = of_type(&records, "flow");
    assert!(
        flows.iter().any(|f| f["steps"].as_array().unwrap().iter().any(|s| {
            s["kind"] == "calls" && s["source"].as_str().unwrap().starts_with("function:")
        })),
        "{:#?}",
        flows
    );
    let names: Vec<String> = flows
        .iter()
        .flat_map(|f| f["nodes"].as_array().unwrap().iter().map(|n| n["name"].as_str().unwrap().to_string()))
        .collect();
    assert!(names.contains(&"finalize_invoice".to_string()) && names.contains(&"compute_tax".to_string()), "{:?}", names);
    // Source is numbered and graph-backed.
    let sources = of_type(&records, "source");
    assert!(sources.iter().all(|s| s["evidence"] == "graph"));
    let content = sources[0]["ranges"][0]["content"].as_str().unwrap();
    assert!(content.starts_with(" 1: ") || content.starts_with("1: "), "{}", content);
    // Facts carry readable references.
    let facts = of_type(&records, "fact");
    assert!(facts.iter().any(|f| f["ref"] == "function:src/invoice.rs:finalize_invoice"), "{:#?}", facts);
    // Test neighbour: the test calling finalize_invoice is surfaced as a test fact.
    assert!(
        facts.iter().any(|f| f["category"] == "test" && f["name"] == "finalizes_with_tax"),
        "{:#?}",
        facts
    );
    // Deterministic: the same request yields byte-identical output.
    let again = run_scope(
        &engine,
        root,
        "how is the tax added when an invoice is finalized",
        &AgentOptionsInput::default(),
        &ScopeExtras::default(),
    );
    assert_eq!(records, again);
}

#[test]
fn budget_truncation_is_deterministic_and_reported() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let engine = fixture(root);
    let input = AgentOptionsInput {
        max_output_tokens: Some(700),
        ..Default::default()
    };
    let a = run_scope(&engine, root, "finalize invoice tax ledger entry", &input, &ScopeExtras::default());
    let b = run_scope(&engine, root, "finalize invoice tax ledger entry", &input, &ScopeExtras::default());
    assert_eq!(a, b, "same budget, same bytes");
    check_framing(&a);
    let summary = a.last().unwrap();
    assert_eq!(summary["truncated"], true, "{:#?}", summary);
    // A truncated source range is cut at whole lines and marked.
    for s in of_type(&a, "source") {
        for r in s["ranges"].as_array().unwrap() {
            let lines = r["content"].as_str().unwrap().lines().count() as i64;
            assert_eq!(r["endLine"].as_i64().unwrap() - r["startLine"].as_i64().unwrap() + 1, lines);
        }
    }

    // Below mandatory framing: rejected, never silently raised.
    let tiny = run_scope(
        &engine,
        root,
        "invoice",
        &AgentOptionsInput {
            max_output_tokens: Some(50),
            ..Default::default()
        },
        &ScopeExtras::default(),
    );
    assert_eq!(tiny.len(), 1);
    assert_eq!(tiny[0]["code"], "INVALID_OUTPUT_BUDGET");

    // Minimal detail: no source, facts only.
    let minimal = run_scope(
        &engine,
        root,
        "finalize invoice",
        &AgentOptionsInput {
            detail: Some(DetailLevel::Minimal),
            max_nodes: Some(2),
            ..Default::default()
        },
        &ScopeExtras::default(),
    );
    check_framing(&minimal);
    assert!(of_type(&minimal, "source").is_empty());
    let facts: Vec<_> = of_type(&minimal, "fact").into_iter().filter(|f| f["category"] != "test").collect();
    assert!(!facts.is_empty() && facts.len() <= 2, "{:#?}", facts);
}

#[test]
fn scope_hybrid_channel_and_knowledge_provider() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let engine = fixture(root);
    let ledger_id = engine.query_where_defined("record_entry").unwrap()[0].id.clone();
    let knowledge = |ids: &[String]| -> Vec<Value> {
        vec![serde_json::json!({ "type": "knowledge", "title": "Ledger", "matchedNodes": ids.len() })]
    };
    let extras = ScopeExtras {
        vector_hits: vec![(ledger_id.clone(), 0.9)],
        warnings: Vec::new(),
        knowledge_for: Some(&knowledge),
    };
    let records = run_scope(
        &engine,
        root,
        "append bookkeeping rows",
        &AgentOptionsInput {
            detail: Some(DetailLevel::Standard),
            ..Default::default()
        },
        &extras,
    );
    check_framing(&records);
    let facts = of_type(&records, "fact");
    let ledger = facts.iter().find(|f| f["id"] == ledger_id.as_str()).expect("vector hit returned");
    assert!(ledger["selectionReasons"].as_array().unwrap().iter().any(|r| r == "vector"));
    assert_eq!(of_type(&records, "knowledge").len(), 1);
    // Knowledge is appended after graph records, before the summary.
    let pos = records.iter().position(|r| r["type"] == "knowledge").unwrap();
    assert_eq!(pos, records.len() - 2);
}

#[test]
fn get_query_and_impact_speak_protocol_v3() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let engine = fixture(root);
    let id = engine.query_where_defined("finalize_invoice").unwrap()[0].id.clone();

    // get: grouped source, unknown ids reported.
    let got = run_get(&engine, root, &[id.clone(), "function:nope".into()], &AgentOptionsInput::default());
    check_framing(&got);
    assert_eq!(of_type(&got, "error")[0]["code"], "NODE_NOT_FOUND");
    let src = of_type(&got, "source");
    assert_eq!(src[0]["ranges"][0]["reason"], "complete-symbol");
    assert!(src[0]["ranges"][0]["content"].as_str().unwrap().contains("compute_tax"));
    // A readable grounding reference works as an id.
    let by_ref = run_get(&engine, root, &["function:src/tax.rs:compute_tax".into()], &AgentOptionsInput::default());
    assert_eq!(of_type(&by_ref, "source").len(), 1);

    // get under a budget too small for the body: fact kept, prefix spilled, retry suggested.
    let floor = run_get(
        &engine,
        root,
        std::slice::from_ref(&id),
        &AgentOptionsInput {
            max_output_tokens: Some(1),
            ..Default::default()
        },
    );
    assert_eq!(floor[0]["code"], "INVALID_OUTPUT_BUDGET");
    let minimum = floor[0]["minimum"].as_u64().unwrap() as usize;
    let squeezed = run_get(
        &engine,
        root,
        std::slice::from_ref(&id),
        &AgentOptionsInput {
            max_output_tokens: Some(minimum + 100),
            ..Default::default()
        },
    );
    check_framing(&squeezed);
    let summary = squeezed.last().unwrap();
    assert_eq!(summary["status"], "partial", "{:#?}", squeezed);
    assert!(summary["suggestedNextCommands"][0].as_str().unwrap().contains("--max-output-tokens"));
    assert_eq!(of_type(&squeezed, "fact").len(), 1);

    // query: callers of compute_tax.
    let q = run_query(&engine, root, "who-calls", "compute_tax", &AgentOptionsInput::default());
    check_framing(&q);
    let results = of_type(&q, "result");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["name"], "finalize_invoice");
    assert_eq!(results[0]["relation"], "who-calls");
    let missing = run_query(&engine, root, "who-calls", "does_not_exist", &AgentOptionsInput::default());
    assert_eq!(missing[0]["code"], "TARGET_NOT_FOUND");
    let invalid = run_query(&engine, root, "sideways", "x", &AgentOptionsInput::default());
    assert_eq!(invalid[0]["code"], "INVALID_QUERY");

    // impact: defines + caller records with depth, budgeted.
    write(
        root,
        ".knobyte/specs/tax.md",
        "---\nid: kb_tax\ntitle: Tax\ntype: spec\ngrounds_to:\n  - function:src/tax.rs:compute_tax\n---\n# Tax\n",
    );
    let imp = run_impact(
        &engine,
        root,
        &root.join(".knobyte"),
        "compute_tax",
        ImpactOptions { depth: 3, callers_only: true },
        &AgentOptionsInput {
            detail: Some(DetailLevel::Source),
            ..Default::default()
        },
    );
    check_framing(&imp);
    assert_eq!(of_type(&imp, "target")[0]["value"], "compute_tax");
    assert_eq!(of_type(&imp, "defines")[0]["name"], "compute_tax");
    let callers: Vec<&str> = of_type(&imp, "caller").iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert!(callers.contains(&"finalize_invoice"), "{:?}", callers);
    assert_eq!(of_type(&imp, "grounding")[0]["doc"], "specs/tax.md");
    assert!(!of_type(&imp, "source").is_empty());
}
