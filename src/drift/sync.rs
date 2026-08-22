use std::collections::BTreeMap;
use std::fs;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::config::KnobyteConfig;
use crate::drift::brief::{build_sync_brief_with, SyncBrief, SyncBriefOptions};
use crate::drift::checker::run_drift_check;
use crate::graph::engine::{map_node_row, NODE_COLUMNS};
use crate::drift::freshness::{inspect_engine, GraphState};
use crate::graph::grounding::{
    extract_doc_refs, kinds_equivalent, last_segment, move_baseline, parse_grounding_ref,
    relocated_ref, resolve_baseline, resolve_grounding_ref, scaffold_markdown_files,
    CommittedIndex, DocRef, EffectiveBaseline, ParsedRef, RefOrigin, RefResolution,
};
use crate::graph::models::Node;
use crate::graph::GraphEngine;
use crate::wiki::WikiIndex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelocationProposal {
    pub scaffold_file: String,
    /// Grounding reference currently written in the document.
    pub old_node_id: String,
    /// Readable reference (`kind:path:qualified_name`) that replaces it.
    pub new_node_id: String,
    pub symbol_name: String,
    pub old_file: Option<String>,
    pub new_file: String,
    pub confidence: f64,
    pub reason: String,
    /// Internal graph node id the new reference resolves to.
    #[serde(default)]
    pub resolved_node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    pub proposals: Vec<RelocationProposal>,
    /// Anchors actually rewritten (always 0 for a dry run).
    pub relocated_count: usize,
    pub dry_run: bool,
    pub success: bool,
    pub message: String,
    /// Why relocation was skipped (the graph is not fresh), when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncAction {
    pub file: String,
    pub recommendation: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncReport {
    pub actions: Vec<SyncAction>,
    pub clean: bool,
    pub proposals: Vec<RelocationProposal>,
    /// Grouped, grounding-aware repair brief for the files in `actions`.
    #[serde(default)]
    pub brief: SyncBrief,
}

/// Why relocation must not run now: `Some(summary)` unless the graph is fresh. A stale
/// snapshot never reconciles (a symbol that looks gone may have moved into an edited file).
pub fn relocation_blocker(config: &KnobyteConfig) -> Option<String> {
    let db = config.graph_db_path();
    if !db.exists() {
        return Some("graph missing · run `knobyte graph rebuild`".to_string());
    }
    let fresh = crate::drift::freshness::inspect_graph(config);
    (fresh.status != GraphState::Fresh).then(|| fresh.summary())
}

/// Find grounding references that no longer resolve and propose where the symbol moved.
/// References that still resolve are never touched. Nothing is proposed unless the graph is
/// fresh. Baselines come from the committed markdown first and the graph.db cache second.
pub fn find_grounding_relocations(
    config: &KnobyteConfig,
) -> Result<Vec<RelocationProposal>, String> {
    let scaffold_root = &config.scaffold_root;
    if !scaffold_root.exists() {
        return Ok(Vec::new());
    }

    let graph_db_path = config.graph_db_path();
    if !graph_db_path.exists() {
        return Ok(Vec::new());
    }

    let engine = GraphEngine::open(&graph_db_path)
        .map_err(|e| format!("Failed to open graph database: {}", e))?;
    if inspect_engine(&engine, &config.project_root).status != GraphState::Fresh {
        return Ok(Vec::new());
    }
    let conn = engine.connection();

    let docs: Vec<(String, Vec<DocRef>)> = scaffold_markdown_files(scaffold_root)
        .into_iter()
        .filter_map(|(rel, path)| {
            fs::read_to_string(path)
                .ok()
                .map(|c| (rel, extract_doc_refs(&c)))
        })
        .collect();
    let index = CommittedIndex::build(docs.iter().map(|(d, r)| (d.as_str(), r.as_slice())));

    let mut proposals: Vec<RelocationProposal> = Vec::new();
    for (doc, refs) in &docs {
        for r in refs {
            if proposals
                .iter()
                .any(|p| &p.scaffold_file == doc && p.old_node_id == r.reference)
            {
                continue;
            }
            match resolve_grounding_ref(conn, &r.reference) {
                Ok(RefResolution::Missing) => {
                    let baseline = resolve_baseline(Some(conn), doc, r, &index);
                    if baseline.conflict {
                        continue;
                    }
                    if let Reconciliation::Moved { proposal, .. } =
                        reconcile_missing_ref_with(conn, doc, &r.reference, &baseline)
                    {
                        proposals.push(proposal);
                    }
                }
                // Intact, or ambiguous (not moved - needs a human to disambiguate).
                Ok(_) => {}
                Err(e) => {
                    return Err(format!(
                        "Failed to resolve grounding '{}': {}",
                        r.reference, e
                    ))
                }
            }
        }
    }

    Ok(proposals)
}

fn query_nodes(conn: &Connection, where_clause: &str, value: &str) -> Vec<Node> {
    let sql = format!(
        "SELECT {} FROM nodes WHERE {} AND kind NOT IN ('file', 'module') LIMIT 50",
        NODE_COLUMNS, where_clause
    );
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare(&sql) {
        if let Ok(rows) = stmt.query_map(params![value], map_node_row) {
            out.extend(rows.flatten());
        }
    }
    out
}

fn proposal(
    doc: &str,
    old_ref: &str,
    node: &Node,
    old_file: Option<String>,
    confidence: f64,
    reason: String,
) -> RelocationProposal {
    RelocationProposal {
        scaffold_file: doc.to_string(),
        old_node_id: old_ref.to_string(),
        new_node_id: relocated_ref(old_ref, node),
        symbol_name: node.name.clone(),
        old_file,
        new_file: node.file_path.clone(),
        confidence,
        reason,
        resolved_node_id: node.id.clone(),
    }
}

/// What the evidence decided a relocation from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveEvidence {
    /// The baseline body hash matched exactly one symbol.
    Body,
    /// A unique symbol with the same name (and kind) exists.
    Name,
    /// The MinHash reconciler decided by caller/callee continuity (small or renamed bodies).
    Neighbors,
    /// Several symbols matched; the one in the reference's old file or container was chosen.
    Surroundings,
    /// MinHash body similarity (LSH) with neighbour evidence picked the symbol (moved/renamed).
    Fingerprint,
}

