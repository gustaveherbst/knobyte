//! Grounding checker: every grounding reference in the scaffold (frontmatter `grounds_to`
//! entries and `<!-- kb-ground: ... -->` anchors) against the code graph.
//!
//! | status                         | severity                            | meaning                                         |
//! |--------------------------------|-------------------------------------|-------------------------------------------------|
//! | intact                         | -                                   | resolves; body matches its baseline             |
//! | `GROUNDING_DRIFT`              | warning                             | body changed, or an anchor should move          |
//! | `GROUNDING_MOVED_BY_NEIGHBORS` | info (unscored)                     | move decided by surroundings, not by body       |
//! | `GROUNDING_AMBIGUOUS`          | warning                             | several candidates, nothing to choose between   |
//! | `GROUNDING_GONE`               | error (frontmatter) / warning (anchor) | nothing in the graph matches                 |
//! | `GROUNDING_UNVERIFIED`         | warning / info                      | stale graph for its source file (warning), or no baseline recorded yet (info) |
//!
//! A frontmatter grounding that moved with an unchanged body adds no issue (it is counted as
//! moved; `knobyte sync` rewrites it).
//!
//! **`knobyte check` is read-only.** Baselines are resolved from the committed markdown first
//! (`body_hash` / `fingerprint` on `grounds_to` entries, `#<body_hash>` on anchors) and the
//! graph.db cache second; they are written only by `knobyte graph ground --rebaseline`, the
//! accept step of `knobyte sync` / `check --fix`, and setup's grounding capture.
//!
//! A stale graph never reconciles: a node that looks gone may have moved into an edited file.
//! Groundings in edited files are re-extracted from the working tree, in memory, and judged
//! exactly (DRIFT or intact); anything that cannot be settled is UNVERIFIED.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::config::KnobyteConfig;
use crate::drift::freshness::{GraphFreshness, GraphState};
use crate::drift::sync::{reconcile_missing_ref_with, MoveEvidence, Reconciliation};
use crate::drift::types::{
    codes, project_relative, DriftIssue, SEVERITY_ERROR, SEVERITY_INFO, SEVERITY_WARNING,
};
use crate::graph::fingerprint::{compute_body_hash, compute_node_id};
pub use crate::graph::grounding::{extract_doc_refs, DocRef, RefOrigin};
use crate::graph::grounding::{
    kinds_equivalent, parse_grounding_ref, readable_ref_for, resolve_baseline,
    resolve_grounding_ref, CommittedIndex, EffectiveBaseline, GroundingRef, ParsedRef,
    RefResolution,
};
use crate::graph::models::{ExtractedSymbol, Node};

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct GroundingHealth {
    pub intact: usize,
    /// Resolves, but the body changed since its baseline.
    pub changed: usize,
    /// Does not resolve as written (`moved + ambiguous + gone`).
    pub missing: usize,
    pub total: usize,
    /// Does not resolve as written, but a relocation candidate was found.
    #[serde(default)]
    pub moved: usize,
    #[serde(default)]
    pub ambiguous: usize,
    #[serde(default)]
    pub gone: usize,
    /// Could not be checked (no usable graph, its source changed since the graph was built, or
    /// no baseline has been recorded yet).
    #[serde(default)]
    pub unverified: usize,
}

/// A scaffold document to check: scaffold-relative path, absolute path, content.
pub struct GroundingDoc<'a> {
    pub scaffold_rel: &'a str,
    pub path: &'a Path,
    pub content: &'a str,
}

fn issue(code: &str, severity: &str, source: &str, origin: RefOrigin, msg: String) -> DriftIssue {
    let line = match origin {
        RefOrigin::Anchor(l) => Some(l),
        RefOrigin::Frontmatter => None,
    };
    DriftIssue::new(code, severity, source, line, msg)
}

fn subject(origin: RefOrigin) -> &'static str {
    match origin {
        RefOrigin::Frontmatter => "Grounded node",
        RefOrigin::Anchor(_) => "Inline anchor",
    }
}

/// The info notice for a move that surroundings decided, naming the evidence. Not scored.
pub fn moved_by_neighbors_message(
    origin: RefOrigin,
    old_ref: &str,
    new_ref: &str,
    evidence: MoveEvidence,
    reason: &str,
) -> String {
    let by = match evidence {
        MoveEvidence::Neighbors => "callers and callees".to_string(),
        _ => reason
            .strip_prefix("Matched by ")
            .and_then(|r| r.split(", not body").next())
            .unwrap_or("surrounding symbols")
            .to_string(),
    };
    format!(
        "{} matched by {}, not body: {} → {}",
        subject(origin),
        by,
        old_ref,
        new_ref
    )
}

