//! `graph scope` source admission on a real index: answers are admitted as complete atomic
//! declarations, every source line is serialized at most once, and the hard output budget
//! holds at every ceiling.

use knobyte::graph::agent::{run_scope, ScopeExtras};
use knobyte::graph::protocol::{estimate_tokens, AgentOptionsInput};
use knobyte::graph::GraphEngine;
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use tempfile::tempdir;

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

/// A long billing module (well over the 200-line whole-file limit) whose answer sits late in
/// the file, a tax module it calls, and unrelated helpers.
fn fixture(root: &Path) -> GraphEngine {
    let mut billing = String::from("import { computeSalesTax } from './tax';\n\n");
    for i in 0..34 {
        billing.push_str(&format!(
            "export function formatReceiptLine{i}(label: string): string {{\n  const padded = label.padEnd({w});\n  const upper = padded.toUpperCase();\n  const trimmed = upper.trim();\n  return trimmed + '{i}';\n}}\n\n",
            i = i,
            w = 10 + i
        ));
    }
    billing.push_str(
        "/** Finalize an invoice total: add the sales tax to the subtotal. */\nexport function finalizeInvoiceTotal(subtotalCents: number): number {\n  const tax = computeSalesTax(subtotalCents);\n  const total = subtotalCents + tax;\n  return total;\n}\n",
    );
    write(root, "src/billing.ts", &billing);
    write(
        root,
        "src/tax.ts",
        "/** Sales tax in cents (8 percent). */\nexport function computeSalesTax(amountCents: number): number {\n  return Math.floor(amountCents * 8 / 100);\n}\n",
    );
    write(root, "src/logger.ts", "export function logLine(message: string): void {\n  console.log(message);\n}\n");
    let mut engine = GraphEngine::open(&root.join(".knobyte").join("graph.db")).unwrap();
    engine.rebuild(root).unwrap();
    engine
}

fn sources(records: &[Value]) -> Vec<&Value> {
    records.iter().filter(|r| r["type"] == "source").collect()
}

/// (file, line) pairs serialized by source records; panics on a duplicate.
fn assert_lines_once(records: &[Value]) {
    let mut seen: HashSet<(String, i64)> = HashSet::new();
    for r in sources(records) {
        let file = r["filePath"].as_str().unwrap().to_string();
        for range in r["ranges"].as_array().unwrap() {
            let (s, e) = (range["startLine"].as_i64().unwrap(), range["endLine"].as_i64().unwrap());
            for l in s..=e {
                assert!(seen.insert((file.clone(), l)), "line {}:{} serialized twice", file, l);
            }
        }
    }
}

fn summary(records: &[Value]) -> &Value {
    records.last().unwrap()
}

#[test]
fn scope_admits_the_answer_declaration_whole_and_each_line_once() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let engine = fixture(root);
    let records = run_scope(
        &engine,
        root,
        "finalize invoice total sales tax",
        &AgentOptionsInput::default(),
        &ScopeExtras::default(),
    );
    assert_lines_once(&records);
    let billing = sources(&records)
        .into_iter()
        .filter(|r| r["filePath"] == "src/billing.ts")
        .flat_map(|r| r["ranges"].as_array().unwrap().clone())
        .collect::<Vec<_>>();
    let answer = billing
        .iter()
        .find(|r| r["content"].as_str().unwrap().contains("export function finalizeInvoiceTotal"))
        .expect("answer declaration is sourced");
    assert_eq!(answer["reason"], "complete-symbol");
    assert_eq!(answer["truncated"], false);
    assert!(answer["content"].as_str().unwrap().contains("return total;"));
    // The callee's file is part of the answer (flow or source).
    let s = summary(&records);
    assert!(s["returnedFiles"].as_array().unwrap().iter().any(|f| f == "src/tax.ts"), "{}", s);
    assert_eq!(s["status"], "ok", "{}", s);
}

#[test]
fn scope_keeps_answers_atomic_and_the_ceiling_hard_under_pressure() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    let engine = fixture(root);
    for max in [900usize, 1200, 1600, 2400, 4000] {
        let input = AgentOptionsInput {
            max_output_tokens: Some(max),
            ..Default::default()
        };
        let records = run_scope(&engine, root, "finalize invoice total sales tax", &input, &ScopeExtras::default());
        let total: usize = records.iter().map(estimate_tokens).sum();
        assert!(total <= max, "{} tokens > {}", total, max);
        assert_lines_once(&records);
        for r in sources(&records) {
            for range in r["ranges"].as_array().unwrap() {
                if range["reason"] == "complete-symbol" && range["truncated"] == false {
                    // A complete symbol is never a prefix: it ends on its closing brace.
                    let last = range["content"].as_str().unwrap().lines().last().unwrap().to_string();
                    assert!(last.trim_end().ends_with('}'), "{}", last);
                }
            }
        }
        // Determinism.
        let again = run_scope(&engine, root, "finalize invoice total sales tax", &input, &ScopeExtras::default());
        assert_eq!(records, again);
    }
}