/// Outcome of reconciling a grounding reference that no longer resolves.
#[derive(Debug, Clone)]
pub enum Reconciliation {
    Moved {
        proposal: RelocationProposal,
        evidence: MoveEvidence,
    },
    /// Several candidates and nothing to choose between them.
    Ambiguous(Vec<Node>),
    Gone,
}

fn qualified_parent(q: &str) -> String {
    let norm = q.replace("::", ".");
    norm.rsplit_once('.')
        .map(|(p, _)| p.to_string())
        .unwrap_or_default()
}

/// Pick the single candidate that shares the old reference's file, else its container.
/// Returns the candidate and what decided it (`file src/a.rs` / `container Ledger`).
fn neighbor_tiebreak(
    candidates: &[Node],
    old_file: Option<&str>,
    want_qualified: Option<&str>,
) -> Option<(Node, String)> {
    if let Some(f) = old_file {
        let same: Vec<&Node> = candidates.iter().filter(|n| n.file_path == f).collect();
        if same.len() == 1 {
            return Some((same[0].clone(), format!("its previous file {}", f)));
        }
    }
    if let Some(q) = want_qualified {
        let parent = qualified_parent(q);
        if !parent.is_empty() {
            let same: Vec<&Node> = candidates
                .iter()
                .filter(|n| qualified_parent(&n.qualified_name) == parent)
                .collect();
            if same.len() == 1 {
                return Some((same[0].clone(), format!("its container {}", parent)));
            }
        }
    }
    None
}

