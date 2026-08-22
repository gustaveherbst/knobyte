//! Whole-scaffold validation. Reads the Markdown directly, so it works without a built index;
//! when an index is available it is only used to detect hand edits that did not bump the
//! revision (`REVISION_DIVERGED`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::graph::grounding::{readable_ref_for, resolve_grounding_ref, RefResolution};
use crate::wiki::diagnostics::{dedupe_diagnostics, diag, sort_diagnostics, DiagExt};
use crate::wiki::index::{committed_index, doc_ref_of, grounding_health, WikiIndex};
use crate::wiki::models::{
    is_active_status, is_relation_type, is_source_type, WikiDiagnostic, WikiEntity,
    PARENT_TOPIC_RELATION,
};
use crate::wiki::parser::{is_valid_entity_id, parse_markdown_file_with, ParsedFile};
use crate::wiki::scope::WikiScope;

pub struct ValidateOptions<'a> {
    pub scope: &'a WikiScope,
    pub project_root: &'a Path,
    /// Code graph database; None (or a missing file) reports groundings as unchecked.
    pub graph_db: Option<&'a Path>,
    pub index: Option<&'a WikiIndex>,
    /// Maximum diagnostics returned (None = all).
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeverityCounts {
    pub error: usize,
    pub warning: usize,
    pub info: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationReport {
    pub files_scanned: usize,
    pub entities_checked: usize,
    pub diagnostics: Vec<WikiDiagnostic>,
    pub counts: SeverityCounts,
    pub truncated: bool,
    pub groundings_unverified: bool,
    pub code_graph_available: bool,
}

/// Parse the whole corpus of a scope. Returns parsed files and discovery diagnostics.
pub fn parse_corpus(scope: &WikiScope) -> (Vec<ParsedFile>, Vec<WikiDiagnostic>) {
    let (files, diags) = scope.discover();
    let mut parsed = Vec::new();
    for (rel, abs) in files {
        let Ok(bytes) = std::fs::read(&abs) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        parsed.push(parse_markdown_file_with(&rel, &text, &scope.registry));
    }
    (parsed, diags)
}

pub fn validate_scaffold(opts: &ValidateOptions) -> ValidationReport {
    let (files, mut out) = parse_corpus(opts.scope);
    let entities: Vec<&WikiEntity> = files
        .iter()
        .flat_map(|f| f.entities.iter().map(|e| &e.entity))
        .collect();
    for f in &files {
        out.extend(f.diagnostics.iter().cloned());
    }
    out.extend(identity_checks(&entities));
    out.extend(overlap_checks(&files));
    out.extend(relation_checks(&entities));
    out.extend(topic_checks(&entities));
    out.extend(orphan_checks(&entities));
    out.extend(source_checks(&entities, opts.project_root));

    let graph = opts
        .graph_db
        .filter(|p| p.exists())
        .and_then(|p| Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY).ok());
    let (gdiags, unverified) = grounding_checks(&entities, graph.as_ref());
    out.extend(gdiags);
    out.extend(crate::wiki::views::view_drift_diagnostics(
        opts.scope, &files,
    ));
    out.extend(crate::wiki::ops::read_audit_log(&opts.scope.scaffold_root).1);
    if let Some(index) = opts.index {
        out.extend(divergence_checks(&entities, index));
    }

    out.extend(grounding_shape_checks(&files));
    out.extend(anchor_mismatch_checks(&files));

    let mut out = dedupe_diagnostics(out);
    locate_diagnostics(&mut out, &files);
    sort_diagnostics(&mut out);
    let total = out.len();
    if let Some(limit) = opts.limit {
        out.truncate(limit);
    }
    let mut counts = SeverityCounts::default();
    for d in &out {
        match d.severity.as_str() {
            "error" => counts.error += 1,
            "warning" => counts.warning += 1,
            _ => counts.info += 1,
        }
    }
    ValidationReport {
        files_scanned: files.len(),
        entities_checked: entities.len(),
        truncated: out.len() < total,
        diagnostics: out,
        counts,
        groundings_unverified: unverified,
        code_graph_available: graph.is_some(),
    }
}

