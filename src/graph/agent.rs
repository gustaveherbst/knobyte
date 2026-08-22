//! Agent-facing graph commands speaking protocol v3 (see [`crate::graph::protocol`]):
//! `graph scope`, `graph query`, `graph get` and `impact`. Each returns the complete list of
//! JSONL records (meta first, summary last) so the CLI can print them and MCP can return them.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use crate::graph::engine::{GraphEngine, ImpactOptions};
use crate::graph::grounding::readable_ref_for;
use crate::graph::models::Node;
use crate::graph::read::ReadGate;
use crate::graph::protocol::{
    begin_response, estimate_tokens, summary_reserve, AgentOptions, AgentOptionsInput, DetailLevel,
    Response, SummaryFields,
};
use crate::graph::query_plan::{is_low_value_graph_path, plan_graph_query, search_components};
use crate::graph::scope::{
    call_counts, file_is_stale, node_fingerprint, node_source, parse_health, plan_file_source,
    read_lines, select_scope, EdgeInfo, RankedScopeFile, ScopeRequest, ScopeSelection, ScopedCandidate, SourceRange,
};

/// Optional providers for `graph scope` (Knobyte supersets, composed by the caller).
#[derive(Default)]
pub struct ScopeExtras<'a> {
    /// Hybrid re-rank: (node id, similarity) from Cozo vector search, best first.
    pub vector_hits: Vec<(String, f64)>,
    /// Warnings to surface in the summary (e.g. hybrid requested but unavailable).
    pub warnings: Vec<String>,
    /// `--wiki`: knowledge records for the returned node ids.
    #[allow(clippy::type_complexity)]
    pub knowledge_for: Option<&'a dyn Fn(&[String]) -> Vec<Value>>,
}

fn error_records(err: Value) -> Vec<Value> {
    vec![err]
}

/// Compact fact fields of a node for agents.
pub fn fact_fields(conn: &Connection, node: &Node, opts: &AgentOptions) -> Map<String, Value> {
    let (callers, callees) = call_counts(conn, &node.id);
    let mut m = Map::new();
    m.insert("id".into(), json!(node.id));
    m.insert("kind".into(), json!(node.kind));
    m.insert("name".into(), json!(node.name));
    if node.qualified_name != node.name {
        m.insert("qualifiedName".into(), json!(node.qualified_name));
    }
    m.insert("filePath".into(), json!(node.file_path));
    m.insert("lineStart".into(), json!(node.start_line));
    m.insert("lineEnd".into(), json!(node.end_line));
    if let Some(sig) = &node.signature {
        let compact = sig.split_whitespace().collect::<Vec<_>>().join(" ");
        let limit = if opts.detail == DetailLevel::Standard { 320 } else { 180 };
        if !compact.is_empty() {
            let s = if compact.chars().count() <= limit {
                compact
            } else {
                let mut t: String = compact.chars().take(limit - 1).collect();
                t.push('…');
                t
            };
            m.insert("signature".into(), json!(s));
        }
    }
    m.insert("callerCount".into(), json!(callers));
    m.insert("calleeCount".into(), json!(callees));
    if !node.file_path.is_empty() && !matches!(node.kind.as_str(), "module") {
        m.insert("ref".into(), json!(readable_ref_for(node)));
    }
    if opts.fingerprint {
        if let Some(h) = node.body_hash.as_ref().filter(|h| !h.is_empty()) {
            m.insert("bodyHash".into(), json!(h));
        }
        if let Some(fp) = node_fingerprint(conn, &node.id) {
            m.insert("fingerprint".into(), json!(fp));
        }
    }
    m
}

fn record(kind: &str, mut fields: Map<String, Value>) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!(kind));
    m.append(&mut fields);
    Value::Object(m)
}

fn source_record(file: &str, ranges: &[SourceRange]) -> Value {
    json!({ "type": "source", "filePath": file, "ranges": ranges })
}