/// Decide where a grounding reference that no longer resolves went: an exact body-hash match,
/// a unique same-name symbol, a candidate chosen by its surroundings, several equally likely
/// candidates, or nothing.
///
/// The exact body hash and readable-reference name strategies run first; when they cannot
/// decide (gone, or several candidates), the MinHash/LSH reconciler judges moves and renames
/// from body similarity and caller/callee continuity.
pub fn reconcile_missing_ref(conn: &Connection, doc: &str, old_ref: &str) -> Reconciliation {
    let r = DocRef {
        reference: old_ref.to_string(),
        origin: RefOrigin::Frontmatter,
        committed: Default::default(),
    };
    let baseline = resolve_baseline(Some(conn), doc, &r, &CommittedIndex::default());
    reconcile_missing_ref_with(conn, doc, old_ref, &baseline)
}

/// [`reconcile_missing_ref`] against an explicit baseline (committed values first, cache
/// second; see [`resolve_baseline`]).
pub fn reconcile_missing_ref_with(
    conn: &Connection,
    doc: &str,
    old_ref: &str,
    baseline: &EffectiveBaseline,
) -> Reconciliation {
    let first = reconcile_exact_or_named(conn, doc, old_ref, baseline);
    if matches!(first, Reconciliation::Moved { .. }) {
        return first;
    }
    let parsed = parse_grounding_ref(old_ref);
    let Some(minhash) = baseline.minhash.clone() else {
        return first;
    };
    let (kind, old_file) = match &parsed {
        ParsedRef::Readable(r) => (Some(r.kind.clone()), Some(r.file_path.clone())),
        _ => (
            baseline
                .source
                .as_deref()
                .and_then(extract_symbol_name)
                .map(|(k, _)| k),
            None,
        ),
    };
    let fingerprint = crate::graph::reconcile::BaselineFingerprint {
        minhash,
        neighbors: baseline.neighbors.clone(),
        body_hash: baseline.body_hash.clone(),
        kind,
    };
    let node_of = |id: &str| -> Option<Node> {
        conn.query_row(
            &format!("SELECT {} FROM nodes WHERE id = ?1", NODE_COLUMNS),
            params![id],
            map_node_row,
        )
        .ok()
    };
    use crate::graph::reconcile::{reconcile, Evidence, Verdict};
    match reconcile(conn, &fingerprint, baseline.node_id.as_deref()) {
        Verdict::Moved { node_id, score, evidence } => match node_of(&node_id) {
            Some(n) => {
                let (reason, ev) = match evidence {
                    Evidence::Neighbors => (
                        format!(
                            "Matched by callers and callees (neighbour continuity), not body: {} symbol '{}' in {}",
                            n.kind,
                            last_segment(&n.qualified_name),
                            n.file_path
                        ),
                        MoveEvidence::Neighbors,
                    ),
                    Evidence::Body => (
                        format!(
                            "Moved or renamed: MinHash body similarity with neighbour evidence ({:.2}) for {} symbol '{}' in {}",
                            score,
                            n.kind,
                            last_segment(&n.qualified_name),
                            n.file_path
                        ),
                        MoveEvidence::Fingerprint,
                    ),
                };
                Reconciliation::Moved {
                    proposal: proposal(doc, old_ref, &n, old_file, score.min(0.95), reason),
                    evidence: ev,
                }
            }
            None => first,
        },
        Verdict::Ambiguous { candidate, .. } => match (first, node_of(&candidate)) {
            (Reconciliation::Ambiguous(mut list), Some(n)) => {
                if let Some(pos) = list.iter().position(|c| c.id == n.id) {
                    let best = list.remove(pos);
                    list.insert(0, best);
                } else {
                    list.insert(0, n);
                }
                Reconciliation::Ambiguous(list)
            }
            (_, Some(n)) => Reconciliation::Ambiguous(vec![n]),
            (other, None) => other,
        },
        Verdict::Gone => first,
    }
}