/// `GROUNDING_MIXED_SHAPE`: one file keeps its `grounds_to` in more than one shape (or a
/// `node_id:` mapping disagrees with its own reference). Shared with `knobyte check`.
fn grounding_shape_checks(files: &[ParsedFile]) -> Vec<WikiDiagnostic> {
    let mut out = Vec::new();
    for f in files {
        for issue in
            crate::drift::checkers::grounding_shape::check_grounding_shape_in(&f.text, &f.path)
        {
            let mut d = diag(&issue.code, issue.message, f.path.clone())
                .severity(&issue.severity)
                .at_path("grounds_to");
            if let Some(fe) = f.file_entity() {
                d = d.for_entity(&fe.entity.id);
            }
            out.push(d);
        }
    }
    out
}

/// Location of an anchor comment's visible text.
fn anchor_location(
    f: &ParsedFile,
    map: &crate::wiki::positions::PositionMap,
    a: &crate::wiki::markdown::Anchor,
) -> crate::wiki::positions::DiagnosticLocation {
    let content = f.text[a.start..a.end].trim_end_matches(['\n', '\r']);
    let lead = content.len() - content.trim_start().len();
    map.location(a.start + lead, a.start + content.len())
}

/// `ANCHOR_GROUNDING_MISMATCH`: an inline `kb-ground` anchor disagrees with the entity's
/// declared grounding: the same reference committed with a different baseline hash, or the
/// same symbol written at a different file path (one side relocated, the other not).
fn anchor_mismatch_checks(files: &[ParsedFile]) -> Vec<WikiDiagnostic> {
    use crate::graph::grounding::{kinds_equivalent, parse_grounding_ref, ParsedRef};
    use crate::wiki::models::GROUNDING_ORIGIN_FRONTMATTER;
    let mut out = Vec::new();
    for f in files {
        let map = crate::wiki::positions::PositionMap::new(&f.text);
        for pe in &f.entities {
            let declared: Vec<_> = pe
                .entity
                .committed_groundings
                .iter()
                .filter(|c| c.origin == GROUNDING_ORIGIN_FRONTMATTER)
                .collect();
            if declared.is_empty() {
                continue;
            }
            let anchors = f
                .doc
                .anchors
                .iter()
                .filter(|a| a.start >= pe.loc.body_start && a.start < pe.loc.body_end);
            for a in anchors {
                let mk = |msg: String| {
                    let loc = anchor_location(f, &map, a);
                    let mut d = diag("ANCHOR_GROUNDING_MISMATCH", msg, f.path.clone())
                        .for_entity(&pe.entity.id)
                        .at_line(Some(loc.start_line));
                    d.location = Some(loc);
                    d
                };
                if let Some(c) = declared.iter().find(|c| c.reference == a.reference) {
                    if let (Some(h1), Some(h2)) = (&c.body_hash, &a.body_hash) {
                        if h1 != h2 {
                            out.push(mk(format!(
                                "The anchor for '{}' commits baseline #{} but `grounds_to` commits {}",
                                a.reference, h2, h1
                            )));
                        }
                    }
                    continue;
                }
                let ParsedRef::Readable(ar) = parse_grounding_ref(&a.reference) else {
                    continue;
                };
                for c in &declared {
                    if let ParsedRef::Readable(dr) = parse_grounding_ref(&c.reference) {
                        if dr.qualified_name == ar.qualified_name
                            && kinds_equivalent(&dr.kind, &ar.kind)
                            && dr.file_path != ar.file_path
                        {
                            out.push(mk(format!(
                                "The anchor grounds '{}' but `grounds_to` names the same symbol as '{}'",
                                a.reference, c.reference
                            )));
                        }
                    }
                }
            }
        }
    }
    out
}

