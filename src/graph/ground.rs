//! Retro-grounding: propose `grounds_to` references for scaffold (wiki) entities that have
//! none, from the code graph's scope ranking, and optionally write them into the documents'
//! frontmatter. The agent-led mode hands the same task to a coding agent with a
//! prose-preserving prompt; deterministic proposals are a starting point it can verify.

use rusqlite::Connection;
use serde::Serialize;
use std::path::Path;

use crate::graph::grounding::{readable_ref_for, scaffold_markdown_files};
use crate::graph::scope::{select_scope, ScopeRequest};
use crate::wiki::parser::parse_markdown_entity;

/// Node kinds worth grounding a document to (behaviour-bearing declarations).
const GROUNDABLE_KINDS: [&str; 9] = [
    "function", "method", "class", "struct", "trait", "interface", "enum", "route", "type_alias",
];
/// Root scaffold documents that are intentionally broad and stay ungrounded.
const BROAD_DOCS: [&str; 6] = ["AGENTS.md", "ROUTER.md", "SETUP.md", "SYNC.md", "README.md", "CLAUDE.md"];

#[derive(Debug, Clone, Serialize)]
pub struct ProposedRef {
    /// Readable grounding reference (`kind:path:qualified_name`).
    pub reference: String,
    pub node_id: String,
    pub kind: String,
    pub qualified_name: String,
    pub file_path: String,
    pub score: f64,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroundProposal {
    /// Scaffold-relative document path.
    pub doc: String,
    pub entity_id: String,
    pub title: String,
    pub refs: Vec<ProposedRef>,
}

/// Corroborated evidence: an exact identifier, or a declaration-name hit backed by a second
/// independent channel (another term, source text or a vector match).
fn strong_enough(reasons: &[String]) -> bool {
    if reasons.iter().any(|r| r.starts_with("exact:")) {
        return true;
    }
    let terms = reasons.iter().filter(|r| r.starts_with("term:")).count();
    let other = reasons
        .iter()
        .any(|r| r == "source-region" || r == "vector" || r == "bm25-node");
    terms >= 2 || (terms >= 1 && other)
}

/// Proposals for every ungrounded scaffold entity (at most `per_entity` references each).
pub fn propose_groundings(conn: &Connection, scaffold_root: &Path, per_entity: usize) -> Vec<GroundProposal> {
    let mut out = Vec::new();
    for (rel, path) in scaffold_markdown_files(scaffold_root) {
        if BROAD_DOCS.contains(&rel.as_str()) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        let Some(entity) = parse_markdown_entity(&rel, &content) else { continue };
        if !entity.grounds_to.is_empty() {
            continue;
        }
        let body: String = entity.body.chars().take(600).collect();
        let query = format!(
            "{} {} {}",
            entity.title,
            entity.summary.clone().unwrap_or_default(),
            body
        );
        let sel = select_scope(
            conn,
            &query,
            &ScopeRequest {
                max_nodes: 12,
                max_files: 3,
                vector_hits: Vec::new(),
            },
        );
        if sel.evidence_strength != "strong" {
            continue;
        }
        let mut refs = Vec::new();
        for c in &sel.candidates {
            if refs.len() >= per_entity {
                break;
            }
            let Some(node) = sel.nodes.get(&c.id) else { continue };
            if !GROUNDABLE_KINDS.contains(&node.kind.as_str()) || c.category == "test" || !strong_enough(&c.reasons) {
                continue;
            }
            refs.push(ProposedRef {
                reference: readable_ref_for(node),
                node_id: node.id.clone(),
                kind: node.kind.clone(),
                qualified_name: node.qualified_name.clone(),
                file_path: node.file_path.clone(),
                score: c.score,
                reasons: c.reasons.clone(),
            });
        }
        if !refs.is_empty() {
            out.push(GroundProposal {
                doc: rel,
                entity_id: entity.id.clone(),
                title: entity.title.clone(),
                refs,
            });
        }
    }
    out
}

fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Insert `grounds_to` entries into a document's frontmatter (creating the key, replacing an
/// empty `grounds_to: []`). Returns the new text, or `None` when the document has no
/// frontmatter or already grounds something.
pub fn insert_grounds_to(content: &str, refs: &[String]) -> Option<String> {
    let rest = content.strip_prefix("---\n").or_else(|| content.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let after = &rest[end..];
    let mut lines: Vec<String> = front.lines().map(String::from).collect();
    let entries: Vec<String> = refs.iter().map(|r| format!("  - {}", yaml_quote(r))).collect();
    if let Some(i) = lines.iter().position(|l| l.trim_start().starts_with("grounds_to:") && !l.starts_with(' ')) {
        let value = lines[i].split_once(':').map(|(_, v)| v.trim()).unwrap_or("");
        let has_items = lines.get(i + 1).is_some_and(|n| n.trim_start().starts_with("- "));
        if !(value.is_empty() || value == "[]") || has_items {
            return None;
        }
        lines[i] = "grounds_to:".to_string();
        for (k, e) in entries.into_iter().enumerate() {
            lines.insert(i + 1 + k, e);
        }
    } else {
        lines.push("grounds_to:".to_string());
        lines.extend(entries);
    }
    Some(format!("---\n{}{}", lines.join("\n"), after))
}

/// Write proposals into their documents. Returns the number of documents changed.
pub fn apply_groundings(scaffold_root: &Path, proposals: &[GroundProposal]) -> std::io::Result<usize> {
    let mut changed = 0;
    for p in proposals {
        let path = scaffold_root.join(&p.doc);
        let content = std::fs::read_to_string(&path)?;
        let refs: Vec<String> = p.refs.iter().map(|r| r.reference.clone()).collect();
        if let Some(text) = insert_grounds_to(&content, &refs) {
            crate::wiki::ops::write_atomic(&path, &text)?;
            changed += 1;
        }
    }
    Ok(changed)
}

/// Prompt for agent-led, prose-preserving retro-grounding of an existing scaffold.
pub fn ground_prompt(scaffold_dir: &str) -> String {
    format!(
        r#"You are retro-grounding an existing populated Knobyte scaffold.
The prose in {dir}/ is the user's accumulated project knowledge. Preserve it.
This is a pointer migration, not a scaffold rewrite or regeneration.

Read every existing scaffold markdown file under {dir}/. Treat its prose as ground truth: do not
rephrase, reorder, expand, shorten or replace it. Do not create or delete scaffold files.
The only permitted edit is adding or updating the YAML frontmatter `grounds_to` list.

Use the code graph for all code lookup:
- knobyte graph scope "<behaviour described by this file>" --jsonl for broad context
- knobyte graph get <id> --jsonl to read the body of a node you intend to ground
- knobyte graph query where-defined <symbol> --jsonl to resolve an exact mention
- knobyte graph query who-calls <symbol> --jsonl / what-calls <symbol> when call context matters
- knobyte impact <symbol|file> --jsonl when blast radius helps disambiguate
- knobyte graph ground --dry-run --json for deterministic candidate groundings to verify

READ BROAD, GROUND TIGHT. Ground only the specific functions, methods, types or routes that
embody behavioural claims already in the prose. Callers and callees are reading context, not
automatic targets. Broad architecture/stack/conventions documents stay sparse or ungrounded.
Never ground file, module, import or parameter nodes, and never add grounding just so every
file has an entry.

Write each grounding as the readable reference from the graph's `ref` field:

grounds_to:
  - "function:src/path/file.rs:qualified::name"

Make the migration idempotent: merge with existing entries, never duplicate a reference.
Before finishing, re-read every changed file and verify the prose is unchanged and every
reference came from graph output. Never invent references. Report which files gained
grounding and which were intentionally left ungrounded. Knobyte captures baselines afterwards
(`knobyte graph ground --rebaseline`)."#,
        dir = scaffold_dir
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_grounds_to_once() {
        let doc = "---\nid: kb_a\ntitle: A\ntype: spec\n---\n# A\nBody\n";
        let out = insert_grounds_to(doc, &["function:src/a.rs:run".into()]).unwrap();
        assert!(out.contains("grounds_to:\n  - \"function:src/a.rs:run\"\n---\n# A"), "{}", out);
        assert!(insert_grounds_to(&out, &["function:src/a.rs:other".into()]).is_none());
        let empty = "---\nid: kb_a\ngrounds_to: []\ntitle: A\n---\nx\n";
        let out = insert_grounds_to(empty, &["struct:src/b.rs:B".into()]).unwrap();
        assert!(out.contains("grounds_to:\n  - \"struct:src/b.rs:B\"\ntitle: A"), "{}", out);
        assert!(insert_grounds_to("no frontmatter", &["x".into()]).is_none());
    }
}