/// Strategies A (exact body hash) and B (unique readable-reference name, neighbour tiebreak).
fn reconcile_exact_or_named(
    conn: &Connection,
    doc: &str,
    old_ref: &str,
    baseline: &EffectiveBaseline,
) -> Reconciliation {
    let parsed = parse_grounding_ref(old_ref);

    // What we know about the symbol: from the readable ref, else from the baseline source.
    let (want_kind, want_name, want_qualified, old_file) = match &parsed {
        ParsedRef::Readable(r) => (
            Some(r.kind.clone()),
            Some(r.symbol_name().to_string()),
            Some(r.qualified_name.clone()),
            Some(r.file_path.clone()),
        ),
        _ => {
            let from_source = baseline.source.as_deref().and_then(extract_symbol_name);
            match from_source {
                Some((k, n)) => (Some(k), Some(n), None, None),
                None => (None, None, None, None),
            }
        }
    };
    let kind_ok = |n: &Node| {
        want_kind
            .as_deref()
            .map(|k| kinds_equivalent(k, &n.kind))
            .unwrap_or(true)
    };
    let neighbors = |cands: &[Node]| {
        neighbor_tiebreak(cands, old_file.as_deref(), want_qualified.as_deref()).map(|(n, by)| {
            let reason = format!(
                "Matched by {}, not body: {} symbol '{}' in {}",
                by,
                n.kind,
                last_segment(&n.qualified_name),
                n.file_path
            );
            Reconciliation::Moved {
                proposal: proposal(doc, old_ref, &n, old_file.clone(), 0.8, reason),
                evidence: MoveEvidence::Surroundings,
            }
        })
    };

    // Strategy A: exact AST body hash from the baseline.
    let mut body_ambiguous: Vec<Node> = Vec::new();
    if let Some(body_hash) = &baseline.body_hash {
        let mut matches: Vec<Node> = query_nodes(conn, "body_hash = ?1", body_hash)
            .into_iter()
            .filter(|n| kind_ok(n))
            .collect();
        if matches.len() > 1 {
            if let Some(name) = &want_name {
                let named: Vec<Node> = matches
                    .iter()
                    .filter(|n| &n.name == name)
                    .cloned()
                    .collect();
                if !named.is_empty() {
                    matches = named;
                }
            }
        }
        if matches.len() == 1 {
            let n = &matches[0];
            return Reconciliation::Moved {
                proposal: proposal(
                    doc,
                    old_ref,
                    n,
                    old_file.clone(),
                    1.0,
                    format!("Exact AST body hash match for {} symbol", n.kind),
                ),
                evidence: MoveEvidence::Body,
            };
        }
        if matches.len() > 1 {
            if let Some(moved) = neighbors(&matches) {
                return moved;
            }
            body_ambiguous = matches;
        }
    }

    // Strategy B: unique symbol with the same name (and kind; function also matches method).
    let Some(name) = want_name.clone() else {
        return if body_ambiguous.is_empty() {
            Reconciliation::Gone
        } else {
            Reconciliation::Ambiguous(body_ambiguous)
        };
    };
    let mut matches: Vec<Node> = query_nodes(conn, "name = ?1", &name)
        .into_iter()
        .filter(|n| kind_ok(n))
        .collect();
    if matches.len() > 1 {
        if let Some(q) = &want_qualified {
            let norm = |s: &str| s.replace("::", ".");
            let exact: Vec<Node> = matches
                .iter()
                .filter(|n| norm(&n.qualified_name) == norm(q))
                .cloned()
                .collect();
            if !exact.is_empty() {
                matches = exact;
            }
        }
    }
    if matches.len() == 1 {
        let n = &matches[0];
        let confidence = if baseline.body_hash.is_some() { 0.92 } else { 0.88 };
        return Reconciliation::Moved {
            proposal: proposal(
                doc,
                old_ref,
                n,
                old_file.clone(),
                confidence,
                format!(
                    "Unique matching {} symbol '{}' found in {}",
                    n.kind,
                    last_segment(&n.qualified_name),
                    n.file_path
                ),
            ),
            evidence: MoveEvidence::Name,
        };
    }
    if matches.len() > 1 {
        if let Some(moved) = neighbors(&matches) {
            return moved;
        }
        return Reconciliation::Ambiguous(matches);
    }
    if body_ambiguous.is_empty() {
        Reconciliation::Gone
    } else {
        Reconciliation::Ambiguous(body_ambiguous)
    }
}