/// Give every diagnostic about a parsed file a precise location (key span, anchor, line or
/// heading), see [`crate::wiki::positions::locate_diagnostic`].
pub fn locate_diagnostics(diags: &mut [WikiDiagnostic], files: &[ParsedFile]) {
    use crate::wiki::models::GROUNDING_ORIGIN_ANCHOR;
    use crate::wiki::positions::{locate_diagnostic, PositionMap};
    let by_path: HashMap<&str, &ParsedFile> = files.iter().map(|f| (f.path.as_str(), f)).collect();
    let mut maps: HashMap<&str, PositionMap> = HashMap::new();
    for d in diags.iter_mut() {
        if d.location.is_some() {
            continue;
        }
        let Some(f) = by_path.get(d.file.as_str()).copied() else {
            continue;
        };
        let map = maps
            .entry(f.path.as_str())
            .or_insert_with(|| PositionMap::new(&f.text));
        let pe = d
            .entity_id
            .as_deref()
            .and_then(|id| f.entities.iter().find(|e| e.entity.id == id));
        // A grounding that came from an inline anchor points at the anchor.
        let anchor = pe.zip(d.path.as_deref()).and_then(|(pe, path)| {
            let i = path
                .strip_prefix("grounds_to[")?
                .strip_suffix(']')?
                .parse::<usize>()
                .ok()?;
            let c = pe.entity.committed_groundings.get(i)?;
            if c.origin != GROUNDING_ORIGIN_ANCHOR {
                return None;
            }
            f.doc.anchors.iter().find(|a| {
                a.reference == c.reference
                    && a.start >= pe.loc.body_start
                    && a.start < pe.loc.body_end
            })
        });
        if let Some(a) = anchor {
            let loc = anchor_location(f, map, a);
            d.line = Some(loc.start_line);
            d.location = Some(loc);
            continue;
        }
        // A relation declared under a shorthand key (`depends_on: [..]`) points at that key's
        // item, not at a `relations` list that does not exist.
        let shorthand = pe.zip(d.path.clone()).and_then(|(pe, path)| {
            let i = path
                .strip_prefix("relations[")?
                .strip_suffix(']')?
                .parse::<usize>()
                .ok()?;
            let rel = pe.entity.relations.get(i)?;
            let key = rel.origin.as_ref()?;
            let nth = pe.entity.relations[..i]
                .iter()
                .filter(|r| r.origin.as_ref() == Some(key))
                .count();
            Some(format!("{}[{}]", key, nth))
        });
        let original_path = d.path.clone();
        if shorthand.is_some() {
            d.path = shorthand;
        }
        locate_diagnostic(d, map, pe.map(|e| &e.loc));
        d.path = original_path;
    }
}

fn identity_checks(entities: &[&WikiEntity]) -> Vec<WikiDiagnostic> {
    let mut first: HashMap<&str, &str> = HashMap::new();
    let mut out = Vec::new();
    for e in entities {
        if !is_valid_entity_id(&e.id) {
            continue;
        }
        match first.get(e.id.as_str()) {
            None => {
                first.insert(&e.id, &e.file);
            }
            Some(orig) => out.push(
                diag(
                    "DUPLICATE_ENTITY_ID",
                    format!(
                        "Entity id '{}' is also defined in {}; this entity is shadowed",
                        e.id, orig
                    ),
                    e.file.clone(),
                )
                .for_entity(&e.id)
                .at_line(Some(e.start_line)),
            ),
        }
    }
    out
}

fn overlap_checks(files: &[ParsedFile]) -> Vec<WikiDiagnostic> {
    let mut out = Vec::new();
    for f in files {
        let mut ranges: Vec<(usize, usize, &str)> = f
            .entities
            .iter()
            .map(|e| {
                (
                    e.loc.metadata_start.min(e.loc.heading_start),
                    e.loc.body_end,
                    e.entity.id.as_str(),
                )
            })
            .collect();
        ranges.sort();
        for w in ranges.windows(2) {
            if w[1].0 < w[0].1 && w[1].0 != w[1].1 && w[0].0 != w[0].1 {
                out.push(
                    diag(
                        "ENTITY_RANGE_OVERLAP",
                        format!(
                            "Entities {} and {} claim overlapping regions of {}",
                            w[0].2, w[1].2, f.path
                        ),
                        f.path.clone(),
                    )
                    .for_entity(w[1].2),
                );
            }
        }
    }
    out
}