/// What a snapshot that is stale only by changed source can still say about one grounding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceDriftResolution {
    /// The body as a refresh would record it: from the snapshot when its file is unchanged,
    /// or re-derived exactly from the edited file.
    Current { body_hash: Option<String> },
    /// Only a refresh can settle this grounding; the reason says why.
    Unverified(String),
}

/// Grounding against a graph whose only fault is that source files changed: edited files are
/// re-extracted from the working tree, in memory, once each.
pub struct SourceDrift<'a> {
    project_root: &'a Path,
    changed: BTreeSet<&'a str>,
    deleted: BTreeSet<&'a str>,
    unparsed: BTreeSet<&'a str>,
    extracted: RefCell<HashMap<String, Option<Vec<ExtractedSymbol>>>>,
}

impl<'a> SourceDrift<'a> {
    pub fn new(project_root: &'a Path, freshness: &'a GraphFreshness) -> Self {
        Self {
            project_root,
            changed: freshness
                .modified
                .iter()
                .chain(freshness.added.iter())
                .map(String::as_str)
                .collect(),
            deleted: freshness.deleted.iter().map(String::as_str).collect(),
            unparsed: freshness.unparsed.iter().map(String::as_str).collect(),
            extracted: RefCell::new(HashMap::new()),
        }
    }

    fn symbols(&self, file: &str) -> Option<Vec<ExtractedSymbol>> {
        self.extracted
            .borrow_mut()
            .entry(file.to_string())
            .or_insert_with(|| {
                let content = std::fs::read_to_string(self.project_root.join(file)).ok()?;
                let result = crate::graph::extractor::extract_file(file, &content)?;
                (result.parse_status != "failed").then_some(result.symbols)
            })
            .clone()
    }

    fn resolve_in_edited_file(&self, r: &GroundingRef) -> SourceDriftResolution {
        let Some(symbols) = self.symbols(&r.file_path) else {
            return SourceDriftResolution::Unverified(
                "its edited file could not be re-parsed".to_string(),
            );
        };
        match resolve_in_symbols(&symbols, r) {
            Ok(sym) => SourceDriftResolution::Current {
                body_hash: Some(compute_body_hash(&sym.body)),
            },
            Err(0) => SourceDriftResolution::Unverified(
                "it is no longer in its edited file; it may have moved or been renamed".to_string(),
            ),
            Err(_) => SourceDriftResolution::Unverified(
                "it matches several symbols in its edited file".to_string(),
            ),
        }
    }

    /// Resolve one grounding reference against the snapshot plus the edited files.
    pub fn resolve(&self, conn: &Connection, reference: &str) -> SourceDriftResolution {
        let unverified = |s: &str| SourceDriftResolution::Unverified(s.to_string());
        if let ParsedRef::Readable(r) = parse_grounding_ref(reference) {
            if self.deleted.contains(r.file_path.as_str()) {
                return unverified("its file was deleted");
            }
            if self.unparsed.contains(r.file_path.as_str()) {
                return unverified("its file failed to parse");
            }
            if self.changed.contains(r.file_path.as_str()) {
                return self.resolve_in_edited_file(&r);
            }
        }
        match resolve_grounding_ref(conn, reference) {
            Ok(RefResolution::Resolved(node)) => {
                let file = node.file_path.as_str();
                if self.deleted.contains(file) {
                    return unverified("its file was deleted");
                }
                if self.changed.contains(file) {
                    // A hashed id into an edited file: find the same node id in the new parse.
                    let Some(symbols) = self.symbols(file) else {
                        return unverified("its edited file could not be re-parsed");
                    };
                    return symbols
                        .iter()
                        .find(|s| compute_node_id(file, &s.kind, &s.qualified_name) == node.id)
                        .map(|s| SourceDriftResolution::Current {
                            body_hash: Some(compute_body_hash(&s.body)),
                        })
                        .unwrap_or_else(|| {
                            unverified("it is no longer in its edited file; it may have moved")
                        });
                }
                SourceDriftResolution::Current {
                    body_hash: node.body_hash.clone().filter(|h| !h.is_empty()),
                }
            }
            Ok(RefResolution::Ambiguous(_)) => {
                unverified("it matches several symbols in a stale graph")
            }
            Ok(RefResolution::Missing) | Err(_) => {
                unverified("not in the graph snapshot; it may have moved into a changed file")
            }
        }
    }
}