fn strip_yaml_quotes(s: &str) -> &str {
    let t = s.trim();
    t.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .or_else(|| t.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
        .unwrap_or(t)
}

/// Replace exact occurrences of a grounding anchor: YAML `grounds_to` list items (block or
/// flow style, or `node_id:` keys) inside the frontmatter, and `<!-- kb-ground: ... -->`
/// comments in the body. Other text mentioning the reference is left alone.
/// Returns the new content and the number of anchors rewritten.
pub fn rewrite_grounding_anchors(content: &str, old_ref: &str, new_ref: &str) -> (String, usize) {
    let mut out = String::with_capacity(content.len() + 32);
    let mut count = 0;
    let mut seen_content = false;
    let mut in_frontmatter = false;

    for raw_line in content.split_inclusive('\n') {
        let line_body = raw_line.trim_end_matches(['\n', '\r']);
        let ending = &raw_line[line_body.len()..];
        let trimmed = line_body.trim();

        if !seen_content && !trimmed.is_empty() {
            seen_content = true;
            if trimmed == "---" {
                in_frontmatter = true;
                out.push_str(raw_line);
                continue;
            }
        }
        if in_frontmatter && trimmed == "---" {
            in_frontmatter = false;
            out.push_str(raw_line);
            continue;
        }

        let mut new_line: Option<String> = None;
        if in_frontmatter {
            // Block list item: `- ref`, `- "ref"`
            if let Some(item) = trimmed.strip_prefix("- ") {
                let item_trimmed = item.trim();
                let value = ["node_id:", "ref:", "node:"]
                    .iter()
                    .find_map(|k| item_trimmed.strip_prefix(k))
                    .map(str::trim)
                    .unwrap_or(item_trimmed);
                if strip_yaml_quotes(value) == old_ref {
                    new_line = Some(line_body.replacen(old_ref, new_ref, 1));
                }
            } else if let Some(value) = ["node_id:", "ref:", "node:"]
                .iter()
                .find_map(|k| trimmed.strip_prefix(k))
            {
                if strip_yaml_quotes(value) == old_ref {
                    new_line = Some(line_body.replacen(old_ref, new_ref, 1));
                }
            } else if let Some(rest) = trimmed.strip_prefix("grounds_to:") {
                // Flow list: `grounds_to: [a, "b"]`
                let rest = rest.trim();
                if let Some(inner) = rest.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
                    let items: Vec<&str> = inner.split(',').collect();
                    if items.iter().any(|i| strip_yaml_quotes(i) == old_ref) {
                        let rebuilt: Vec<String> = items
                            .iter()
                            .map(|i| {
                                if strip_yaml_quotes(i) == old_ref {
                                    i.replacen(old_ref, new_ref, 1)
                                } else {
                                    i.to_string()
                                }
                            })
                            .collect();
                        let indent = &line_body[..line_body.len() - line_body.trim_start().len()];
                        new_line = Some(format!("{}grounds_to: [{}]", indent, rebuilt.join(",")));
                    }
                }
            }
        } else {
            for prefix in ["<!-- kb-ground:", "<!-- grounds:", "<!-- kb-anchor:"] {
                if let Some(rest) = trimmed.strip_prefix(prefix) {
                    if let Some(id_part) = rest.strip_suffix("-->") {
                        if crate::graph::grounding::split_anchor(id_part).0 == old_ref {
                            new_line = Some(line_body.replacen(old_ref, new_ref, 1));
                        }
                    }
                    break;
                }
            }
        }

        match new_line {
            Some(l) => {
                count += 1;
                out.push_str(&l);
                out.push_str(ending);
            }
            None => out.push_str(raw_line),
        }
    }

    (out, count)
}