/// Every cycle in the `supersedes` graph, each reported once.
pub fn supersession_cycles(entities: &[&WikiEntity]) -> Vec<Vec<String>> {
    // Merge the edges of every claimant of an id: a duplicated id must not hide the
    // `supersedes` edges of the copy a plain map insert would overwrite.
    let mut succ: HashMap<&str, Vec<&str>> = HashMap::new();
    for e in entities {
        let edges = succ.entry(e.id.as_str()).or_default();
        for r in e.relations.iter().filter(|r| r.rel_type == "supersedes") {
            if !edges.contains(&r.target_id.as_str()) {
                edges.push(r.target_id.as_str());
            }
        }
    }
    find_cycles(entities.iter().map(|e| e.id.as_str()), &succ)
}

fn find_cycles<'a>(
    nodes: impl Iterator<Item = &'a str>,
    succ: &HashMap<&'a str, Vec<&'a str>>,
) -> Vec<Vec<String>> {
    // 0 = unvisited, 1 = in progress, 2 = done
    let mut state: HashMap<&str, u8> = HashMap::new();
    let mut cycles = Vec::new();
    let mut reported: HashSet<String> = HashSet::new();
    for start in nodes {
        if state.get(start).copied().unwrap_or(0) != 0 {
            continue;
        }
        let mut path: Vec<&str> = vec![start];
        let mut stack: Vec<(&str, usize)> = vec![(start, 0)];
        state.insert(start, 1);
        while let Some((node, idx)) = stack.last().copied() {
            let edges = succ.get(node).map(|v| v.as_slice()).unwrap_or(&[]);
            if idx >= edges.len() {
                state.insert(node, 2);
                stack.pop();
                path.pop();
                continue;
            }
            stack.last_mut().expect("non-empty").1 += 1;
            let next = edges[idx];
            match state.get(next).copied().unwrap_or(0) {
                1 => {
                    if let Some(p) = path.iter().position(|x| *x == next) {
                        let cycle: Vec<String> = path[p..].iter().map(|s| s.to_string()).collect();
                        let mut key = cycle.clone();
                        key.sort();
                        if reported.insert(key.join(">")) {
                            cycles.push(cycle);
                        }
                    }
                }
                0 => {
                    state.insert(next, 1);
                    path.push(next);
                    stack.push((next, 0));
                }
                _ => {}
            }
        }
    }
    cycles
}