fn ranges_of(record: &Value) -> Vec<SourceRange> {
    record["ranges"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some(SourceRange {
                        start_line: r["startLine"].as_i64()?,
                        end_line: r["endLine"].as_i64()?,
                        node_ids: r["nodeIds"]
                            .as_array()?
                            .iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect(),
                        content: r["content"].as_str()?.to_string(),
                        truncated: r["truncated"].as_bool().unwrap_or(false),
                        reason: match r["reason"].as_str().unwrap_or("") {
                            "whole-file" => "whole-file",
                            "complete-symbol" => "complete-symbol",
                            "query-hit" => "query-hit",
                            "callsite" => "callsite",
                            "signature" => "signature",
                            _ => "text-only",
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Shrink a source record deterministically to at most `available` tokens: drop trailing
/// ranges, then keep the longest whole-line prefix of the last range. `None` when not even
/// one line fits.
fn fit_source(record: &Value, available: usize) -> Option<Value> {
    if estimate_tokens(record) <= available {
        return Some(record.clone());
    }
    let file = record["filePath"].as_str().unwrap_or("").to_string();
    let mut ranges = ranges_of(record);
    let mut extra = record.as_object().cloned().unwrap_or_default();
    extra.remove("type");
    extra.remove("filePath");
    extra.remove("ranges");
    let build = |ranges: &[SourceRange]| {
        let mut v = source_record(&file, ranges);
        if let Some(o) = v.as_object_mut() {
            for (k, val) in &extra {
                o.insert(k.clone(), val.clone());
            }
        }
        v
    };
    while ranges.len() > 1 && estimate_tokens(&build(&ranges)) > available {
        ranges.pop();
    }
    if estimate_tokens(&build(&ranges)) <= available {
        return Some(build(&ranges));
    }
    let last = ranges.pop()?;
    let lines: Vec<&str> = last.content.lines().collect();
    // Largest prefix that fits (monotone): binary search.
    let (mut lo, mut hi) = (0usize, lines.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let mut cut = last.clone();
        cut.content = lines[..mid].join("\n");
        cut.end_line = cut.start_line + mid as i64 - 1;
        cut.truncated = true;
        let mut rs = ranges.clone();
        rs.push(cut);
        if estimate_tokens(&build(&rs)) <= available {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 {
        return None;
    }
    let mut cut = last.clone();
    cut.content = lines[..lo].join("\n");
    cut.end_line = cut.start_line + lo as i64 - 1;
    cut.truncated = true;
    ranges.push(cut);
    Some(build(&ranges))
}

fn sourced_ids(records: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in records {
        for range in ranges_of(r) {
            for id in range.node_ids {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
    }
    out
}

/// Indexed files whose content changed on disk (mtime/size shortcut, hash when they differ).
fn stale_files(conn: &Connection, root: &Path) -> Vec<String> {
    let rows: Vec<(String, String, i64, i64)> = conn
        .prepare("SELECT path, content_hash, size, modified_at FROM files ORDER BY path")
        .and_then(|mut s| {
            s.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .map(|it| it.flatten().collect())
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    for (path, hash, size, mtime) in rows {
        let full = root.join(&path);
        let Ok(meta) = std::fs::metadata(&full) else {
            out.push(path);
            continue;
        };
        let live_mtime = crate::graph::build::file_mtime_ms(&full);
        if meta.len() as i64 == size && live_mtime == mtime {
            continue;
        }
        match std::fs::read(&full) {
            Ok(bytes) if crate::graph::fingerprint::compute_file_hash(&bytes) == hash => {}
            _ => out.push(path),
        }
    }
    out
}

fn flow_record(conn: &Connection, steps: &[EdgeInfo], nodes: &HashMap<String, Node>) -> Value {
    let mut ids: Vec<String> = Vec::new();
    for s in steps {
        for id in [&s.source, &s.target] {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
    }
    let node_refs: Vec<Value> = ids
        .iter()
        .filter_map(|id| {
            let n = nodes.get(id).cloned().or_else(|| GraphEngine::node_by_id(conn, id))?;
            Some(json!({ "id": n.id, "name": n.name, "filePath": n.file_path }))
        })
        .collect();
    json!({ "type": "flow", "nodes": node_refs, "steps": steps })
}

// ---------------------------------------------------------------------------
// graph scope
// ---------------------------------------------------------------------------

const RESERVED_PRIMARY_DECLARATIONS_PER_FILE: usize = 3;
const PRIMARY_DECLARATION_ANCHORS_PER_FILE: usize = 3;
const PRIMARY_FLOW_ANCHORS_PER_FILE: usize = 3;
const SOURCE_DECLARATION_ANCHOR_LINES: i64 = 6;
/// Longest declaration admitted as one complete, atomic answer.
const COMPLETE_DECLARATION_LINES: i64 = 160;

/// Source admission phases, in priority order: complete answer declarations, one compact
/// range per file (cross-file fairness), further anchors, then optional bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Answer,
    Fairness,
    Secondary,
    Optional,
}

/// One planned source record (a file's evidence label plus its ranges).
#[derive(Debug, Clone)]
struct SourcePlan {
    file: String,
    evidence: &'static str,
    stale: bool,
    ranges: Vec<SourceRange>,
}

impl SourcePlan {
    fn record(&self, ranges: &[SourceRange]) -> Value {
        let mut rec = source_record(&self.file, ranges);
        rec["evidence"] = json!(self.evidence);
        if self.stale {
            rec["stale"] = json!(true);
        }
        rec
    }
}

#[derive(Debug, Clone)]
struct Admission {
    plan: usize,
    ranges: Vec<SourceRange>,
    phase: Phase,
    atomic: bool,
}

fn is_named_source_node(node: Option<&Node>) -> bool {
    node.is_some_and(|n| !n.name.is_empty() && !n.name.starts_with('<'))
}

fn slice_range(range: &SourceRange, start: i64, end: i64, node_ids: Vec<String>) -> SourceRange {
    let offset = (start - range.start_line).max(0) as usize;
    let count = (end - start + 1).max(0) as usize;
    SourceRange {
        start_line: start,
        end_line: end,
        node_ids,
        content: range.content.split('\n').skip(offset).take(count).collect::<Vec<_>>().join("\n"),
        truncated: range.truncated,
        reason: range.reason,
    }
}

/// The declaration's first lines inside `range` (its truthful header).
fn declaration_anchor(range: &SourceRange, node: &Node, max_lines: i64) -> Option<SourceRange> {
    let start = range.start_line.max(node.start_line);
    let end = range.end_line.min(node.end_line).min(start + max_lines.max(1) - 1);
    if end < start {
        return None;
    }
    let mut a = slice_range(range, start, end, vec![node.id.clone()]);
    a.reason = if end >= node.end_line { "complete-symbol" } else { "signature" };
    a.truncated = range.truncated || end < node.end_line;
    Some(a)
}

/// The whole declaration, when `range` carries all of it and it is short enough.
fn complete_declaration_range(range: &SourceRange, node: &Node) -> Option<SourceRange> {
    let len = node.end_line - node.start_line + 1;
    if len > COMPLETE_DECLARATION_LINES || range.start_line > node.start_line || range.end_line < node.end_line {
        return None;
    }
    let mut r = slice_range(range, node.start_line, node.end_line, vec![node.id.clone()]);
    r.reason = "complete-symbol";
    r.truncated = false;
    Some(r)
}

fn source_interval(range: &SourceRange, start: i64, end: i64, nodes: &HashMap<String, Node>) -> SourceRange {
    let ids = range
        .node_ids
        .iter()
        .filter(|id| nodes.get(*id).is_none_or(|n| n.start_line <= end && n.end_line >= start))
        .cloned()
        .collect();
    let mut r = slice_range(range, start, end, ids);
    r.truncated = true;
    r
}

/// `range` minus `intervals`, as the remaining pieces.
fn subtract_intervals(range: &SourceRange, intervals: &[(i64, i64)], nodes: &HashMap<String, Node>) -> Vec<SourceRange> {
    let mut clipped: Vec<(i64, i64)> = intervals
        .iter()
        .map(|(s, e)| (range.start_line.max(*s), range.end_line.min(*e)))
        .filter(|(s, e)| e >= s)
        .collect();
    if clipped.is_empty() {
        return vec![range.clone()];
    }
    clipped.sort();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (s, e) in clipped {
        match merged.last_mut() {
            Some(p) if s <= p.1 + 1 => p.1 = p.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    let mut pieces = Vec::new();
    let mut cursor = range.start_line;
    for (s, e) in merged {
        if cursor < s {
            pieces.push(source_interval(range, cursor, s - 1, nodes));
        }
        cursor = cursor.max(e + 1);
    }
    if cursor <= range.end_line {
        pieces.push(source_interval(range, cursor, range.end_line, nodes));
    }
    pieces
}

fn merge_admission_range(ranges: &mut Vec<SourceRange>, incoming: SourceRange) {
    match ranges.iter_mut().find(|r| r.start_line == incoming.start_line && r.end_line == incoming.end_line) {
        Some(d) => {
            for id in incoming.node_ids {
                if !d.node_ids.contains(&id) {
                    d.node_ids.push(id);
                }
            }
        }
        None => ranges.push(incoming),
    }
}

/// The planned range carrying `node`'s declaration line (starting on it, then smallest).
fn best_declaration_range_index(ranges: &[SourceRange], node: &Node) -> Option<usize> {
    ranges
        .iter()
        .enumerate()
        .filter(|(_, r)| r.node_ids.contains(&node.id) && r.start_line <= node.start_line && r.end_line >= node.start_line)
        .min_by(|(ia, a), (ib, b)| {
            (a.start_line != node.start_line)
                .cmp(&(b.start_line != node.start_line))
                .then((a.end_line - a.start_line).cmp(&(b.end_line - b.start_line)))
                .then(ia.cmp(ib))
        })
        .map(|(i, _)| i)
}

fn ranges_overlap(a: &SourceRange, b: &SourceRange) -> bool {
    a.start_line <= b.end_line && b.start_line <= a.end_line
}

fn strip_line_number(line: &str) -> &str {
    let t = line.trim_start();
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && t[digits..].starts_with(':') {
        let rest = &t[digits + 1..];
        rest.strip_prefix(' ').unwrap_or(rest)
    } else {
        line
    }
}

fn combine_ranges(a: &SourceRange, b: &SourceRange) -> SourceRange {
    let start = a.start_line.min(b.start_line);
    let end = a.end_line.max(b.end_line);
    let mut lines: HashMap<i64, String> = HashMap::new();
    for r in [a, b] {
        for (off, content) in r.content.split('\n').enumerate() {
            lines.entry(r.start_line + off as i64).or_insert_with(|| strip_line_number(content).to_string());
        }
    }
    let width = end.to_string().len();
    let content = (start..=end)
        .map(|l| format!("{:>w$}: {}", l, lines.get(&l).map(String::as_str).unwrap_or(""), w = width))
        .collect::<Vec<_>>()
        .join("\n");
    let a_contains = a.start_line <= b.start_line && a.end_line >= b.end_line;
    let b_contains = b.start_line <= a.start_line && b.end_line >= a.end_line;
    let provenance = if b_contains && !a_contains { b } else { a };
    let mut ids = a.node_ids.clone();
    for id in &b.node_ids {
        if !ids.contains(id) {
            ids.push(id.clone());
        }
    }
    SourceRange {
        start_line: start,
        end_line: end,
        node_ids: ids,
        content,
        truncated: if a_contains {
            a.truncated
        } else if b_contains {
            b.truncated
        } else {
            a.truncated || b.truncated
        },
        reason: provenance.reason,
    }
}

fn source_ranges_cover(emitted: &[SourceRange], wanted: &SourceRange) -> bool {
    let mut sorted: Vec<&SourceRange> = emitted.iter().collect();
    sorted.sort_by(|a, b| a.start_line.cmp(&b.start_line).then(a.end_line.cmp(&b.end_line)));
    let mut cursor = wanted.start_line;
    for r in sorted {
        if r.end_line < cursor {
            continue;
        }
        if r.start_line > cursor {
            return false;
        }
        cursor = cursor.max(r.end_line + 1);
        if cursor > wanted.end_line {
            return true;
        }
    }
    false
}

/// Per-file plan: compact anchors, reserved answers, direct answers and optional remainders.
struct FilePlan {
    plan: usize,
    anchors: Vec<SourceRange>,
    answers: Vec<SourceRange>,
    direct_answers: Vec<SourceRange>,
    optional: Vec<SourceRange>,
}

/// Turn each planned file into round-robin admissions: global reservations first, then the
/// strongest file's answers, round-robin answers of later files, one compact range per file,
/// further anchors and finally the optional bodies.
#[allow(clippy::too_many_arguments)]
fn prioritize_source_admissions(
    plans: &[SourcePlan],
    primary_order: &[String],
    exact_order: &HashMap<String, usize>,
    flow_answer_ids: &HashSet<String>,
    reserved_primary: &HashSet<String>,
    global_order: &HashMap<String, usize>,
    nodes: &HashMap<String, Node>,
) -> Vec<Admission> {
    let mut file_plans: Vec<FilePlan> = Vec::new();
    for (record_index, plan) in plans.iter().enumerate() {
        let ranges = &plan.ranges;
        if ranges.is_empty() {
            continue;
        }
        let whole_file = ranges.len() == 1 && ranges[0].reason == "whole-file" && ranges[0].end_line - ranges[0].start_line < 200;
        if whole_file {
            let r = &ranges[0];
            let primary = record_index == 0 || r.node_ids.iter().any(|id| reserved_primary.contains(id));
            let direct = !primary && r.node_ids.iter().any(|id| flow_answer_ids.contains(id));
            let compact_id = primary_order
                .iter()
                .find(|id| r.node_ids.contains(id) && nodes.get(*id).is_some_and(|n| n.file_path == plan.file))
                .or_else(|| r.node_ids.first());
            let compact_node = compact_id.and_then(|id| nodes.get(id));
            let compact_anchor = if !primary && !direct {
                compact_node.and_then(|n| declaration_anchor(r, n, SOURCE_DECLARATION_ANCHOR_LINES))
            } else {
                None
            };
            let optional = compact_anchor
                .as_ref()
                .map(|a| subtract_intervals(r, &[(a.start_line, a.end_line)], nodes))
                .unwrap_or_default();
            file_plans.push(FilePlan {
                plan: record_index,
                anchors: if primary || direct { Vec::new() } else { vec![compact_anchor.unwrap_or_else(|| r.clone())] },
                answers: if primary { vec![r.clone()] } else { Vec::new() },
                direct_answers: if direct { vec![r.clone()] } else { Vec::new() },
                optional,
            });
            continue;
        }
        let ordered: Vec<&String> = primary_order
            .iter()
            .filter(|id| nodes.get(*id).is_some_and(|n| n.file_path == plan.file))
            .collect();
        let (mut direct_anchors, mut flow_anchors) = (0usize, 0usize);
        let mut selected_as_flow: HashSet<String> = HashSet::new();
        let mut selected_ids: Vec<String> = Vec::new();
        for id in ordered {
            if reserved_primary.contains(id) {
                selected_ids.push(id.clone());
            } else if flow_answer_ids.contains(id) {
                flow_anchors += 1;
                if flow_anchors <= PRIMARY_FLOW_ANCHORS_PER_FILE {
                    selected_as_flow.insert(id.clone());
                    selected_ids.push(id.clone());
                }
            } else {
                direct_anchors += 1;
                if direct_anchors <= PRIMARY_DECLARATION_ANCHORS_PER_FILE {
                    selected_ids.push(id.clone());
                }
            }
        }
        let completes = |id: &String| -> bool {
            nodes.get(id).is_some_and(|n| {
                best_declaration_range_index(ranges, n).is_some_and(|i| complete_declaration_range(&ranges[i], n).is_some())
            })
        };
        let mut direct_answer_ids: HashSet<String> =
            selected_ids.iter().filter(|id| reserved_primary.contains(*id) && completes(id)).cloned().collect();
        if let Some(fallback) = selected_ids
            .iter()
            .find(|id| !reserved_primary.contains(*id) && !exact_order.contains_key(*id) && !selected_as_flow.contains(*id) && completes(id))
        {
            direct_answer_ids.insert(fallback.clone());
        }
        let mut anchors: Vec<SourceRange> = Vec::new();
        let mut answers: Vec<SourceRange> = Vec::new();
        let mut direct_answers: Vec<SourceRange> = Vec::new();
        let mut cuts: HashMap<usize, Vec<(i64, i64)>> = HashMap::new();
        for id in &selected_ids {
            let Some(node) = nodes.get(id) else { continue };
            let Some(ri) = best_declaration_range_index(ranges, node) else { continue };
            let range = &ranges[ri];
            let exact = exact_order.contains_key(id);
            let anchor = if exact {
                Some(range.clone())
            } else {
                declaration_anchor(range, node, SOURCE_DECLARATION_ANCHOR_LINES)
            };
            let Some(anchor) = anchor else { continue };
            let complete_answer =
                if !exact && selected_as_flow.contains(id) { complete_declaration_range(range, node) } else { None };
            let complete_direct =
                if direct_answer_ids.contains(id) { complete_declaration_range(range, node) } else { None };
            let reserved_anchor = (reserved_primary.contains(id) && complete_answer.is_none() && complete_direct.is_none())
                .then(|| anchor.clone());
            if complete_answer.is_none() && complete_direct.is_none() && reserved_anchor.is_none() {
                merge_admission_range(&mut anchors, anchor.clone());
            }
            if let Some(a) = &reserved_anchor {
                merge_admission_range(&mut answers, a.clone());
            }
            if let Some(a) = &complete_answer {
                if reserved_primary.contains(id) {
                    merge_admission_range(&mut answers, a.clone());
                } else {
                    merge_admission_range(&mut direct_answers, a.clone());
                }
            }
            if let Some(a) = &complete_direct {
                if reserved_primary.contains(id) || record_index == 0 {
                    merge_admission_range(&mut answers, a.clone());
                } else {
                    merge_admission_range(&mut direct_answers, a.clone());
                }
            }
            let consumed = complete_answer.or(complete_direct).or(reserved_anchor).unwrap_or(anchor);
            cuts.entry(ri).or_default().push((consumed.start_line, consumed.end_line));
        }
        // Node-less evidence takes part in the same one-first-range-per-file contract.
        if anchors.is_empty() && answers.is_empty() && direct_answers.is_empty() {
            let range = &ranges[0];
            let fallback_node = range.node_ids.iter().find_map(|id| nodes.get(id));
            let fallback = fallback_node
                .and_then(|n| declaration_anchor(range, n, SOURCE_DECLARATION_ANCHOR_LINES))
                .unwrap_or_else(|| range.clone());
            let complete = if record_index == 0 { fallback_node.and_then(|n| complete_declaration_range(range, n)) } else { None };
            let consumed = match complete {
                Some(c) => {
                    direct_answers.push(c.clone());
                    c
                }
                None => {
                    anchors.push(fallback.clone());
                    fallback
                }
            };
            cuts.insert(0, vec![(consumed.start_line, consumed.end_line)]);
        }
        let optional = ranges
            .iter()
            .enumerate()
            .flat_map(|(i, r)| subtract_intervals(r, cuts.get(&i).map(Vec::as_slice).unwrap_or(&[]), nodes))
            .collect();
        file_plans.push(FilePlan {
            plan: record_index,
            anchors,
            answers,
            direct_answers,
            optional,
        });
    }

    let mut ordered: Vec<Admission> = Vec::new();
    let single = |plan: usize, r: &SourceRange, phase: Phase, atomic: bool| Admission {
        plan,
        ranges: vec![r.clone()],
        phase,
        atomic,
    };
    // Global reservations (dominant exact lookup, terminal flow target, NL answer) first.
    let mut admitted: HashSet<(usize, usize, bool)> = HashSet::new();
    let mut global: Vec<(usize, usize, bool, usize, usize)> = Vec::new();
    for (pi, fp) in file_plans.iter().enumerate() {
        for (ri, r) in fp.answers.iter().map(|r| (r, false)).chain(fp.direct_answers.iter().map(|r| (r, true))).enumerate() {
            let priority = r.0.node_ids.iter().filter_map(|id| global_order.get(id)).min().copied();
            if let Some(p) = priority {
                let local = if r.1 { ri - fp.answers.len() } else { ri };
                global.push((p, pi, r.1, local, ri));
            }
        }
    }
    global.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.4.cmp(&b.4)));
    for (_, pi, direct, local, _) in global {
        if !admitted.insert((pi, local, direct)) {
            continue;
        }
        let fp = &file_plans[pi];
        let r = if direct { &fp.direct_answers[local] } else { &fp.answers[local] };
        ordered.push(single(fp.plan, r, Phase::Answer, true));
    }
    if let Some(fp) = file_plans.first() {
        for (i, r) in fp.answers.iter().enumerate() {
            if !admitted.contains(&(0, i, false)) {
                ordered.push(single(fp.plan, r, Phase::Answer, true));
            }
        }
        for (i, r) in fp.direct_answers.iter().enumerate() {
            if !admitted.contains(&(0, i, true)) {
                ordered.push(single(fp.plan, r, Phase::Answer, true));
            }
        }
    }
    let waves = file_plans.iter().skip(1).map(|f| f.answers.len()).max().unwrap_or(0);
    for w in 0..waves {
        for (pi, fp) in file_plans.iter().enumerate().skip(1) {
            if let Some(r) = fp.answers.get(w) {
                if !admitted.contains(&(pi, w, false)) {
                    ordered.push(single(fp.plan, r, Phase::Answer, true));
                }
            }
        }
    }
    let waves = file_plans.iter().skip(1).map(|f| f.direct_answers.len()).max().unwrap_or(0);
    for w in 0..waves {
        for (pi, fp) in file_plans.iter().enumerate().skip(1) {
            if let Some(r) = fp.direct_answers.get(w) {
                if !admitted.contains(&(pi, w, true)) {
                    ordered.push(single(fp.plan, r, Phase::Answer, true));
                }
            }
        }
    }
    for fp in &file_plans {
        if let Some(r) = fp.anchors.first() {
            let atomic = r.reason == "whole-file" && r.end_line - r.start_line < 200;
            ordered.push(single(fp.plan, r, Phase::Fairness, atomic));
        }
    }
    let waves = file_plans.iter().map(|f| f.anchors.len()).max().unwrap_or(0);
    for w in 1..waves {
        for fp in &file_plans {
            if let Some(r) = fp.anchors.get(w) {
                ordered.push(single(fp.plan, r, Phase::Secondary, false));
            }
        }
    }
    for fp in &file_plans {
        if !fp.optional.is_empty() {
            ordered.push(Admission {
                plan: fp.plan,
                ranges: fp.optional.clone(),
                phase: Phase::Optional,
                atomic: false,
            });
        }
    }
    ordered
}

/// Every source line is serialized at most once: same-phase overlaps merge, later phases lose
/// what an earlier admission already covers (keeping covered node ids for accounting).
fn coalesce_source_admissions(admissions: Vec<Admission>, plans: &[SourcePlan], nodes: &HashMap<String, Node>) -> Vec<Admission> {
    struct Entry {
        adm: Admission,
        range: SourceRange,
        order: usize,
    }
    let mut flat: Vec<Entry> = Vec::new();
    for adm in admissions {
        for r in adm.ranges.clone() {
            let order = flat.len();
            flat.push(Entry {
                adm: adm.clone(),
                range: r,
                order,
            });
        }
    }
    let mut groups: Vec<((String, Phase), Vec<Entry>)> = Vec::new();
    for e in flat {
        let key = (plans[e.adm.plan].file.clone(), e.adm.phase);
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => g.push(e),
            None => groups.push((key, vec![e])),
        }
    }
    let mut same_phase: Vec<Entry> = Vec::new();
    for (_, mut group) in groups {
        group.sort_by(|a, b| {
            a.range
                .start_line
                .cmp(&b.range.start_line)
                .then(b.range.end_line.cmp(&a.range.end_line))
                .then(a.order.cmp(&b.order))
        });
        let mut merged: Vec<Entry> = Vec::new();
        for e in group {
            match merged.last_mut() {
                Some(p) if ranges_overlap(&p.range, &e.range) => {
                    p.range = combine_ranges(&p.range, &e.range);
                    p.adm.atomic = p.adm.atomic && e.adm.atomic;
                    p.order = p.order.min(e.order);
                }
                _ => merged.push(e),
            }
        }
        same_phase.extend(merged);
    }
    same_phase.sort_by(|a, b| {
        a.order
            .cmp(&b.order)
            .then(a.range.start_line.cmp(&b.range.start_line))
            .then(a.range.end_line.cmp(&b.range.end_line))
    });
    let mut out: Vec<Entry> = Vec::new();
    for e in same_phase {
        let file = &plans[e.adm.plan].file;
        let mut pending = vec![e.range.clone()];
        for prior in out.iter_mut() {
            if &plans[prior.adm.plan].file != file {
                continue;
            }
            let mut next = Vec::new();
            for r in pending {
                if !ranges_overlap(&prior.range, &r) {
                    next.push(r);
                    continue;
                }
                if prior.range.start_line <= r.start_line && prior.range.end_line >= r.end_line {
                    for id in &r.node_ids {
                        if !prior.range.node_ids.contains(id) {
                            prior.range.node_ids.push(id.clone());
                        }
                    }
                    continue;
                }
                let covered: Vec<String> = r
                    .node_ids
                    .iter()
                    .filter(|id| {
                        nodes
                            .get(*id)
                            .is_some_and(|n| prior.range.start_line <= n.start_line && prior.range.end_line >= n.end_line)
                    })
                    .cloned()
                    .collect();
                for id in covered {
                    if !prior.range.node_ids.contains(&id) {
                        prior.range.node_ids.push(id);
                    }
                }
                next.extend(subtract_intervals(&r, &[(prior.range.start_line, prior.range.end_line)], nodes));
            }
            pending = next;
            if pending.is_empty() {
                break;
            }
        }
        for r in pending {
            let atomic = e.adm.atomic && r.start_line == e.range.start_line && r.end_line == e.range.end_line;
            out.push(Entry {
                adm: Admission {
                    atomic,
                    ..e.adm.clone()
                },
                range: r,
                order: e.order,
            });
        }
    }
    out.sort_by(|a, b| {
        a.order
            .cmp(&b.order)
            .then(a.range.start_line.cmp(&b.range.start_line))
            .then(a.range.end_line.cmp(&b.range.end_line))
    });
    out.into_iter()
        .map(|e| Admission {
            ranges: vec![e.range],
            ..e.adm
        })
        .collect()
}

/// Largest whole-line part of a single-range record that fits (centred for query windows).
fn fit_source_range(plan: &SourcePlan, range: &SourceRange, available: usize, resp: &Response) -> Option<(Value, SourceRange)> {
    if available == 0 {
        return None;
    }
    let lines: Vec<&str> = range.content.split('\n').collect();
    let centered = matches!(range.reason, "query-hit" | "callsite" | "text-only");
    let (mut lo, mut hi) = (1usize, lines.len());
    let mut best = None;
    while lo <= hi {
        let count = (lo + hi) / 2;
        let offset = if centered { (lines.len() - count) / 2 } else { 0 };
        let cut = SourceRange {
            start_line: range.start_line + offset as i64,
            end_line: range.end_line.min(range.start_line + (offset + count) as i64 - 1),
            node_ids: range.node_ids.clone(),
            content: lines[offset..offset + count].join("\n"),
            truncated: count < lines.len() || range.truncated,
            reason: range.reason,
        };
        let rec = plan.record(std::slice::from_ref(&cut));
        if estimate_tokens(&rec) <= available && resp.ledger.fits(&rec) {
            best = Some((rec, cut));
            lo = count + 1;
        } else {
            hi = count - 1;
        }
    }
    best
}

struct Admitted {
    records: Vec<Value>,
    deferred: Vec<SourceRange>,
    tokens: usize,
    trimmed: bool,
}

/// Admit `ranges` of `plan` within `available` tokens (and the ledger). An atomic admission is
/// all-or-nothing; otherwise ranges are tried one by one, complete declarations are never
/// prefix-trimmed (they wait for the spill pass) and other ranges keep their fitting prefix.
fn admit_source_within_share(resp: &mut Response, plan: &SourcePlan, ranges: &[SourceRange], available: usize, atomic: bool) -> Admitted {
    let mut out = Admitted {
        records: Vec::new(),
        deferred: Vec::new(),
        tokens: 0,
        trimmed: false,
    };
    let full = plan.record(ranges);
    let full_tokens = estimate_tokens(&full);
    if full_tokens <= available && resp.ledger.try_add(&full) {
        out.records.push(full);
        out.tokens = full_tokens;
        return out;
    }
    if atomic {
        out.deferred = ranges.to_vec();
        return out;
    }
    let mut remaining = available;
    for r in ranges {
        let single = plan.record(std::slice::from_ref(r));
        let t = estimate_tokens(&single);
        if t <= remaining && resp.ledger.fits(&single) {
            resp.ledger.try_add(&single);
            out.records.push(single);
            remaining -= t;
            out.tokens += t;
            continue;
        }
        if r.reason == "complete-symbol" && !r.truncated && r.end_line - r.start_line < COMPLETE_DECLARATION_LINES {
            out.deferred.push(r.clone());
            continue;
        }
        match fit_source_range(plan, r, remaining, resp) {
            Some((rec, cut)) => {
                let t = estimate_tokens(&rec);
                resp.ledger.try_add(&rec);
                out.records.push(rec);
                remaining = remaining.saturating_sub(t);
                out.tokens += t;
                out.trimmed |= cut.end_line < r.end_line || r.truncated;
            }
            None => out.deferred.push(r.clone()),
        }
    }
    out
}

/// One globally corroborated declaration for natural-language queries: a callsite-backed
/// semantic neighbour whose identity carries at least two query concepts.
fn best_semantic_query_source_candidate(
    task: &str,
    candidates: &[ScopedCandidate],
    files: &[RankedScopeFile],
    nodes: &HashMap<String, Node>,
) -> Option<String> {
    let plan = plan_graph_query(task);
    let mut term_concept: HashMap<String, (String, f64, bool)> = HashMap::new();
    for t in &plan.terms {
        let key = t.raw.to_lowercase();
        match term_concept.get(&t.term) {
            Some(cur) if t.weight <= cur.1 => {}
            _ => {
                term_concept.insert(t.term.clone(), (key, t.weight, t.stem));
            }
        }
    }
    let file_order: HashMap<&String, usize> = files.iter().enumerate().map(|(i, f)| (&f.file_path, i)).collect();
    let mut node_order: HashMap<&String, usize> = HashMap::new();
    for f in files {
        for (i, id) in f.node_ids.iter().enumerate() {
            node_order.insert(id, i);
        }
    }
    let semantic = ["graph:calls", "graph:instantiates", "graph:references", "graph:calls_trait_method"];
    #[derive(Debug)]
    struct Scored {
        id: String,
        concepts: usize,
        corroborated: usize,
        leaf: usize,
        literal: usize,
        weight: f64,
        kind: u8,
        bm25: bool,
        score: f64,
        file: usize,
        node: usize,
        index: usize,
    }
    let mut scored: Vec<Scored> = Vec::new();
    for (index, c) in candidates.iter().enumerate() {
        let node = nodes.get(&c.id);
        if !is_named_source_node(node) {
            continue;
        }
        let node = node.unwrap();
        let Some(&fi) = file_order.get(&node.file_path) else { continue };
        let file = &files[fi];
        if file.text_only
            || file.parse_status != "ok"
            || node.end_line - node.start_line + 1 > COMPLETE_DECLARATION_LINES
            || (!plan.asks_for_tests && is_low_value_graph_path(&node.file_path))
        {
            continue;
        }
        if !c.reasons.iter().any(|r| r == "source-region:callsite") || !c.reasons.iter().any(|r| semantic.contains(&r.as_str())) {
            continue;
        }
        let mut parts = search_components(&node.name);
        parts.extend(search_components(&node.qualified_name));
        parts.extend(search_components(node.signature.as_deref().unwrap_or("")));
        let leaf_parts = search_components(&node.name);
        let matches = |set: &HashSet<String>, term: &str, stem: bool| set.contains(term) || (stem && set.iter().any(|p| p.starts_with(term)));
        let mut corroborated: HashSet<String> = HashSet::new();
        for r in &c.reasons {
            let Some(term) = r.strip_prefix("term:") else { continue };
            if let Some((key, _, stem)) = term_concept.get(term) {
                if matches(&parts, term, *stem) {
                    corroborated.insert(key.clone());
                }
            }
        }
        if corroborated.is_empty() {
            continue;
        }
        let mut concepts: HashMap<String, (f64, bool)> = HashMap::new();
        let mut leaf: HashSet<String> = HashSet::new();
        for t in &plan.terms {
            if !matches(&parts, &t.term, t.stem) {
                continue;
            }
            let key = t.raw.to_lowercase();
            if matches(&leaf_parts, &t.term, t.stem) {
                leaf.insert(key.clone());
            }
            let literal = !t.stem;
            let replace = concepts
                .get(&key)
                .is_none_or(|cur| t.weight > cur.0 || (t.weight == cur.0 && literal));
            if replace {
                concepts.insert(key, (t.weight, literal));
            }
        }
        if concepts.len() < 2 {
            continue;
        }
        scored.push(Scored {
            id: c.id.clone(),
            concepts: concepts.len(),
            corroborated: corroborated.len(),
            leaf: leaf.len(),
            literal: concepts.values().filter(|v| v.1).count(),
            weight: concepts.values().map(|v| v.0).sum(),
            kind: if c.reasons.iter().any(|r| r == "graph:calls" || r == "graph:calls_trait_method") {
                3
            } else if c.reasons.iter().any(|r| r == "graph:instantiates") {
                2
            } else {
                1
            },
            bm25: c.reasons.iter().any(|r| r == "bm25-node"),
            score: c.score,
            file: fi,
            node: node_order.get(&c.id).copied().unwrap_or(usize::MAX),
            index,
        });
    }
    let f = |a: f64, b: f64| a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal);
    scored.sort_by(|a, b| {
        b.leaf
            .cmp(&a.leaf)
            .then(b.corroborated.cmp(&a.corroborated))
            .then(b.concepts.cmp(&a.concepts))
            .then(b.literal.cmp(&a.literal))
            .then(f(b.weight, a.weight))
            .then(b.kind.cmp(&a.kind))
            .then(b.bm25.cmp(&a.bm25))
            .then(f(b.score, a.score))
            .then(a.file.cmp(&b.file))
            .then(a.node.cmp(&b.node))
            .then(a.index.cmp(&b.index))
            .then(a.id.cmp(&b.id))
    });
    scored.into_iter().next().map(|s| s.id)
}

fn flow_edge_key(e: &EdgeInfo) -> String {
    format!("{}\0{}\0{}\0{}\0{}", e.source, e.target, e.kind, e.line.unwrap_or(-1), e.column.unwrap_or(-1))
}

/// `graph scope <task>`: broad, source-bearing retrieval in one response.
pub fn run_scope(
    engine: &GraphEngine,
    root: &Path,
    task: &str,
    input: &AgentOptionsInput,
    extras: &ScopeExtras,
) -> Vec<Value> {
    // Scope answers around changed files itself (text-only evidence); it refuses only a store
    // that cannot be read at all.
    if let Err(u) = ReadGate::inspect_tolerant(engine.db_path(), root) {
        return vec![u.record()];
    }
    let conn = engine.connection();
    let (indexed, ok_files, partial, failed) = parse_health(conn).unwrap_or_default();
    let opts = input.resolve_scope(indexed);
    let stale = stale_files(conn, root);
    let stale_set: HashSet<&String> = stale.iter().collect();
    let selection: ScopeSelection = select_scope(
        conn,
        task,
        &ScopeRequest {
            max_nodes: opts.max_nodes,
            max_files: opts.max_files,
            vector_hits: extras.vector_hits.clone(),
        },
    );
    let mut resp = match begin_response("graph scope", &opts, Some(task), &[]) {
        Ok(r) => r,
        Err(e) => return error_records(e),
    };
    let health = json!({
        "type": "health",
        "indexedFiles": indexed,
        "okFiles": ok_files,
        "partialFiles": partial,
        "failedFiles": failed,
        "staleFiles": stale,
    });
    let mut truncated = !resp.push(health);
    let nodes = &selection.nodes;

    // Flows whose every endpoint is in a current file, within the step budget (whole flows
    // only; a flow longer than seven hops or the remaining budget is skipped).
    let mut flows: Vec<Vec<EdgeInfo>> = Vec::new();
    let mut steps_left = opts.max_flow_steps;
    for f in &selection.flows {
        let trusted = f.steps.iter().all(|s| {
            [&s.source, &s.target]
                .iter()
                .all(|id| nodes.get(*id).is_some_and(|n| !stale_set.contains(&n.file_path)))
        });
        if !trusted || steps_left == 0 || f.steps.is_empty() || f.steps.len() > 7 || f.steps.len() > steps_left {
            continue;
        }
        steps_left -= f.steps.len();
        flows.push(f.steps.clone());
    }

    // Source authority order: exact lookups, the first flow's terminal target, one
    // corroborated NL declaration, then authoritative source regions and displayed flows.
    let exact_ids: Vec<String> = selection
        .candidates
        .iter()
        .filter(|c| c.reasons.iter().any(|r| r.starts_with("exact:")))
        .map(|c| c.id.clone())
        .collect();
    let exact_order: HashMap<String, usize> = exact_ids.iter().enumerate().map(|(i, id)| (id.clone(), i)).collect();
    let region_ids: HashSet<&String> = selection
        .candidates
        .iter()
        .filter(|c| c.reasons.iter().any(|r| r == "source-region" || r.starts_with("source-region:")))
        .map(|c| &c.id)
        .collect();
    let authoritative_ids: HashSet<&String> = selection
        .candidates
        .iter()
        .filter(|c| c.reasons.iter().any(|r| r == "source-region"))
        .map(|c| &c.id)
        .collect();
    let mut region_order: Vec<String> = Vec::new();
    for f in &selection.files {
        for id in &f.node_ids {
            if region_ids.contains(id) && !region_order.contains(id) {
                region_order.push(id.clone());
            }
        }
    }
    let mut flow_order: Vec<String> = Vec::new();
    for s in flows.iter().flatten() {
        for id in [&s.source, &s.target] {
            if !flow_order.contains(id) {
                flow_order.push(id.clone());
            }
        }
    }
    let first_flow_ids: Vec<String> = {
        let mut v: Vec<String> = Vec::new();
        for s in flows.first().map(|f| f.as_slice()).unwrap_or(&[]) {
            for id in [&s.source, &s.target] {
                if !v.contains(id) {
                    v.push(id.clone());
                }
            }
        }
        v
    };
    let semantic_id = best_semantic_query_source_candidate(task, &selection.candidates, &selection.files, nodes);
    let authoritative_order: Vec<String> = region_order.iter().filter(|id| authoritative_ids.contains(id)).cloned().collect();
    let mut primary_authoritative: Vec<String> = Vec::new();
    {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for id in &authoritative_order {
            let Some(n) = nodes.get(id) else { continue };
            let c = counts.entry(n.file_path.as_str()).or_insert(0);
            if *c >= RESERVED_PRIMARY_DECLARATIONS_PER_FILE {
                continue;
            }
            *c += 1;
            primary_authoritative.push(id.clone());
        }
    }
    let raw_concepts: HashSet<String> = selection.plan.terms.iter().map(|t| t.raw.to_lowercase()).collect();
    let global_exact: Vec<String> = if raw_concepts.len() <= 2 { exact_ids.iter().take(1).cloned().collect() } else { Vec::new() };
    let flow_answer_ids: HashSet<String> =
        flow_order.iter().filter(|id| is_named_source_node(nodes.get(*id))).cloned().collect();
    let healthy = |id: &String| -> bool {
        let n = nodes.get(id);
        is_named_source_node(n)
            && selection
                .files
                .iter()
                .any(|f| f.file_path == n.unwrap().file_path && !f.text_only && f.parse_status == "ok")
    };
    let first_flow_target: Option<String> =
        flows.first().and_then(|f| f.iter().rev().map(|s| s.target.clone()).find(|id| healthy(id)));
    let mut reserved_primary: HashSet<String> = exact_ids.iter().cloned().collect();
    reserved_primary.extend(first_flow_target.iter().cloned());
    reserved_primary.extend(semantic_id.iter().cloned());
    reserved_primary.extend(primary_authoritative.iter().cloned());
    let mut global_order: HashMap<String, usize> = HashMap::new();
    for id in global_exact.iter().chain(first_flow_target.iter()).chain(semantic_id.iter()) {
        let n = global_order.len();
        global_order.entry(id.clone()).or_insert(n);
    }
    let mut primary_order: Vec<String> = Vec::new();
    {
        let mut push = |id: &String| {
            if !primary_order.contains(id) {
                primary_order.push(id.clone());
            }
        };
        for id in exact_ids.iter().chain(first_flow_target.iter()).chain(semantic_id.iter()).chain(&primary_authoritative) {
            push(id);
        }
        for id in first_flow_ids.iter().filter(|id| is_named_source_node(nodes.get(*id))) {
            push(id);
        }
        for id in flow_order.iter().filter(|id| flow_answer_ids.contains(*id)) {
            push(id);
        }
        for id in authoritative_order.iter().chain(&region_order) {
            push(id);
        }
    }
    let first_in_file = |order: &[String], file: &str| -> usize {
        order
            .iter()
            .position(|id| nodes.get(id).is_some_and(|n| n.file_path == file))
            .unwrap_or(usize::MAX)
    };

    // Plan per-file source.
    let literal = selection.plan.literal_terms();
    let mut plans: Vec<SourcePlan> = Vec::new();
    let mut high_priority_files: Vec<String> = Vec::new();
    let mut expectations: HashMap<String, Vec<SourceRange>> = HashMap::new();
    if opts.detail == DetailLevel::Source {
        let mut source_files: Vec<&RankedScopeFile> = selection.files.iter().collect();
        source_files.sort_by(|a, b| {
            let delta = b.score - a.score;
            if delta.abs() > 1e-9 {
                return b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal);
            }
            first_in_file(&exact_ids, &a.file_path)
                .cmp(&first_in_file(&exact_ids, &b.file_path))
                .then(first_in_file(&flow_order, &a.file_path).cmp(&first_in_file(&flow_order, &b.file_path)))
                .then(a.file_path.cmp(&b.file_path))
        });
        let primary_graph_file = source_files
            .iter()
            .find(|f| !f.text_only && f.parse_status == "ok" && !f.node_ids.is_empty() && !stale_set.contains(&f.file_path))
            .map(|f| f.file_path.clone());
        if let Some(p) = &primary_graph_file {
            high_priority_files.push(p.clone());
        }
        for id in global_order.keys() {
            if let Some(n) = nodes.get(id) {
                if !high_priority_files.contains(&n.file_path) {
                    high_priority_files.push(n.file_path.clone());
                }
            }
        }
        let mut global_sorted: Vec<(&String, &usize)> = global_order.iter().collect();
        global_sorted.sort_by_key(|(_, i)| **i);
        for file in source_files {
            let Some(content) = read_lines(root, &file.file_path) else { continue };
            let is_stale = stale_set.contains(&file.file_path);
            let text_only = file.text_only || file.parse_status != "ok" || is_stale || selection.evidence_strength == "weak";
            let file_nodes: Vec<Node> = if file.text_only || is_stale {
                Vec::new()
            } else {
                let mut ids: Vec<&String> = primary_order
                    .iter()
                    .filter(|id| nodes.get(*id).is_some_and(|n| n.file_path == file.file_path))
                    .collect();
                for id in &file.node_ids {
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                ids.into_iter().filter_map(|id| nodes.get(id).cloned()).collect()
            };
            let callsites: Vec<i64> = flows
                .iter()
                .flatten()
                .filter(|s| nodes.get(&s.source).is_some_and(|n| n.file_path == file.file_path))
                .filter_map(|s| s.line)
                .collect();
            let budget = 400.min(opts.max_source_lines * file_nodes.len().clamp(1, 2));
            let ranges = plan_file_source(&content, &file_nodes, &file.text_hits, &literal, budget, &callsites);
            if ranges.is_empty() {
                continue;
            }
            if high_priority_files.contains(&file.file_path) {
                let mut wanted: Vec<SourceRange> = Vec::new();
                if primary_graph_file.as_deref() == Some(file.file_path.as_str()) {
                    merge_admission_range(&mut wanted, ranges[0].clone());
                }
                for (id, _) in &global_sorted {
                    let Some(n) = nodes.get(*id) else { continue };
                    if n.file_path != file.file_path {
                        continue;
                    }
                    let expected = best_declaration_range_index(&ranges, n).and_then(|i| {
                        complete_declaration_range(&ranges[i], n)
                            .or_else(|| declaration_anchor(&ranges[i], n, SOURCE_DECLARATION_ANCHOR_LINES))
                    });
                    if let Some(e) = expected {
                        merge_admission_range(&mut wanted, e);
                    }
                }
                if !wanted.is_empty() {
                    expectations.insert(file.file_path.clone(), wanted);
                }
            }
            plans.push(SourcePlan {
                file: file.file_path.clone(),
                evidence: if text_only { "text-only" } else { "graph" },
                stale: is_stale,
                ranges,
            });
        }
    }

    // Source is the high-value payload: 75% of what framing leaves, flows reserve up to 15%
    // (at least their first record), and unused capacity spills back to deferred source.
    let flow_records_planned: Vec<(Value, usize)> =
        flows.iter().map(|steps| (flow_record(conn, steps, nodes), steps.len())).collect();
    let payload = resp
        .effective_max
        .saturating_sub(resp.ledger.estimated_tokens())
        .saturating_sub(summary_reserve(&[]));
    let source_share = payload * 3 / 4;
    let flow_share = payload * 15 / 100;
    let mut flow_reserve = 0usize;
    for (i, (rec, _)) in flow_records_planned.iter().enumerate() {
        let t = estimate_tokens(rec);
        if i > 0 && flow_reserve + t > flow_share {
            break;
        }
        flow_reserve += t;
    }
    let admissions = coalesce_source_admissions(
        prioritize_source_admissions(&plans, &primary_order, &exact_order, &flow_answer_ids, &reserved_primary, &global_order, nodes),
        &plans,
        nodes,
    );
    let mut source_records: Vec<Value> = Vec::new();
    let mut source_tokens = 0usize;
    let mut deferred: Vec<(usize, Vec<SourceRange>, bool, Phase)> = Vec::new();
    let mut defer_lower = false;
    for adm in &admissions {
        let plan = &plans[adm.plan];
        if defer_lower && adm.phase != Phase::Answer {
            deferred.push((adm.plan, adm.ranges.clone(), adm.atomic, adm.phase));
            continue;
        }
        let a = admit_source_within_share(&mut resp, plan, &adm.ranges, source_share.saturating_sub(source_tokens), adm.atomic);
        source_records.extend(a.records);
        source_tokens += a.tokens;
        truncated |= a.trimmed;
        let mut unresolved = a.deferred;
        // A complete answer that only crossed the source-share boundary gets its hard-ledger
        // opportunity at once, keeping the flow reserve.
        if adm.phase == Phase::Answer && adm.atomic && !unresolved.is_empty() {
            let mut still = Vec::new();
            for r in unresolved {
                let avail = resp.ledger.available().saturating_sub(flow_reserve);
                let s = admit_source_within_share(&mut resp, plan, std::slice::from_ref(&r), avail, true);
                source_records.extend(s.records);
                source_tokens += s.tokens;
                truncated |= s.trimmed;
                still.extend(s.deferred);
            }
            unresolved = still;
        }
        if adm.phase == Phase::Answer && !unresolved.is_empty() {
            defer_lower = true;
        }
        if !unresolved.is_empty() {
            deferred.push((adm.plan, unresolved, adm.atomic, adm.phase));
        }
    }
    let mut ordinary: Vec<(usize, Vec<SourceRange>, bool)> = Vec::new();
    for (pi, ranges, atomic, phase) in deferred {
        if !atomic || phase != Phase::Answer {
            ordinary.push((pi, ranges, atomic));
            continue;
        }
        let avail = resp.ledger.available().saturating_sub(flow_reserve);
        let s = admit_source_within_share(&mut resp, &plans[pi], &ranges, avail, true);
        if s.trimmed || !s.deferred.is_empty() {
            truncated = true;
        }
        source_records.extend(s.records);
    }
    let mut flow_records: Vec<Value> = Vec::new();
    let mut returned_edges = 0;
    for (rec, n) in flow_records_planned {
        if resp.ledger.try_add(&rec) {
            returned_edges += n;
            flow_records.push(rec);
        } else {
            truncated = true;
        }
    }
    for (pi, ranges, atomic) in ordinary {
        let avail = resp.ledger.available();
        let s = admit_source_within_share(&mut resp, &plans[pi], &ranges, avail, atomic);
        if s.trimmed || !s.deferred.is_empty() {
            truncated = true;
        }
        source_records.extend(s.records);
    }
    let sourced = sourced_ids(&source_records);
    let flow_ids: HashSet<String> = flows.iter().flatten().flat_map(|s| [s.source.clone(), s.target.clone()]).collect();
    let mut facts: Vec<Value> = Vec::new();
    let mut fact_ids: Vec<String> = Vec::new();
    for c in selection.candidates.iter().chain(selection.tests.iter()) {
        if opts.detail == DetailLevel::Source && c.category != "test" && !sourced.contains(&c.id) && !flow_ids.contains(&c.id) {
            continue;
        }
        let Some(node) = nodes.get(&c.id) else { continue };
        if stale_set.contains(&node.file_path) {
            continue;
        }
        let mut f = fact_fields(conn, node, &opts);
        f.insert("score".into(), json!(c.score));
        if c.category == "test" {
            f.insert("category".into(), json!("test"));
        }
        if opts.detail == DetailLevel::Standard {
            f.insert("selectionReasons".into(), json!(c.reasons));
        }
        let rec = record("fact", f);
        if resp.ledger.try_add(&rec) {
            facts.push(rec);
            fact_ids.push(c.id.clone());
        } else {
            truncated = true;
        }
    }

    // Status, warnings and summary.
    let mut returned_files: Vec<String> = Vec::new();
    for r in &source_records {
        if let Some(p) = r["filePath"].as_str() {
            if !returned_files.iter().any(|x| x == p) {
                returned_files.push(p.to_string());
            }
        }
    }
    let mut text_fallback: Vec<String> = Vec::new();
    for r in source_records.iter().filter(|r| r["evidence"] == "text-only") {
        if let Some(p) = r["filePath"].as_str() {
            if !text_fallback.iter().any(|x| x == p) {
                text_fallback.push(p.to_string());
            }
        }
    }
    let omitted_sources: Vec<&String> = high_priority_files
        .iter()
        .filter(|p| {
            let Some(wanted) = expectations.get(*p).filter(|w| !w.is_empty()) else { return true };
            let emitted: Vec<SourceRange> =
                source_records.iter().filter(|r| r["filePath"].as_str() == Some(p.as_str())).flat_map(ranges_of).collect();
            wanted.iter().any(|w| !source_ranges_cover(&emitted, w))
        })
        .collect();
    let primary_keys: HashSet<String> = flows.first().map(|f| f.iter().map(flow_edge_key).collect()).unwrap_or_default();
    let returned_keys: HashSet<String> = flow_records
        .iter()
        .flat_map(|r| r["steps"].as_array().cloned().unwrap_or_default())
        .map(|s| {
            format!(
                "{}\0{}\0{}\0{}\0{}",
                s["source"].as_str().unwrap_or(""),
                s["target"].as_str().unwrap_or(""),
                s["kind"].as_str().unwrap_or(""),
                s["line"].as_i64().unwrap_or(-1),
                s["column"].as_i64().unwrap_or(-1)
            )
        })
        .collect();
    let omitted_flow_steps = primary_keys.iter().filter(|k| !returned_keys.contains(*k)).count();
    let high_priority_omitted = !omitted_sources.is_empty() || omitted_flow_steps > 0;
    let returned_graph_files: HashSet<&str> =
        source_records.iter().filter(|r| r["evidence"] == "graph").filter_map(|r| r["filePath"].as_str()).collect();
    let graph_primary = high_priority_files
        .iter()
        .any(|p| returned_graph_files.contains(p.as_str()) && !omitted_sources.contains(&p));
    if !text_fallback.is_empty() {
        truncated = true;
    }
    let relies_on_text = !text_fallback.is_empty() && !graph_primary;
    let mut warnings: Vec<String> = extras.warnings.clone();
    if failed > 0 {
        warnings.push(format!("{} indexed file(s) have failed structural parsing.", failed));
    }
    if partial > 0 {
        warnings.push(format!("{} indexed file(s) have partial structural parsing.", partial));
    }
    if !stale.is_empty() {
        warnings.push(format!(
            "{} indexed file(s) differ from live source; matching files use text-only evidence. Run `knobyte graph refresh`.",
            stale.len()
        ));
    }
    if high_priority_omitted {
        warnings.push(format!(
            "High-priority evidence omitted: {} source file(s), {} flow step(s).",
            omitted_sources.len(),
            omitted_flow_steps
        ));
    }
    let status = if returned_files.is_empty() && facts.is_empty() && flow_records.is_empty() {
        "no-match"
    } else if high_priority_omitted {
        "partial"
    } else if relies_on_text {
        "degraded"
    } else {
        "ok"
    };
    let suggestions = if status == "partial" {
        fact_ids
            .first()
            .map(|id| vec![format!("knobyte graph get {} --detail source", id)])
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut all_records: Vec<Value> = Vec::new();
    all_records.extend(source_records);
    all_records.extend(flow_records);
    all_records.extend(facts);
    // Grounded knowledge (`--wiki`), charged to the same ledger, appended last.
    if let Some(k) = extras.knowledge_for {
        let mut ids = sourced.clone();
        for id in &fact_ids {
            if !ids.contains(id) {
                ids.push(id.clone());
            }
        }
        for rec in k(&ids) {
            if resp.ledger.try_add(&rec) {
                all_records.push(rec);
            } else {
                truncated = true;
                break;
            }
        }
    }
    for r in all_records {
        resp.push_accounted(r);
    }
    resp.finish(SummaryFields {
        matched_nodes: selection.matched_count,
        returned_nodes: fact_ids.len(),
        returned_edges,
        truncated,
        suggested_next_commands: suggestions,
        status: Some(status),
        evidence_strength: Some(selection.evidence_strength),
        covered_terms: selection.covered_terms.clone(),
        returned_files,
        source_backed_nodes: sourced,
        text_fallback_files: text_fallback,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Shared: per-file grouped source for a node list
// ---------------------------------------------------------------------------

/// A file's live content, only when it is byte-identical to what was indexed: returned source
/// always matches the coordinates the graph holds for it.
fn read_indexed_source(conn: &Connection, root: &Path, path: &str) -> Option<String> {
    let indexed: String = conn
        .query_row("SELECT content_hash FROM files WHERE path = ?1", params![path], |r| r.get(0))
        .optional()
        .ok()
        .flatten()?;
    let bytes = std::fs::read(root.join(path)).ok()?;
    if crate::graph::fingerprint::compute_file_hash(&bytes) != indexed {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn plan_node_sources(
    resp: &mut Response,
    conn: &Connection,
    root: &Path,
    nodes: &[Node],
    opts: &AgentOptions,
) -> Vec<Value> {
    if opts.detail != DetailLevel::Source {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    for n in nodes {
        if seen.insert(n.id.clone()) && !files.contains(&n.file_path) && !n.file_path.is_empty() {
            files.push(n.file_path.clone());
        }
    }
    for f in files {
        let Some(content) = read_indexed_source(conn, root, &f) else { continue };
        let mut uniq = HashSet::new();
        let ranges: Vec<SourceRange> = nodes
            .iter()
            .filter(|n| n.file_path == f && uniq.insert(n.id.clone()))
            .filter_map(|n| node_source(&content, n, opts.max_source_lines))
            .collect();
        if ranges.is_empty() {
            continue;
        }
        let grouped = source_record(&f, &ranges);
        if resp.ledger.fits(&grouped) {
            resp.ledger.try_add(&grouped);
            out.push(grouped);
        } else {
            for r in ranges {
                let rec = source_record(&f, &[r]);
                if resp.ledger.try_add(&rec) {
                    out.push(rec);
                }
            }
        }
    }
    out
}

/// Records preceding a refusal: the drift declaration (when any), then the error.
fn refusal(gate: &ReadGate, error: Value) -> Vec<Value> {
    let mut out = gate.status_records();
    out.push(error);
    out
}

/// Admit the gate's `status` records to the ledger (they precede every data record).
fn admit_status(resp: &mut Response, gate: &ReadGate) -> (Vec<Value>, bool) {
    let mut out = Vec::new();
    let mut truncated = false;
    for r in gate.status_records() {
        if resp.ledger.try_add(&r) {
            out.push(r);
        } else {
            truncated = true;
        }
    }
    (out, truncated)
}

fn node_ref(n: &Node) -> Value {
    json!({ "id": n.id, "kind": n.kind, "name": n.name, "file": n.file_path, "line": n.start_line })
}

fn not_found(gate: &ReadGate, target: &str) -> Vec<Value> {
    let mut rec = json!({ "type": "error", "code": "TARGET_NOT_FOUND", "target": target });
    if let Some(o) = rec.as_object_mut() {
        o.extend(gate.not_found_coverage());
    }
    refusal(gate, rec)
}

// ---------------------------------------------------------------------------
// graph query
// ---------------------------------------------------------------------------

pub const QUERY_RELATIONS: [&str; 4] = ["where-defined", "who-calls", "what-calls", "who-imports"];

/// `graph query <relation> <target>`, gated on graph freshness (see [`crate::graph::read`]).
pub fn run_query(engine: &GraphEngine, root: &Path, relation: &str, target: &str, input: &AgentOptionsInput) -> Vec<Value> {
    if !QUERY_RELATIONS.contains(&relation) {
        return vec![json!({ "type": "error", "code": "INVALID_QUERY", "relation": relation, "expected": QUERY_RELATIONS })];
    }
    match ReadGate::inspect(engine.db_path(), root) {
        Ok(gate) => run_query_gated(engine, &gate, root, relation, target, input),
        Err(u) => vec![u.record()],
    }
}

/// `graph query` against an already-inspected gate.
pub fn run_query_gated(
    engine: &GraphEngine,
    gate: &ReadGate,
    root: &Path,
    relation: &str,
    target: &str,
    input: &AgentOptionsInput,
) -> Vec<Value> {
    if !QUERY_RELATIONS.contains(&relation) {
        return vec![json!({ "type": "error", "code": "INVALID_QUERY", "relation": relation, "expected": QUERY_RELATIONS })];
    }
    let opts = input.resolve();
    let conn = engine.connection();
    let _snapshot = engine.read_snapshot().ok();
    if let Err(u) = gate.verify(engine) {
        return vec![u.record()];
    }
    let defined = engine.query_where_defined(target).unwrap_or_default();
    // (queried target id, result node)
    let mut pairs: Vec<(String, Node)> = Vec::new();
    let mut excluded = 0usize;
    match relation {
        "who-imports" => {
            for n in engine.query_who_imports(target).unwrap_or_default() {
                pairs.push((target.to_string(), n));
            }
        }
        _ => {
            if defined.is_empty() {
                if relation == "who-calls" {
                    if let Some(records) = unresolved_callers(engine, gate, target, &opts) {
                        return records;
                    }
                }
                return not_found(gate, target);
            }
            let paths: Vec<&str> = defined.iter().map(|n| n.file_path.as_str()).collect();
            if let Some(err) = gate.target_drifted(target, &paths) {
                return refusal(gate, err);
            }
            let mut roots: Vec<Node> = defined.iter().filter(|n| !gate.is_drifted(&n.file_path)).cloned().collect();
            roots.sort_by(|a, b| a.id.cmp(&b.id));
            let mut seen = HashSet::new();
            for r in &roots {
                let related: Vec<Node> = match relation {
                    "where-defined" => vec![r.clone()],
                    "who-calls" => engine.callers_of(&r.id).unwrap_or_default().into_iter().map(|(n, _)| n).collect(),
                    _ => engine.callees_of(&r.id).unwrap_or_default().into_iter().map(|(n, _)| n).collect(),
                };
                for n in related {
                    if gate.is_drifted(&n.file_path) {
                        excluded += 1;
                        continue;
                    }
                    if seen.insert((r.id.clone(), n.id.clone())) {
                        pairs.push((r.id.clone(), n));
                    }
                }
            }
            if relation == "who-calls" {
                // Ambiguous call sites (several same-named targets) are reported, not linked.
                for n in engine.query_who_calls(target).unwrap_or_default() {
                    if gate.is_drifted(&n.file_path) || pairs.iter().any(|(_, p)| p.id == n.id) {
                        continue;
                    }
                    pairs.push((target.to_string(), n));
                }
            }
        }
    }
    let anticipated: Vec<String> = pairs
        .first()
        .filter(|_| opts.detail != DetailLevel::Source)
        .map(|(_, n)| vec![format!("knobyte graph get {} --detail source", n.id)])
        .unwrap_or_default();
    let mut resp = match begin_response(&format!("graph query {}", relation), &opts, None, &anticipated) {
        Ok(r) => r,
        Err(e) => return error_records(e),
    };
    let (status_records, mut truncated) = admit_status(&mut resp, gate);
    let mut returned: Vec<Node> = Vec::new();
    let mut records = Vec::new();
    for (tid, n) in &pairs {
        if returned.len() >= opts.max_nodes {
            truncated = true;
            break;
        }
        let mut f = fact_fields(conn, n, &opts);
        f.insert("relation".into(), json!(relation));
        f.insert("target".into(), json!(tid));
        let rec = record("result", f);
        if !resp.ledger.try_add(&rec) {
            truncated = true;
            break;
        }
        records.push(rec);
        returned.push(n.clone());
    }
    let sources = plan_node_sources(&mut resp, conn, root, &returned, &opts);
    let backed = sourced_ids(&sources);
    for r in status_records.into_iter().chain(records).chain(sources) {
        resp.push_accounted(r);
    }
    let suggestions = returned
        .first()
        .filter(|_| opts.detail != DetailLevel::Source)
        .map(|n| vec![format!("knobyte graph get {} --detail source", n.id)])
        .unwrap_or_default();
    let mut warnings: Vec<String> = gate.warning().into_iter().collect();
    if excluded > 0 {
        warnings.push(format!("{} result(s) located in changed files were excluded.", excluded));
    }
    resp.finish(SummaryFields {
        matched_nodes: pairs.len() + excluded,
        returned_nodes: returned.len(),
        truncated,
        suggested_next_commands: suggestions,
        status: (gate.source_drifted() && !returned.is_empty()).then_some("degraded"),
        source_backed_nodes: backed,
        warnings,
        ..Default::default()
    })
}

/// `who-calls` fallback for a name with call sites but no declaration: the call sites the
/// resolver recorded, as `unresolved-reference` records (never `result`). `None` when there
/// are none, so the caller abstains with `TARGET_NOT_FOUND`.
fn unresolved_callers(engine: &GraphEngine, gate: &ReadGate, target: &str, opts: &AgentOptions) -> Option<Vec<Value>> {
    let (matched, rows) = engine.unresolved_call_sites(target, opts.max_nodes).ok()?;
    let rows: Vec<_> = rows.into_iter().filter(|r| !gate.is_drifted(&r.file_path)).collect();
    if rows.is_empty() {
        return None;
    }
    let anticipated = vec![format!("knobyte graph get {} --detail source", rows[0].from_node_id)];
    let mut resp = match begin_response("graph query who-calls", opts, None, &anticipated) {
        Ok(r) => r,
        Err(e) => return Some(error_records(e)),
    };
    let (status_records, mut truncated) = admit_status(&mut resp, gate);
    truncated |= matched > rows.len();
    let mut records = Vec::new();
    for row in &rows {
        let mut rec = json!({
            "type": "unresolved-reference",
            "relation": "who-calls",
            "target": target,
            "name": row.name,
            "referenceKind": row.reference_kind,
            "resolution": row.status,
            "file": row.file_path,
            "line": row.line,
            "col": row.col,
            "fromNode": row.from_node_id,
        });
        if let Some(r) = &row.receiver {
            rec["receiver"] = json!(r);
        }
        if let Some(q) = &row.qualifier {
            rec["qualifier"] = json!(q);
        }
        if !resp.ledger.try_add(&rec) {
            truncated = true;
            break;
        }
        records.push(rec);
    }
    let first = records.first().and_then(|r| r["fromNode"].as_str()).map(String::from);
    for r in status_records.into_iter().chain(records) {
        resp.push_accounted(r);
    }
    Some(resp.finish(SummaryFields {
        matched_nodes: matched,
        returned_nodes: 0,
        truncated,
        status: Some("partial"),
        evidence_strength: Some("weak"),
        suggested_next_commands: first
            .map(|id| vec![format!("knobyte graph get {} --detail source", id)])
            .unwrap_or_default(),
        warnings: vec![format!(
            "No declaration named \"{}\" is indexed. {} unresolved reference(s) to that name were recorded during \
             extraction and are reported instead of resolved callers; they may be dynamically generated, defined \
             outside the indexed corpus, or ambiguous.",
            target, matched
        )],
        ..Default::default()
    }))
}

// ---------------------------------------------------------------------------
// graph get
// ---------------------------------------------------------------------------

/// `graph get <ids...>`: targeted source expansion by node id or grounding reference, gated on
/// graph freshness.
pub fn run_get(engine: &GraphEngine, root: &Path, ids: &[String], input: &AgentOptionsInput) -> Vec<Value> {
    match ReadGate::inspect(engine.db_path(), root) {
        Ok(gate) => run_get_gated(engine, &gate, root, ids, input),
        Err(u) => vec![u.record()],
    }
}

/// `graph get` against an already-inspected gate.
pub fn run_get_gated(
    engine: &GraphEngine,
    gate: &ReadGate,
    root: &Path,
    ids: &[String],
    input: &AgentOptionsInput,
) -> Vec<Value> {
    let mut opts = input.resolve();
    opts.detail = DetailLevel::Source;
    let conn = engine.connection();
    let _snapshot = engine.read_snapshot().ok();
    if let Err(u) = gate.verify(engine) {
        return vec![u.record()];
    }
    let frame_opts = AgentOptions {
        max_nodes: ids.len(),
        max_flow_steps: 0,
        ..opts.clone()
    };
    let mut resp = match begin_response("graph get", &frame_opts, None, &[]) {
        Ok(r) => r,
        Err(e) => return error_records(e),
    };
    let (status_records, mut truncated) = admit_status(&mut resp, gate);
    let mut errors = Vec::new();
    let mut nodes: Vec<Node> = Vec::new();
    for id in ids {
        let node = engine
            .get_nodes(std::slice::from_ref(id))
            .ok()
            .and_then(|v| v.into_iter().next())
            .or_else(|| engine.resolve_ref(id).ok().and_then(|r| r.node().cloned()));
        let rec = match node {
            None => json!({ "type": "error", "code": "NODE_NOT_FOUND", "id": id }),
            Some(n)
                if !n.file_path.is_empty()
                    && (gate.is_drifted(&n.file_path) || file_is_stale(conn, root, &n.file_path)) =>
            {
                json!({
                    "type": "error",
                    "code": "TARGET_SOURCE_DRIFTED",
                    "id": id,
                    "filePaths": [n.file_path],
                    "message": "The node's file changed since the last build; its indexed coordinates no longer describe it.",
                    "recoveryCommand": "knobyte graph refresh",
                })
            }
            Some(n) => {
                nodes.push(n);
                continue;
            }
        };
        if resp.ledger.try_add(&rec) {
            errors.push(rec);
        } else {
            truncated = true;
        }
    }
    let mut sources = plan_node_sources(&mut resp, conn, root, &nodes, &opts);
    let mut backed: Vec<String> = sourced_ids(&sources);
    let mut seen = HashSet::new();
    let omitted: Vec<Node> = nodes
        .iter()
        .filter(|n| !backed.contains(&n.id) && seen.insert(n.id.clone()))
        .cloned()
        .collect();
    if !omitted.is_empty() {
        truncated = true;
    }
    let mut facts = Vec::new();
    for n in &omitted {
        let rec = record("fact", fact_fields(conn, n, &opts));
        if resp.ledger.try_add(&rec) {
            facts.push(rec);
        }
    }
    let mut retry: Option<(String, usize)> = None;
    for n in &omitted {
        let Some(content) = read_indexed_source(conn, root, &n.file_path) else { continue };
        let Some(range) = node_source(&content, n, opts.max_source_lines) else { continue };
        let full = source_record(&n.file_path, &[range]);
        if retry.is_none() {
            let needed = estimate_tokens(&resp.meta) + estimate_tokens(&full) + summary_reserve(&[]);
            retry = Some((n.id.clone(), needed.max(opts.max_output_tokens + 1)));
        }
        if let Some(fitted) = fit_source(&full, resp.ledger.available()) {
            if resp.ledger.try_add(&fitted) {
                for id in sourced_ids(std::slice::from_ref(&fitted)) {
                    if !backed.contains(&id) {
                        backed.push(id);
                    }
                }
                sources.push(fitted);
            }
        }
    }
    let suggestions = retry
        .map(|(id, budget)| {
            let lines = if opts.max_source_lines == AgentOptions::default().max_source_lines {
                String::new()
            } else {
                format!(" --max-source-lines {}", opts.max_source_lines)
            };
            vec![format!("knobyte graph get {} --max-output-tokens {}{}", id, budget, lines)]
        })
        .unwrap_or_default();
    for r in status_records.into_iter().chain(errors).chain(sources).chain(facts.iter().cloned()) {
        resp.push_accounted(r);
    }
    resp.finish(SummaryFields {
        matched_nodes: ids.len(),
        returned_nodes: backed.len(),
        truncated,
        suggested_next_commands: suggestions,
        status: if !omitted.is_empty() {
            Some("partial")
        } else if backed.is_empty() {
            Some("no-match")
        } else {
            None
        },
        evidence_strength: (!facts.is_empty()).then_some("strong"),
        source_backed_nodes: backed.clone(),
        warnings: gate.warning().into_iter().collect(),
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// impact
// ---------------------------------------------------------------------------

/// Why an impact target cannot be analysed as one declaration.
pub enum ImpactTarget {
    Found(crate::graph::engine::ImpactReport),
    NotFound,
    /// Every root lies in a file that changed since the build.
    Drifted(Vec<String>),
    /// Several declarations share the name: they are not merged.
    Ambiguous(Vec<Node>),
}

/// Resolve and run an impact query under `gate`: a file path analyses every declaration of the
/// file; a name must resolve to exactly one declaration. Nodes in changed files are dropped.
pub fn gated_impact(
    engine: &GraphEngine,
    gate: &ReadGate,
    scaffold_root: &Path,
    target: &str,
    impact: ImpactOptions,
) -> rusqlite::Result<ImpactTarget> {
    // One snapshot for the whole analysis, proven to be the inspected publication.
    let _snapshot = engine.read_snapshot()?;
    if let Err(u) = gate.verify(engine) {
        return Err(crate::graph::maintenance::GraphMaintenanceError::new(&u.reason_code, u.message).into());
    }
    let roots = engine.impact_roots(target)?;
    if roots.is_empty() {
        return Ok(ImpactTarget::NotFound);
    }
    let paths: Vec<&str> = roots.iter().map(|n| n.file_path.as_str()).collect();
    if gate.target_drifted(target, &paths).is_some() {
        let mut files: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
        files.sort();
        files.dedup();
        return Ok(ImpactTarget::Drifted(files));
    }
    let is_file = roots.iter().all(|r| r.file_path == target);
    if !is_file && roots.len() > 1 {
        let mut c = roots;
        c.sort_by(|a, b| a.id.cmp(&b.id));
        return Ok(ImpactTarget::Ambiguous(c));
    }
    let mut report = engine.impact_with_groundings(target, impact, scaffold_root)?;
    report.roots.retain(|n| !gate.is_drifted(&n.file_path));
    report.impacted.retain(|e| !gate.is_drifted(&e.node.file_path));
    report.groundings.retain(|g| {
        report.roots.iter().any(|r| r.id == g.node_id) || report.impacted.iter().any(|e| e.node.id == g.node_id)
    });
    Ok(ImpactTarget::Found(report))
}

/// `impact <target>`: blast radius with groundings, budgeted, gated on graph freshness.
pub fn run_impact(
    engine: &GraphEngine,
    root: &Path,
    scaffold_root: &Path,
    target: &str,
    impact: ImpactOptions,
    input: &AgentOptionsInput,
) -> Vec<Value> {
    match ReadGate::inspect(engine.db_path(), root) {
        Ok(gate) => run_impact_gated(engine, &gate, root, scaffold_root, target, impact, input),
        Err(u) => vec![u.record()],
    }
}

/// `impact` against an already-inspected gate.
pub fn run_impact_gated(
    engine: &GraphEngine,
    gate: &ReadGate,
    root: &Path,
    scaffold_root: &Path,
    target: &str,
    impact: ImpactOptions,
    input: &AgentOptionsInput,
) -> Vec<Value> {
    let mut opts = input.resolve();
    if input.max_nodes.is_none() {
        opts.max_nodes = 50;
    }
    opts.depth = impact.depth;
    let conn = engine.connection();
    let report = match gated_impact(engine, gate, scaffold_root, target, impact) {
        Ok(ImpactTarget::Found(r)) => r,
        Ok(ImpactTarget::NotFound) => return not_found(gate, target),
        Ok(ImpactTarget::Drifted(files)) => {
            return refusal(
                gate,
                json!({
                    "type": "error",
                    "code": "TARGET_SOURCE_DRIFTED",
                    "target": target,
                    "filePaths": files,
                    "message": "The target's file changed since the last build; its indexed facts no longer describe it.",
                    "recoveryCommand": "knobyte graph refresh",
                }),
            )
        }
        Ok(ImpactTarget::Ambiguous(c)) => {
            return refusal(
                gate,
                json!({
                    "type": "error",
                    "code": "TARGET_AMBIGUOUS",
                    "target": target,
                    "candidates": c.iter().map(node_ref).collect::<Vec<_>>(),
                }),
            )
        }
        Err(e) => {
            let m = crate::graph::maintenance::graph_error(&e);
            return vec![json!({ "type": "error", "code": m.code, "message": m.message })];
        }
    };
    let mut roots = report.roots.clone();
    roots.sort_by(|a, b| a.id.cmp(&b.id));
    let anticipated: Vec<String> =
        roots.first().map(|r| vec![format!("knobyte graph get {} --detail source", r.id)]).unwrap_or_default();
    let mut resp = match begin_response("impact", &opts, None, &anticipated) {
        Ok(r) => r,
        Err(e) => return error_records(e),
    };
    let (status_records, status_truncated) = admit_status(&mut resp, gate);
    let mut truncated = report.truncated || status_truncated;
    let is_file = !roots.is_empty() && roots.iter().all(|r| r.file_path == target);
    let head = json!({ "type": "target", "targetType": if is_file { "file" } else { "symbol" }, "value": target });
    let mut head_records = Vec::new();
    if resp.ledger.try_add(&head) {
        head_records.push(head);
    } else {
        truncated = true;
    }
    let mut emitted: Vec<Node> = Vec::new();
    let mut facts = Vec::new();
    for r in &roots {
        if emitted.len() >= opts.max_nodes {
            truncated = true;
            break;
        }
        let rec = record("defines", fact_fields(conn, r, &opts));
        if !resp.ledger.try_add(&rec) {
            truncated = true;
            break;
        }
        facts.push(rec);
        emitted.push(r.clone());
    }
    // Groundings are admitted before callers: they are small and no other command returns them.
    let mut groundings = Vec::new();
    for g in &report.groundings {
        let rec = json!({ "type": "grounding", "doc": g.doc, "reference": g.reference, "node": g.node_id, "qualifiedName": g.qualified_name });
        if resp.ledger.try_add(&rec) {
            groundings.push(rec);
        } else {
            truncated = true;
        }
    }
    let kind = if impact.callers_only { "caller" } else { "dependent" };
    for e in &report.impacted {
        if emitted.len() >= opts.max_nodes {
            truncated = true;
            break;
        }
        let mut f = fact_fields(conn, &e.node, &opts);
        f.insert("depth".into(), json!(e.depth));
        f.insert("root".into(), json!(e.root));
        f.insert("via".into(), json!(e.via));
        let rec = record(kind, f);
        if !resp.ledger.try_add(&rec) {
            truncated = true;
            break;
        }
        facts.push(rec);
        emitted.push(e.node.clone());
    }
    let sources = plan_node_sources(&mut resp, conn, root, &emitted, &opts);
    let backed = sourced_ids(&sources);
    for r in status_records
        .into_iter()
        .chain(head_records)
        .chain(facts)
        .chain(sources)
        .chain(groundings)
    {
        resp.push_accounted(r);
    }
    let docs: BTreeSet<&str> = report.groundings.iter().map(|g| g.doc.as_str()).collect();
    let mut warnings: Vec<String> = gate.warning().into_iter().collect();
    if !docs.is_empty() {
        warnings.push(format!("{} grounded document(s) describe affected code.", docs.len()));
    }
    resp.finish(SummaryFields {
        matched_nodes: report.roots.len() + report.impacted.len(),
        returned_nodes: emitted.len(),
        truncated,
        suggested_next_commands: emitted
            .first()
            .map(|n| vec![format!("knobyte graph get {} --detail source", n.id)])
            .unwrap_or_default(),
        status: (gate.source_drifted() && !emitted.is_empty()).then_some("degraded"),
        source_backed_nodes: backed,
        warnings,
        ..Default::default()
    })
}

impl GraphEngine {
    /// A node by id from any connection.
    pub(crate) fn node_by_id(conn: &Connection, id: &str) -> Option<Node> {
        conn.query_row(
            &format!("SELECT {} FROM nodes WHERE id = ?1", crate::graph::engine::NODE_COLUMNS),
            params![id],
            crate::graph::engine::map_node_row,
        )
        .optional()
        .ok()
        .flatten()
    }
}