/// Apply relocations: rewrite anchors in the markdown documents (readable refs only) and move
/// each grounding baseline to its new reference. Returns the number of proposals applied.
pub fn apply_grounding_relocations(
    config: &KnobyteConfig,
    proposals: &[RelocationProposal],
) -> Result<usize, String> {
    if proposals.is_empty() {
        return Ok(0);
    }

    let engine = GraphEngine::open(&config.graph_db_path())
        .map_err(|e| format!("Failed to open graph database: {}", e))?;
    let conn = engine.connection();

    // Group by document so each file is read and written once.
    let mut by_doc: BTreeMap<&str, Vec<&RelocationProposal>> = BTreeMap::new();
    for p in proposals {
        by_doc.entry(p.scaffold_file.as_str()).or_default().push(p);
    }

    let mut applied = 0;
    for (doc, props) in by_doc {
        let file_path = config.scaffold_root.join(doc);
        let mut content = match fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut moved: Vec<&RelocationProposal> = Vec::new();
        for p in props {
            let (updated, n) = rewrite_grounding_anchors(&content, &p.old_node_id, &p.new_node_id);
            if n > 0 {
                content = updated;
                moved.push(p);
            }
        }
        if moved.is_empty() {
            continue;
        }
        fs::write(&file_path, &content)
            .map_err(|e| format!("Failed to write {}: {}", file_path.display(), e))?;

        for p in moved {
            let node_id = if p.resolved_node_id.is_empty() {
                resolve_grounding_ref(conn, &p.new_node_id)
                    .ok()
                    .and_then(|r| r.node().map(|n| n.id.clone()))
                    .unwrap_or_default()
            } else {
                p.resolved_node_id.clone()
            };
            let _ = move_baseline(conn, doc, &p.old_node_id, &p.new_node_id, &node_id);
            applied += 1;
        }
    }

    if applied > 0 {
        // Rebuild wiki index to reflect the relocated anchors
        if let Ok(mut wiki) = WikiIndex::open(&config.wiki_db_path()) {
            let _ = wiki.rebuild(&config.scaffold_root);
        }
    }

    Ok(applied)
}

/// Keep only proposals whose anchor can actually be rewritten in the current file content.
fn applicable_proposals(
    config: &KnobyteConfig,
    proposals: Vec<RelocationProposal>,
) -> Vec<RelocationProposal> {
    proposals
        .into_iter()
        .filter(|p| {
            fs::read_to_string(config.scaffold_root.join(&p.scaffold_file))
                .map(|c| rewrite_grounding_anchors(&c, &p.old_node_id, &p.new_node_id).1 > 0)
                .unwrap_or(false)
        })
        .collect()
}

/// Full synchronization workflow: finds relocations, optionally applies them, and returns summary.
pub fn sync_groundings(config: &KnobyteConfig, dry_run: bool) -> Result<SyncResult, String> {
    if config.graph_db_path().exists() {
        if let Some(reason) = relocation_blocker(config) {
            return Ok(SyncResult {
                proposals: Vec::new(),
                relocated_count: 0,
                dry_run,
                success: true,
                message: format!(
                    "Grounding relocation skipped: the code graph is not fresh ({}).",
                    reason
                ),
                skipped: Some(reason),
            });
        }
    }
    let proposals = applicable_proposals(config, find_grounding_relocations(config)?);

    if dry_run {
        let count = proposals.len();
        return Ok(SyncResult {
            proposals,
            relocated_count: 0,
            dry_run: true,
            success: true,
            message: format!(
                "Dry-run: {} grounding anchor(s) would be relocated; no files were changed",
                count
            ),
            skipped: None,
        });
    }

    let relocated = apply_grounding_relocations(config, &proposals)?;

    Ok(SyncResult {
        proposals,
        relocated_count: relocated,
        dry_run: false,
        success: true,
        message: format!(
            "Successfully relocated and healed {} grounding anchor(s)",
            relocated
        ),
        skipped: None,
    })
}