/// Resolve a readable reference among freshly extracted symbols (same rules as
/// [`resolve_grounding_ref`]). `Err(n)` is the number of candidates when not exactly one.
pub fn resolve_in_symbols<'s>(
    symbols: &'s [ExtractedSymbol],
    r: &GroundingRef,
) -> Result<&'s ExtractedSymbol, usize> {
    let norm = |q: &str| q.replace("::", ".");
    let wanted = norm(&r.qualified_name);
    let bare_ref = !r.qualified_name.contains("::") && !r.qualified_name.contains('.');
    let mut candidates: Vec<&ExtractedSymbol> = symbols
        .iter()
        .filter(|s| kinds_equivalent(&r.kind, &s.kind))
        .filter(|s| norm(&s.qualified_name) == wanted || (bare_ref && s.name == r.qualified_name))
        .collect();
    let narrow = |c: &mut Vec<&'s ExtractedSymbol>, keep: &dyn Fn(&ExtractedSymbol) -> bool| {
        if c.len() > 1 {
            let kept: Vec<&ExtractedSymbol> = c.iter().copied().filter(|s| keep(s)).collect();
            if !kept.is_empty() {
                *c = kept;
            }
        }
    };
    narrow(&mut candidates, &|s| norm(&s.qualified_name) == wanted);
    narrow(&mut candidates, &|s| s.kind == r.kind);
    if candidates.len() > 1 {
        let top: Vec<&ExtractedSymbol> = candidates
            .iter()
            .copied()
            .filter(|s| s.container.is_none())
            .collect();
        if top.len() == 1 {
            candidates = top;
        }
    }
    match candidates.len() {
        1 => Ok(candidates[0]),
        n => Err(n),
    }
}

struct Ctx<'a> {
    source: String,
    issues: &'a mut Vec<DriftIssue>,
    health: &'a mut GroundingHealth,
}

impl Ctx<'_> {
    fn unverified(&mut self, r: &DocRef, reason: &str) {
        self.health.unverified += 1;
        self.issues.push(
            issue(
                codes::GROUNDING_UNVERIFIED,
                SEVERITY_WARNING,
                &self.source,
                r.origin,
                format!(
                    "{} cannot be verified until `knobyte graph refresh`: {} ({})",
                    subject(r.origin),
                    r.reference,
                    reason
                ),
            )
            .with_symbol(&r.reference),
        );
    }

    fn no_baseline(&mut self, r: &DocRef) {
        self.health.unverified += 1;
        self.issues.push(
            issue(
                codes::GROUNDING_UNVERIFIED,
                SEVERITY_INFO,
                &self.source,
                r.origin,
                format!(
                    "{} has no committed baseline, so body drift cannot be detected: {}. Run `knobyte graph ground --rebaseline` to record it.",
                    subject(r.origin),
                    r.reference
                ),
            )
            .with_symbol(&r.reference),
        );
    }

    /// Compare a current body hash against the baseline.
    fn judge_body(
        &mut self,
        r: &DocRef,
        baseline: &EffectiveBaseline,
        current: Option<&str>,
        at: Option<String>,
    ) {
        let Some(base) = baseline.body_hash.as_deref() else {
            self.no_baseline(r);
            return;
        };
        match current {
            Some(cur) if cur != base => {
                self.health.changed += 1;
                self.issues.push(
                    issue(
                        codes::GROUNDING_DRIFT,
                        SEVERITY_WARNING,
                        &self.source,
                        r.origin,
                        format!(
                            "{} body changed: {}{}. Review the documentation, then run `knobyte graph ground --rebaseline` to accept the current code.",
                            subject(r.origin),
                            r.reference,
                            at.map(|a| format!(" ({})", a)).unwrap_or_default()
                        ),
                    )
                    .with_symbol(&r.reference),
                );
            }
            _ => self.health.intact += 1,
        }
    }
}