fn relation_checks(entities: &[&WikiEntity]) -> Vec<WikiDiagnostic> {
    let mut by_id: HashMap<&str, &WikiEntity> = HashMap::new();
    for e in entities {
        by_id.entry(e.id.as_str()).or_insert(e);
    }
    let mut out = Vec::new();
    let mut contradictions: HashSet<String> = HashSet::new();
    for e in entities {
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for (i, r) in e.relations.iter().enumerate() {
            let path = format!("relations[{}]", i);
            let mk = |code: &str, msg: String| {
                diag(code, msg, e.file.clone())
                    .for_entity(&e.id)
                    .at_path(path.clone())
                    .at_line(Some(e.start_line))
            };
            if !is_relation_type(&r.rel_type) {
                out.push(mk(
                    "INVALID_RELATION_TYPE",
                    format!("Unknown relation type \"{}\"", r.rel_type),
                ));
                continue;
            }
            if r.target_id == e.id {
                out.push(mk(
                    "SELF_RELATION",
                    format!(
                        "Entity {} has a \"{}\" relation to itself",
                        e.id, r.rel_type
                    ),
                ));
                continue;
            }
            if !seen.insert((r.rel_type.clone(), r.target_id.clone())) {
                out.push(mk(
                    "DUPLICATE_RELATION",
                    format!("Duplicate relation \"{}\" to {}", r.rel_type, r.target_id),
                ));
                continue;
            }
            let Some(target) = by_id.get(r.target_id.as_str()) else {
                out.push(mk(
                    "INVALID_RELATION_TARGET",
                    format!(
                        "Relation '{}' points to nonexistent entity '{}'",
                        r.rel_type, r.target_id
                    ),
                ));
                continue;
            };
            if is_active_status(&e.status) && !is_active_status(&target.status) {
                out.push(mk(
                    "INACTIVE_RELATION_TARGET",
                    format!(
                        "Active entity {} has a \"{}\" relation to {} entity {}",
                        e.id, r.rel_type, target.status, target.id
                    ),
                ));
            }
            if r.rel_type == "contradicts"
                && e.status == "promoted"
                && target.status == "promoted"
                && e.entity_type == "decision"
                && target.entity_type == "decision"
                && !r.waived
            {
                let mut pair = [e.id.clone(), target.id.clone()];
                pair.sort();
                if contradictions.insert(pair.join("><")) {
                    out.push(mk(
                        "CONTRADICTORY_ACTIVE_DECISIONS",
                        format!(
                            "Promoted decisions {} and {} contradict each other",
                            e.id, target.id
                        ),
                    ));
                }
            }
        }
    }
    for cycle in supersession_cycles(entities) {
        // Locate the diagnostic at an entity that carries one of the cycle's `supersedes`
        // edges (not merely a claimant of the id), at that relation: the smallest such id,
        // with the chain rotated to start there.
        let n = cycle.len();
        let mut anchor: Option<(usize, &WikiEntity, usize)> = None;
        for (pos, id) in cycle.iter().enumerate() {
            let next = &cycle[(pos + 1) % n];
            for e in entities.iter().filter(|e| &e.id == id) {
                if let Some(ri) = e
                    .relations
                    .iter()
                    .position(|r| r.rel_type == "supersedes" && &r.target_id == next)
                {
                    let better = anchor.is_none_or(|(p, a, _)| {
                        (id.as_str(), e.file.as_str()) < (cycle[p].as_str(), a.file.as_str())
                    });
                    if better {
                        anchor = Some((pos, e, ri));
                    }
                }
            }
        }
        let (start, located) = match anchor {
            Some((pos, e, ri)) => (pos, Some((e, ri))),
            None => (0, None),
        };
        let mut chain: Vec<String> = (0..n).map(|i| cycle[(start + i) % n].clone()).collect();
        chain.push(chain[0].clone());
        let d = diag(
            "SUPERSESSION_CYCLE",
            format!("Supersession cycle: {}", chain.join(" -> ")),
            located
                .map(|(e, _)| e.file.clone())
                .or_else(|| by_id.get(chain[0].as_str()).map(|e| e.file.clone()))
                .unwrap_or_default(),
        )
        .for_entity(&chain[0]);
        out.push(match located {
            Some((e, ri)) => d
                .at_path(format!("relations[{}]", ri))
                .at_line(Some(e.start_line)),
            None => d.at_path("relations"),
        });
    }
    out
}

fn normalize_name(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Topic lookup: ids plus normalized titles and aliases.
pub struct TopicIndex<'a> {
    pub by_id: HashMap<&'a str, &'a WikiEntity>,
    pub by_name: HashMap<String, Vec<&'a str>>,
}

pub enum TopicResolution {
    Resolved(String),
    Unknown,
    Ambiguous(Vec<String>),
}