pub fn plan_sync(config: &KnobyteConfig, include_warnings: bool) -> SyncReport {
    let report = run_drift_check(config);
    let proposals = applicable_proposals(
        config,
        find_grounding_relocations(config).unwrap_or_default(),
    );
    let brief = build_sync_brief_with(config, &report, SyncBriefOptions { include_warnings });

    let mut actions = Vec::new();
    for (target, file_brief) in brief.targets.iter().zip(&brief.files) {
        let relocations: Vec<String> = proposals
            .iter()
            .filter(|p| {
                crate::drift::types::project_relative(
                    &config.project_root,
                    &config.scaffold_root.join(&p.scaffold_file),
                ) == target.file
                    && target
                        .issues
                        .iter()
                        .any(|i| i.symbol.as_deref() == Some(p.old_node_id.as_str()))
            })
            .map(|p| {
                format!(
                    "auto-relocate '{}' from {} -> {} (confidence: {:.0}%)",
                    p.symbol_name,
                    p.old_file.as_deref().unwrap_or("previous location"),
                    p.new_file,
                    p.confidence * 100.0
                )
            })
            .collect();
        let mut recommendation = format!(
            "Fix {} error(s) and {} warning(s) in {}",
            target.errors(),
            target.warnings(),
            target.file
        );
        if !relocations.is_empty() {
            recommendation.push_str(&format!(
                "; {} (run 'knobyte sync' to apply)",
                relocations.join("; ")
            ));
        }
        actions.push(SyncAction {
            file: target.file.clone(),
            recommendation,
            prompt: file_brief.prompt.clone(),
        });
    }

    let clean = actions.is_empty();
    SyncReport {
        actions,
        clean,
        proposals,
        brief,
    }
}

/// Best-effort (kind, name) of the symbol declared in a source snippet.
/// Function-like declarations report kind `function`, which also matches methods.
fn extract_symbol_name(source: &str) -> Option<(String, String)> {
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('#')
            || trimmed.starts_with('*')
            || trimmed.starts_with('@')
        {
            continue;
        }

        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        for (i, &t) in tokens.iter().enumerate() {
            if (t == "fn" || t == "def" || t == "function") && i + 1 < tokens.len() {
                let name = tokens[i + 1].split('(').next()?.split('<').next()?.trim();
                if !name.is_empty() {
                    return Some(("function".to_string(), name.to_string()));
                }
            } else if (t == "struct"
                || t == "class"
                || t == "enum"
                || t == "trait"
                || t == "interface"
                || t == "type")
                && i + 1 < tokens.len()
            {
                let name = tokens[i + 1]
                    .split('{')
                    .next()?
                    .split('<')
                    .next()?
                    .split('(')
                    .next()?
                    .trim();
                let clean = name.trim_end_matches(';').trim_end_matches(':').trim();
                if !clean.is_empty() {
                    let kind = if t == "type" { "type_alias" } else { t };
                    return Some((kind.to_string(), clean.to_string()));
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_only_exact_anchors() {
        let old = "function:src/a.rs:calc";
        let new = "function:src/b.rs:calc";
        let doc = "---\nid: x\ngrounds_to:\n  - function:src/a.rs:calc\n  - function:src/a.rs:calc_more\n---\n# T\n\nSee function:src/a.rs:calc in prose.\n<!-- kb-ground: function:src/a.rs:calc -->\n<!-- kb-ground: function:src/a.rs:calc_more -->\n";
        let (out, n) = rewrite_grounding_anchors(doc, old, new);
        assert_eq!(n, 2);
        assert!(out.contains("  - function:src/b.rs:calc\n"));
        assert!(out.contains("  - function:src/a.rs:calc_more\n"));
        assert!(out.contains("See function:src/a.rs:calc in prose."));
        assert!(out.contains("<!-- kb-ground: function:src/b.rs:calc -->"));
        assert!(out.contains("<!-- kb-ground: function:src/a.rs:calc_more -->"));
    }

    #[test]
    fn rewrites_flow_lists() {
        let doc = "---\ngrounds_to: [\"function:src/a.rs:f\", function:src/a.rs:g]\n---\nbody\n";
        let (out, n) = rewrite_grounding_anchors(doc, "function:src/a.rs:f", "function:src/b.rs:f");
        assert_eq!(n, 1);
        assert!(out.contains("grounds_to: [\"function:src/b.rs:f\", function:src/a.rs:g]"));
    }
}