/// Check every grounding reference of `docs` against the graph. Never writes anything.
pub fn check_groundings(
    config: &KnobyteConfig,
    docs: &[GroundingDoc],
    conn: Option<&Connection>,
    freshness: &GraphFreshness,
) -> (Vec<DriftIssue>, GroundingHealth) {
    let mut issues = Vec::new();
    let mut health = GroundingHealth::default();
    let usable = matches!(freshness.status, GraphState::Fresh | GraphState::Stale);
    let stale = freshness.status == GraphState::Stale;
    let source_drift = SourceDrift::new(&config.project_root, freshness);
    let refs: Vec<Vec<DocRef>> = docs.iter().map(|d| extract_doc_refs(d.content)).collect();
    let index = CommittedIndex::build(
        docs.iter()
            .zip(&refs)
            .map(|(d, r)| (d.scaffold_rel, r.as_slice())),
    );

    for (doc, doc_refs) in docs.iter().zip(&refs) {
        let mut ctx = Ctx {
            source: project_relative(&config.project_root, doc.path),
            issues: &mut issues,
            health: &mut health,
        };
        for r in doc_refs {
            ctx.health.total += 1;
            let conn = match (usable, conn) {
                (true, Some(c)) => c,
                _ => {
                    ctx.health.unverified += 1;
                    continue;
                }
            };
            if let Some(reason) = &freshness.whole_graph {
                ctx.unverified(r, reason);
                continue;
            }
            let baseline = resolve_baseline(Some(conn), doc.scaffold_rel, r, &index);

            if stale {
                match source_drift.resolve(conn, &r.reference) {
                    SourceDriftResolution::Unverified(reason) => ctx.unverified(r, &reason),
                    SourceDriftResolution::Current { body_hash } => {
                        ctx.judge_body(r, &baseline, body_hash.as_deref(), None)
                    }
                }
                continue;
            }
            if let ParsedRef::Readable(p) = parse_grounding_ref(&r.reference) {
                if freshness.unparsed.iter().any(|f| f == &p.file_path) {
                    ctx.unverified(r, "its file failed to parse");
                    continue;
                }
            }

            match resolve_grounding_ref(conn, &r.reference) {
                Ok(RefResolution::Resolved(node)) => ctx.judge_body(
                    r,
                    &baseline,
                    node.body_hash.as_deref().filter(|h| !h.is_empty()),
                    Some(format!("{}:{}", node.file_path, node.start_line)),
                ),
                Ok(RefResolution::Ambiguous(candidates)) => {
                    ctx.health.missing += 1;
                    ctx.health.ambiguous += 1;
                    let example = readable_ref_for(&candidates[0]);
                    let mut i = issue(
                        codes::GROUNDING_AMBIGUOUS,
                        SEVERITY_WARNING,
                        &ctx.source,
                        r.origin,
                        format!(
                            "Grounding reference is ambiguous ({} matching symbols): {}; candidate: {}. Use the qualified name.",
                            candidates.len(),
                            r.reference,
                            example
                        ),
                    )
                    .with_symbol(&r.reference);
                    i.candidate = Some(example);
                    ctx.issues.push(i);
                }
                Ok(RefResolution::Missing) | Err(_) => {
                    reconcile(conn, doc, &mut ctx, r, &baseline);
                }
            }
        }
    }
    (issues, health)
}