impl<'a> TopicIndex<'a> {
    pub fn build(entities: &[&'a WikiEntity]) -> Self {
        let mut by_id = HashMap::new();
        let mut by_name: HashMap<String, Vec<&'a str>> = HashMap::new();
        for e in entities.iter().filter(|e| e.entity_type == "topic") {
            by_id.entry(e.id.as_str()).or_insert(*e);
            for n in std::iter::once(&e.title).chain(e.aliases.iter()) {
                let k = normalize_name(n);
                if k.is_empty() {
                    continue;
                }
                let v = by_name.entry(k).or_default();
                if !v.contains(&e.id.as_str()) {
                    v.push(e.id.as_str());
                }
            }
        }
        Self { by_id, by_name }
    }

    pub fn resolve(&self, reference: &str) -> TopicResolution {
        if self.by_id.contains_key(reference) {
            return TopicResolution::Resolved(reference.to_string());
        }
        match self.by_name.get(&normalize_name(reference)) {
            Some(v) if v.len() == 1 => TopicResolution::Resolved(v[0].to_string()),
            Some(v) if v.len() > 1 => {
                let mut c: Vec<String> = v.iter().map(|s| s.to_string()).collect();
                c.sort();
                TopicResolution::Ambiguous(c)
            }
            _ => TopicResolution::Unknown,
        }
    }
}

fn topic_checks(entities: &[&WikiEntity]) -> Vec<WikiDiagnostic> {
    let topics = TopicIndex::build(entities);
    let by_id: HashMap<&str, &WikiEntity> = entities.iter().map(|e| (e.id.as_str(), *e)).collect();
    let mut out = Vec::new();
    for e in entities {
        for (i, t) in e.topics.iter().enumerate() {
            let path = format!("topics[{}]", i);
            match topics.resolve(t) {
                TopicResolution::Resolved(_) => {}
                TopicResolution::Ambiguous(c) => out.push(
                    diag(
                        "AMBIGUOUS_TOPIC_REFERENCE",
                        format!(
                            "\"{}\" matches several topics ({}). Use the topic id",
                            t,
                            c.join(", ")
                        ),
                        e.file.clone(),
                    )
                    .for_entity(&e.id)
                    .at_path(path),
                ),
                TopicResolution::Unknown => {
                    if let Some(other) = by_id.get(t.as_str()) {
                        out.push(
                            diag(
                                "INVALID_TOPIC_MEMBER",
                                format!(
                                    "Topic membership points at {}, which is a \"{}\", not a topic",
                                    t, other.entity_type
                                ),
                                e.file.clone(),
                            )
                            .for_entity(&e.id)
                            .at_path(path),
                        );
                    } else {
                        out.push(
                            diag(
                                "UNKNOWN_TOPIC",
                                format!("No topic matches \"{}\"", t),
                                e.file.clone(),
                            )
                            .for_entity(&e.id)
                            .at_path(path),
                        );
                    }
                }
            }
        }
    }
    // Parent hierarchy must be acyclic.
    let parents: HashMap<&str, Vec<&str>> = topics
        .by_id
        .iter()
        .map(|(id, t)| {
            (
                *id,
                t.relations
                    .iter()
                    .filter(|r| {
                        r.rel_type == PARENT_TOPIC_RELATION
                            && topics.by_id.contains_key(r.target_id.as_str())
                    })
                    .map(|r| r.target_id.as_str())
                    .collect(),
            )
        })
        .collect();
    let mut ids: Vec<&str> = topics.by_id.keys().copied().collect();
    ids.sort();
    for cycle in find_cycles(ids.into_iter(), &parents) {
        let file = topics
            .by_id
            .get(cycle[0].as_str())
            .map(|e| e.file.clone())
            .unwrap_or_default();
        let mut chain = cycle.clone();
        chain.push(cycle[0].clone());
        out.push(
            diag(
                "TOPIC_CYCLE",
                format!("Topic hierarchy cycle: {}", chain.join(" -> ")),
                file,
            )
            .for_entity(&cycle[0])
            .at_path("relations"),
        );
    }
    out
}

fn orphan_checks(entities: &[&WikiEntity]) -> Vec<WikiDiagnostic> {
    let mut targeted: HashSet<&str> = HashSet::new();
    let topics = TopicIndex::build(entities);
    for e in entities {
        for r in &e.relations {
            targeted.insert(r.target_id.as_str());
        }
        for t in &e.topics {
            if let TopicResolution::Resolved(id) = topics.resolve(t) {
                if let Some((k, _)) = topics.by_id.get_key_value(id.as_str()) {
                    targeted.insert(k);
                }
            }
            targeted.insert(t.as_str());
        }
    }
    entities
        .iter()
        .filter(|e| {
            e.status == "promoted"
                && e.entity_type != "topic"
                && e.metadata_kind != "implicit"
                && e.relations.is_empty()
                && e.topics.is_empty()
                && !targeted.contains(e.id.as_str())
        })
        .map(|e| {
            diag(
                "ORPHANED_ENTITY",
                format!(
                    "{} ({}) is promoted but relates to nothing and nothing relates to it",
                    e.title, e.id
                ),
                e.file.clone(),
            )
            .for_entity(&e.id)
        })
        .collect()
}

fn commit_re() -> Regex {
    Regex::new(r"^[0-9a-fA-F]{7,40}$").expect("static regex")
}

/// Per-kind source checks for one entity's sources.
pub fn source_diagnostics(e: &WikiEntity, project_root: Option<&Path>) -> Vec<WikiDiagnostic> {
    let commit = commit_re();
    let mut out = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (i, s) in e.sources.iter().enumerate() {
        let path = format!("sources[{}]", i);
        let mk = |code: &str, msg: String| {
            diag(code, msg, e.file.clone())
                .for_entity(&e.id)
                .at_path(path.clone())
        };
        let r = s
            .reference
            .as_deref()
            .map(str::trim)
            .filter(|r| !r.is_empty());
        if !is_source_type(&s.source_type) {
            out.push(mk(
                "MALFORMED_SOURCE",
                format!("Unknown source type \"{}\"", s.source_type),
            ));
            continue;
        }
        match s.source_type.as_str() {
            "commit" => match s.commit.as_deref().or(r) {
                None => out.push(mk(
                    "MALFORMED_SOURCE",
                    "A \"commit\" source requires a commit SHA".into(),
                )),
                Some(sha) if !commit.is_match(sha) => out.push(mk(
                    "INVALID_COMMIT_FORMAT",
                    format!(
                        "\"{}\" is not a hexadecimal commit SHA of 7-40 characters",
                        sha
                    ),
                )),
                _ => {}
            },
            "manual" => {
                if s.note.as_deref().map(str::trim).unwrap_or("").is_empty() {
                    out.push(mk(
                        "MALFORMED_SOURCE",
                        "A \"manual\" source requires a note; the note is the evidence".into(),
                    ));
                }
            }
            "url" => match r {
                None => out.push(mk(
                    "MALFORMED_SOURCE",
                    "A \"url\" source requires a URL in `ref`".into(),
                )),
                Some(u) => {
                    let ok = u.split_once("://").is_some_and(|(scheme, rest)| {
                        !scheme.is_empty()
                            && scheme
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
                            && !rest.is_empty()
                    });
                    if !ok {
                        out.push(mk(
                            "MALFORMED_SOURCE",
                            format!("\"{}\" is not a parseable URL", u),
                        ));
                    }
                }
            },
            "file" | "test" | "symbol" | "document" | "agent_session" | "issue"
            | "pull_request"
                if r.is_none() =>
            {
                out.push(mk(
                    "MALFORMED_SOURCE",
                    format!("A \"{}\" source requires `ref`", s.source_type),
                ));
            }
            _ => {}
        }
        if s.source_type != "commit" {
            if let Some(c) = s.commit.as_deref() {
                if !commit.is_match(c) {
                    out.push(mk(
                        "INVALID_COMMIT_FORMAT",
                        format!(
                            "\"{}\" is not a hexadecimal commit SHA of 7-40 characters",
                            c
                        ),
                    ));
                }
            }
        }
        if let Some(at) = s.captured_at.as_deref() {
            if chrono::DateTime::parse_from_rfc3339(at).is_err()
                && chrono::NaiveDate::parse_from_str(at, "%Y-%m-%d").is_err()
            {
                out.push(mk(
                    "MALFORMED_SOURCE",
                    format!("\"{}\" is not an ISO 8601 timestamp", at),
                ));
            }
        }
        if !seen.insert(s.identity()) {
            out.push(mk(
                "DUPLICATE_SOURCE",
                format!("Duplicate {} evidence", s.source_type),
            ));
        }
        if matches!(s.source_type.as_str(), "url" | "issue" | "pull_request") {
            out.push(mk(
                "UNRESOLVED_EXTERNAL_SOURCE",
                format!(
                    "External {} evidence {} has not been resolved",
                    s.source_type,
                    r.unwrap_or("(no ref)")
                ),
            ));
        }
        if let (Some(root), Some(r)) = (project_root, r) {
            if matches!(s.source_type.as_str(), "file" | "test") {
                let rel = r.split('#').next().unwrap_or(r);
                let escapes = rel.starts_with('/') || rel.split('/').any(|seg| seg == "..");
                if escapes {
                    out.push(mk(
                        "MALFORMED_SOURCE",
                        format!("Source ref \"{}\" resolves outside the project", r),
                    ));
                } else if !root.join(rel).exists() {
                    out.push(mk(
                        "SOURCE_FILE_MISSING",
                        format!(
                            "{}, cited as {} evidence, is not in this checkout",
                            r, s.source_type
                        ),
                    ));
                }
            }
        }
    }
    out
}

fn source_checks(entities: &[&WikiEntity], project_root: &Path) -> Vec<WikiDiagnostic> {
    entities
        .iter()
        .flat_map(|e| source_diagnostics(e, Some(project_root)))
        .collect()
}

fn grounding_checks(
    entities: &[&WikiEntity],
    graph: Option<&Connection>,
) -> (Vec<WikiDiagnostic>, bool) {
    let mut out = Vec::new();
    let total: usize = entities.iter().map(|e| e.grounds_to.len()).sum();
    let Some(gc) = graph else {
        if total > 0 {
            out.push(diag(
                "GROUNDINGS_UNCHECKED",
                format!(
                    "{} grounding(s) not checked: code graph not built (run 'knobyte graph rebuild')",
                    total
                ),
                "graph.db",
            ));
        }
        return (out, total > 0);
    };
    let committed = committed_index(entities.iter().copied());
    for e in entities {
        for (i, reference) in e.grounds_to.iter().enumerate() {
            let path = format!("grounds_to[{}]", i);
            let mk = |code: &str, msg: String| {
                diag(code, msg, e.file.clone())
                    .for_entity(&e.id)
                    .at_path(path.clone())
                    .at_line(Some(e.start_line))
            };
            let (health, state) =
                grounding_health(Some(gc), &e.file, &doc_ref_of(e, reference), &committed);
            match (health.as_str(), state.as_str()) {
                ("ambiguous", _) => {
                    let example = match resolve_grounding_ref(gc, reference) {
                        Ok(RefResolution::Ambiguous(c)) if !c.is_empty() => {
                            format!("{} symbols; qualify it, e.g. '{}'", c.len(), readable_ref_for(&c[0]))
                        }
                        _ => "several symbols".to_string(),
                    };
                    out.push(mk(
                        "AMBIGUOUS_GROUNDING",
                        format!("Grounding '{}' matches {}", reference, example),
                    ));
                }
                ("missing", "missing") => out.push(mk(
                    "GROUNDING_MISSING",
                    format!(
                        "Grounding '{}' no longer exists in the code graph (run 'knobyte sync' if it moved)",
                        reference
                    ),
                )),
                ("missing", _) => out.push(mk(
                    "GROUNDING_UNRESOLVED",
                    format!(
                        "Grounding '{}' does not resolve to a code symbol (run 'knobyte sync' if it moved)",
                        reference
                    ),
                )),
                ("changed", _) => out.push(mk(
                    "GROUNDING_STALE",
                    format!(
                        "The code under '{}' changed since {} was grounded to it",
                        reference, e.id
                    ),
                )),
                _ => {}
            }
        }
    }
    (out, false)
}

fn divergence_checks(entities: &[&WikiEntity], index: &WikiIndex) -> Vec<WikiDiagnostic> {
    let mut out = Vec::new();
    for e in entities {
        if let Ok(Some((rev, hash))) = index.revision_mark(&e.id) {
            if rev == e.revision && hash != e.content_hash {
                out.push(
                    diag(
                        "REVISION_DIVERGED",
                        format!(
                            "Entity {} was edited by hand since revision {} was recorded",
                            e.id, e.revision
                        ),
                        e.file.clone(),
                    )
                    .for_entity(&e.id),
                );
            }
        }
    }
    out
}

/// Convenience: validate the scaffold at `scaffold_root` with its configured scope.
pub fn validate_path(
    scaffold_root: &Path,
    graph_db: Option<&Path>,
    index: Option<&WikiIndex>,
    limit: Option<usize>,
) -> ValidationReport {
    let scope = WikiScope::load(scaffold_root);
    let project_root: PathBuf = scaffold_root
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| scaffold_root.to_path_buf());
    validate_scaffold(&ValidateOptions {
        scope: &scope,
        project_root: &project_root,
        graph_db,
        index,
        limit,
    })
}