fn reconcile(
    conn: &Connection,
    doc: &GroundingDoc,
    ctx: &mut Ctx,
    r: &DocRef,
    baseline: &EffectiveBaseline,
) {
    let reference = r.reference.as_str();
    let source = ctx.source.clone();
    ctx.health.missing += 1;
    if baseline.conflict {
        ctx.health.ambiguous += 1;
        ctx.issues.push(
            issue(
                codes::GROUNDING_AMBIGUOUS,
                SEVERITY_WARNING,
                &source,
                r.origin,
                format!(
                    "{} has conflicting committed fingerprints in other scaffold files: {}",
                    subject(r.origin),
                    reference
                ),
            )
            .with_symbol(reference),
        );
        return;
    }
    match reconcile_missing_ref_with(conn, doc.scaffold_rel, reference, baseline) {
        Reconciliation::Moved { proposal, evidence } => {
            ctx.health.moved += 1;
            if matches!(
                evidence,
                MoveEvidence::Neighbors | MoveEvidence::Surroundings
            ) {
                ctx.issues.push(
                    issue(
                        codes::GROUNDING_MOVED_BY_NEIGHBORS,
                        SEVERITY_INFO,
                        &source,
                        r.origin,
                        moved_by_neighbors_message(
                            r.origin,
                            reference,
                            &proposal.new_node_id,
                            evidence,
                            &proposal.reason,
                        ),
                    )
                    .with_symbol(reference),
                );
            }
            let candidate_hash: Option<String> = resolve_grounding_ref(conn, &proposal.new_node_id)
                .ok()
                .and_then(|res| res.node().and_then(|n: &Node| n.body_hash.clone()))
                .filter(|h| !h.is_empty());
            let body_changed = matches!(
                (&baseline.body_hash, &candidate_hash),
                (Some(b), Some(c)) if b != c
            );
            let message = match (r.origin, body_changed) {
                (RefOrigin::Frontmatter, false) => None,
                (RefOrigin::Frontmatter, true) => Some(format!(
                    "Grounded node moved and its body changed: {}; candidate: {} ({}, confidence {:.0}%). Review the prose, then run `knobyte sync` to rewrite the reference.",
                    reference,
                    proposal.new_node_id,
                    proposal.reason,
                    proposal.confidence * 100.0
                )),
                (RefOrigin::Anchor(_), changed) => Some(format!(
                    "Inline anchor should move{}: {}; candidate: {} ({}, confidence {:.0}%). Run `knobyte sync` (or `knobyte check --fix`) to rewrite it.",
                    if changed { " and its body changed" } else { "" },
                    reference,
                    proposal.new_node_id,
                    proposal.reason,
                    proposal.confidence * 100.0
                )),
            };
            if let Some(message) = message {
                let mut i = issue(
                    codes::GROUNDING_DRIFT,
                    SEVERITY_WARNING,
                    &source,
                    r.origin,
                    message,
                )
                .with_symbol(reference);
                i.candidate = Some(proposal.new_node_id.clone());
                ctx.issues.push(i);
            }
        }
        Reconciliation::Ambiguous(candidates) => {
            ctx.health.ambiguous += 1;
            let example = readable_ref_for(&candidates[0]);
            let mut i = issue(
                codes::GROUNDING_AMBIGUOUS,
                SEVERITY_WARNING,
                &source,
                r.origin,
                format!(
                    "{} may have moved: {}; candidate: {} ({} candidates)",
                    subject(r.origin),
                    reference,
                    example,
                    candidates.len()
                ),
            )
            .with_symbol(reference);
            i.candidate = Some(example);
            ctx.issues.push(i);
        }
        Reconciliation::Gone => {
            ctx.health.gone += 1;
            let (severity, msg) = match r.origin {
                RefOrigin::Frontmatter => (
                    SEVERITY_ERROR,
                    format!("Grounded node no longer exists: {}", reference),
                ),
                RefOrigin::Anchor(_) => (
                    SEVERITY_WARNING,
                    format!("Inline anchor points to a deleted node: {}", reference),
                ),
            };
            ctx.issues.push(
                issue(codes::GROUNDING_GONE, severity, &source, r.origin, msg)
                    .with_symbol(reference),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_refs_with_origins_and_committed_values() {
        let doc = "---\ngrounds_to:\n  - function:src/a.rs:f\n  - node_id: function:src/b.rs:g\n  - ref: function:src/c.rs:h\n    body_hash: abc\n    fingerprint: mh1:3:00\n---\n# T\n\n<!-- kb-ground: function:src/a.rs:f #def -->\n<!-- kb-ground: struct:src/c.rs:S -->\n";
        let refs = extract_doc_refs(doc);
        assert_eq!(refs.len(), 5);
        assert_eq!(refs[0].origin, RefOrigin::Frontmatter);
        assert_eq!(refs[1].reference, "function:src/b.rs:g");
        assert_eq!(refs[2].committed.body_hash.as_deref(), Some("abc"));
        assert_eq!(refs[2].committed.fingerprint.as_deref(), Some("mh1:3:00"));
        // The anchor for a frontmatter ref is its own grounding, with its own hash.
        assert_eq!(refs[3].reference, "function:src/a.rs:f");
        assert_eq!(refs[3].origin, RefOrigin::Anchor(11));
        assert_eq!(refs[3].committed.body_hash.as_deref(), Some("def"));
        assert_eq!(refs[4].origin, RefOrigin::Anchor(12));
    }
}
