//! Golden scope ranking tests. Each case is a reference scope test case
//! (`src/graph/__tests__/scope.test.ts`): the same fixture graph, served through the
//! [`ScopeGraph`] trait, and the same expected ranking / admission outcome.

use knobyte::graph::chunks::ChunkHit;
use knobyte::graph::models::Node;
use knobyte::graph::scope::{
    plan_file_source, select_scope_in, EdgeInfo, ScopeFlow, ScopeGraph, ScopeRequest, ScopeSelection,
};
use std::cell::RefCell;
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Fixture graph
// ---------------------------------------------------------------------------

fn node(id: &str, name: &str, line: i64) -> Node {
    Node {
        id: id.to_string(),
        kind: "function".to_string(),
        name: name.to_string(),
        qualified_name: format!("module.{}", name),
        container_id: None,
        identity_key: id.to_string(),
        file_path: "src/sample.ts".to_string(),
        language: "typescript".to_string(),
        start_line: line,
        end_line: line,
        start_column: 0,
        end_column: 1,
        docstring: None,
        signature: None,
        visibility: None,
        is_exported: false,
        is_async: false,
        is_static: false,
        is_abstract: false,
        return_type: None,
        body_hash: None,
        updated_at: 1,
    }
}

fn with(mut n: Node, f: impl FnOnce(&mut Node)) -> Node {
    f(&mut n);
    n
}

fn in_file(n: Node, file: &str) -> Node {
    with(n, |n| n.file_path = file.to_string())
}

fn span(n: Node, start: i64, end: i64) -> Node {
    with(n, |n| {
        n.start_line = start;
        n.end_line = end;
    })
}

fn exported(n: Node) -> Node {
    with(n, |n| n.is_exported = true)
}

fn calls(source: &Node, target: &Node) -> EdgeInfo {
    EdgeInfo::new(&source.id, &target.id, "calls")
}

fn contains(source: &Node, target: &Node) -> EdgeInfo {
    EdgeInfo::new(&source.id, &target.id, "contains")
}

fn hit(file: &str, start: i64, end: i64, rank: f64, terms: &[&str], ids: &[&str]) -> ChunkHit {
    ChunkHit {
        file_path: file.to_string(),
        start_line: start,
        end_line: end,
        rank,
        matched_terms: terms.iter().map(|s| s.to_string()).collect(),
        node_ids: ids.iter().map(|s| s.to_string()).collect(),
    }
}

type Search = Box<dyn Fn(&str) -> Vec<Node>>;

/// A mock graph: node lookups, lexical search, edge lists per direction, indexed files and
/// source hits, with a log of every call.
struct G {
    nodes: Vec<Node>,
    search: Search,
    /// Edges served by `outgoing` / `incoming`.
    out_edges: Vec<EdgeInfo>,
    in_edges: Vec<EdgeInfo>,
    /// Filter adjacency by the requested kinds (the reference mocks mostly ignore them).
    honor_kinds: bool,
    files: Vec<(String, String)>,
    source: Vec<ChunkHit>,
    log: RefCell<Vec<String>>,
}

impl G {
    fn new(nodes: Vec<Node>) -> Self {
        Self {
            nodes,
            search: Box::new(|_| Vec::new()),
            out_edges: Vec::new(),
            in_edges: Vec::new(),
            honor_kinds: false,
            files: Vec::new(),
            source: Vec::new(),
            log: RefCell::new(Vec::new()),
        }
    }

    fn search(mut self, f: impl Fn(&str) -> Vec<Node> + 'static) -> Self {
        self.search = Box::new(f);
        self
    }

    fn search_all(self, results: Vec<Node>) -> Self {
        self.search(move |_| results.clone())
    }

    fn edges(mut self, edges: Vec<EdgeInfo>) -> Self {
        self.out_edges = edges.clone();
        self.in_edges = edges;
        self
    }

    fn outgoing_only(mut self, edges: Vec<EdgeInfo>) -> Self {
        self.out_edges = edges;
        self
    }

    fn honoring_kinds(mut self) -> Self {
        self.honor_kinds = true;
        self
    }

    /// Every node's file indexed with parse status `ok`.
    fn indexed(mut self) -> Self {
        let mut seen = Vec::new();
        for n in &self.nodes {
            if !seen.iter().any(|(p, _): &(String, String)| p == &n.file_path) {
                seen.push((n.file_path.clone(), "ok".to_string()));
            }
        }
        self.files = seen;
        self
    }

    fn files(mut self, files: &[(&str, &str)]) -> Self {
        self.files = files.iter().map(|(p, s)| (p.to_string(), s.to_string())).collect();
        self
    }

    fn source(mut self, hits: Vec<ChunkHit>) -> Self {
        self.source = hits;
        self
    }

    fn find(&self, id: &str) -> Option<Node> {
        self.nodes.iter().find(|n| n.id == id).cloned()
    }

    fn calls_logged(&self, prefix: &str) -> Vec<String> {
        self.log.borrow().iter().filter(|l| l.starts_with(prefix)).cloned().collect()
    }
}

impl ScopeGraph for G {
    fn search_nodes(&self, query: &str, _limit: usize) -> Vec<Node> {
        self.log.borrow_mut().push(format!("search:{}", query));
        (self.search)(query)
    }

    fn search_source(&self, _query: &str, _terms: &[(String, f64, bool)], _limit: usize) -> Vec<ChunkHit> {
        self.source.clone()
    }

    fn node(&self, id: &str) -> Option<Node> {
        self.log.borrow_mut().push(format!("node:{}", id));
        self.find(id)
    }

    fn incoming(&self, id: &str, kinds: &[&str]) -> Vec<(Node, EdgeInfo)> {
        self.log.borrow_mut().push(format!("in:{}:{}", id, kinds.join(",")));
        self.in_edges
            .iter()
            .filter(|e| e.target == id && (!self.honor_kinds || kinds.contains(&e.kind.as_str())))
            .filter_map(|e| Some((self.find(&e.source)?, e.clone())))
            .collect()
    }

    fn outgoing(&self, id: &str, kinds: &[&str]) -> Vec<(Node, EdgeInfo)> {
        self.log.borrow_mut().push(format!("out:{}:{}", id, kinds.join(",")));
        self.out_edges
            .iter()
            .filter(|e| e.source == id && (!self.honor_kinds || kinds.contains(&e.kind.as_str())))
            .filter_map(|e| Some((self.find(&e.target)?, e.clone())))
            .collect()
    }

    fn indexed_files(&self) -> Vec<(String, String)> {
        self.files.clone()
    }
}

fn select(g: &G, task: &str, max_nodes: usize, max_files: usize) -> ScopeSelection {
    select_scope_in(
        g,
        task,
        &ScopeRequest {
            max_nodes,
            max_files,
            vector_hits: Vec::new(),
        },
    )
}

fn ids(sel: &ScopeSelection) -> Vec<String> {
    sel.candidates.iter().map(|c| c.id.clone()).collect()
}

fn paths(sel: &ScopeSelection) -> Vec<String> {
    sel.files.iter().map(|f| f.file_path.clone()).collect()
}

fn reasons_of(sel: &ScopeSelection, id: &str) -> Vec<String> {
    sel.candidates.iter().find(|c| c.id == id).map(|c| c.reasons.clone()).unwrap_or_default()
}

fn all_steps(sel: &ScopeSelection) -> Vec<EdgeInfo> {
    sel.flows.iter().flat_map(|f| f.steps.clone()).collect()
}

fn first_flow(sel: &ScopeSelection) -> Vec<EdgeInfo> {
    sel.flows.first().map(|f| f.steps.clone()).unwrap_or_default()
}

fn flow_shape_ok(sel: &ScopeSelection) {
    assert!(all_steps(sel).len() <= 8, "{:?}", sel.flows);
    for f in &sel.flows {
        assert!(f.steps.len() <= 7);
        for w in f.steps.windows(2) {
            assert_eq!(w[0].target, w[1].source, "contiguous: {:?}", f);
        }
    }
}

/// Reference `fixture()`: seed with a caller, a callee and a low-confidence callee.
fn seed_fixture() -> G {
    let seed = with(node("function:seed", "seed", 2), |n| {
        n.signature = Some("function seed(): string".into());
        n.docstring = Some("Seed docs".into());
    });
    let caller = node("function:caller", "caller", 1);
    let callee = node("function:callee", "callee", 3);
    let ambiguous = node("function:ambiguous", "ambiguous", 4);
    let incoming = calls(&caller, &seed).at(8, 2);
    let outgoing = calls(&seed, &callee).at(3, 2);
    let low = calls(&seed, &ambiguous).at(4, 2).with_confidence(0.75);
    let mut g = G::new(vec![seed.clone(), caller, callee, ambiguous]).search_all(vec![seed]);
    g.in_edges = vec![incoming];
    g.out_edges = vec![outgoing, low];
    g
}

// ---------------------------------------------------------------------------
// query-time graph scope
// ---------------------------------------------------------------------------

#[test]
fn pins_an_explicit_seed_and_expands_two_hops_only_through_reliable_typed_edges() {
    let g = seed_fixture();
    let sel = select(&g, "Seed task", 24, 6);
    let got = ids(&sel);
    assert_eq!(got[0], "function:seed");
    let rest: HashSet<&str> = got[1..].iter().map(String::as_str).collect();
    assert_eq!(rest, ["function:caller", "function:callee"].into_iter().collect());
    assert!(!got.contains(&"function:ambiguous".to_string()));
    assert!(g.calls_logged("search:").contains(&"search:Seed task".to_string()));
}

#[test]
fn ranks_an_exact_identifier_match_first_with_reasons_and_category() {
    let g = seed_fixture();
    let sel = select(&g, "Seed task", 10, 6);
    assert_eq!(sel.matched_count, 3);
    assert_eq!(sel.candidates[0].id, "function:seed");
    assert_eq!(sel.candidates[0].category, "direct");
    assert!(sel.candidates[0].score > sel.candidates[1].score);
    for r in ["bm25-node", "exact:Seed", "term:seed"] {
        assert!(sel.candidates[0].reasons.contains(&r.to_string()), "{:?}", sel.candidates[0].reasons);
    }
    let mut neighbors: Vec<&str> =
        sel.candidates.iter().filter(|c| c.category == "neighbor").map(|c| c.id.as_str()).collect();
    neighbors.sort();
    assert_eq!(neighbors, vec!["function:callee", "function:caller"]);
    assert_eq!(sel.flows.len(), 1);
    let step = &sel.flows[0].steps;
    assert_eq!(step.len(), 1);
    assert_eq!((step[0].source.as_str(), step[0].target.as_str(), step[0].kind.as_str()), ("function:seed", "function:callee", "calls"));
    assert_eq!(step[0].confidence, 1.0);
}

#[test]
fn caps_returned_candidates_at_max_nodes_while_reporting_the_full_match_count() {
    let g = seed_fixture();
    let sel = select(&g, "Seed", 1, 6);
    assert_eq!(ids(&sel), vec!["function:seed"]);
    assert!(sel.matched_count > 1);
}

#[test]
fn does_not_expand_or_display_flows_from_an_incidental_bm25_only_hit() {
    let g = seed_fixture().files(&[("src/sample.ts", "ok")]);
    let sel = select(&g, "documentation architecture topic", 10, 6);
    assert!(sel.flows.is_empty());
    assert_eq!(sel.files[0].file_path, "src/sample.ts");
    assert!(sel.files[0].text_only);
}

#[test]
fn routes_test_file_nodes_into_the_test_quota_bucket() {
    let t = with(in_file(node("function:seed", "seed", 2), "src/__tests__/seed.test.ts"), |n| {
        n.signature = Some("s".into())
    });
    let g = G::new(vec![t.clone()]).search_all(vec![t]);
    let sel = select(&g, "seed", 10, 6);
    assert_eq!(sel.candidates[0].category, "test");
}

// ---------------------------------------------------------------------------
// source range planning
// ---------------------------------------------------------------------------

fn numbered_file(lines: usize, edits: &[(usize, &str)]) -> String {
    let mut v: Vec<String> = (1..=lines).map(|i| format!("line {}", i)).collect();
    for (i, s) in edits {
        v[*i] = s.to_string();
    }
    v.join("\n")
}

#[test]
fn preserves_caller_provided_node_priority_instead_of_source_line_order() {
    let content = numbered_file(260, &[]);
    let named = span(node("function:named", "NamedTarget", 200), 200, 205);
    let neighbor = span(node("function:neighbor", "neighbor", 10), 10, 15);
    let ranges = plan_file_source(&content, &[named.clone(), neighbor.clone()], &[], &["namedtarget".into()], 200, &[]);
    assert_eq!(ranges.iter().map(|r| r.node_ids.clone()).collect::<Vec<_>>(), vec![vec![named.id], vec![neighbor.id]]);
    assert_eq!(ranges.iter().map(|r| r.start_line).collect::<Vec<_>>(), vec![200, 10]);
}

#[test]
fn deduplicates_a_complete_parent_child_pair_while_retaining_both_ids() {
    let content = numbered_file(260, &[]);
    let child = with(span(node("method:child", "child", 40), 40, 60), |n| n.kind = "method".into());
    let parent = with(span(node("class:parent", "Parent", 10), 10, 100), |n| n.kind = "class".into());
    let ranges = plan_file_source(&content, &[child.clone(), parent.clone()], &[], &[], 200, &[]);
    assert_eq!(ranges.len(), 1);
    assert_eq!((ranges[0].start_line, ranges[0].end_line, ranges[0].reason, ranges[0].truncated), (10, 100, "complete-symbol", false));
    let got: HashSet<&String> = ranges[0].node_ids.iter().collect();
    assert_eq!(got, [&child.id, &parent.id].into_iter().collect());
}

#[test]
fn uses_a_signature_plus_at_most_two_25_line_windows_for_a_long_symbol() {
    let content = numbered_file(260, &[(89, "const queryAlpha = first();"), (119, "return queryAlpha;"), (179, "invokeFlowSpine();")]);
    let long = with(span(node("method:long", "longMethod", 1), 1, 240), |n| n.kind = "method".into());
    let ranges = plan_file_source(&content, std::slice::from_ref(&long), &[], &["queryalpha".into()], 200, &[180]);
    assert_eq!(ranges.len(), 3);
    assert_eq!((ranges[0].start_line, ranges[0].end_line, ranges[0].reason, ranges[0].truncated), (1, 6, "signature", true));
    assert_eq!((ranges[1].start_line, ranges[1].end_line, ranges[1].reason), (168, 192, "callsite"));
    assert_eq!((ranges[2].start_line, ranges[2].end_line, ranges[2].reason), (78, 102, "query-hit"));
    for r in &ranges[1..] {
        assert!(r.end_line - r.start_line < 25);
    }
    assert!(!ranges.iter().any(|r| r.content.contains("return queryAlpha")));
}

#[test]
fn merges_two_windows_ten_lines_apart_without_merging_the_signature() {
    let content = numbered_file(260, &[(49, "queryNeedle();"), (79, "queryNeedle();")]);
    let long = with(span(node("method:long", "longMethod", 1), 1, 240), |n| n.kind = "method".into());
    let ranges = plan_file_source(&content, &[long], &[], &["queryneedle".into()], 200, &[]);
    assert_eq!(ranges.len(), 2);
    assert_eq!((ranges[0].start_line, ranges[0].end_line, ranges[0].reason), (1, 6, "signature"));
    assert_eq!((ranges[1].start_line, ranges[1].end_line, ranges[1].reason), (38, 92, "query-hit"));
}

// ---------------------------------------------------------------------------
// scored scope selection
// ---------------------------------------------------------------------------

#[test]
fn prefers_concepts_co_located_in_one_region_over_terms_scattered_in_a_central_file() {
    let sig = "function f(alpha: Alpha, beta: Beta, gamma: Gamma): void";
    let focused = with(in_file(node("function:focused", "focused", 1), "src/focused.ts"), |n| n.signature = Some(sig.into()));
    let central = with(in_file(node("function:central", "central", 1), "src/central.ts"), |n| n.signature = Some(sig.into()));
    let g = G::new(vec![central.clone(), focused.clone()])
        .search_all(vec![central, focused])
        .files(&[("src/central.ts", "ok"), ("src/focused.ts", "ok")])
        .source(vec![
            hit("src/central.ts", 1, 20, -0.05, &["alpha"], &[]),
            hit("src/central.ts", 61, 80, -0.05, &["beta"], &[]),
            hit("src/central.ts", 121, 140, -0.05, &["gamma"], &[]),
            hit("src/focused.ts", 1, 20, -0.05, &["alpha", "beta", "gamma"], &[]),
        ]);
    let sel = select(&g, "alpha beta gamma", 10, 2);
    assert_eq!(sel.files[0].file_path, "src/focused.ts");
    assert!(sel.files[0].score > sel.files[1].score);
}

#[test]
fn reserves_the_best_source_chunk_declaration_before_aggregate_representatives() {
    let aligned = span(in_file(node("function:aligned", "recoverCandidate", 10), "src/extensions.ts"), 10, 30);
    let aggregate = span(in_file(node("function:aggregate", "supportedExtensionLookup", 12), "src/extensions.ts"), 12, 20);
    let g = G::new(vec![aligned.clone(), aggregate.clone()])
        .search_all(vec![aggregate.clone()])
        .indexed()
        .source(vec![
            hit("src/extensions.ts", 10, 30, -0.2, &["supported", "extension", "lookup"], &[&aligned.id]),
            hit("src/extensions.ts", 12, 20, -0.19, &["supported", "extension", "lookup"], &[&aggregate.id]),
        ]);
    let sel = select(&g, "supported extension lookup", 2, 1);
    assert_eq!(ids(&sel), vec![aggregate.id.clone(), aligned.id.clone()]);
    assert_eq!(sel.files[0].node_ids, vec![aggregate.id, aligned.id]);
}

#[test]
fn preserves_a_full_query_declaration_when_a_same_file_region_ranks_first() {
    let target = with(span(in_file(node("function:target", "planFileSource", 120), "src/planner.ts"), 120, 150), |n| {
        n.signature = Some("function planFileSource(symbols: Symbol[]): SourceWindow[]".into())
    });
    let decoy = span(in_file(node("function:source-decoy", "selectScope", 1), "src/planner.ts"), 1, 80);
    let t = target.clone();
    let g = G::new(vec![target.clone(), decoy.clone()])
        .search(move |q| if ["plan source", "plan", "source"].contains(&q) { vec![t.clone()] } else { Vec::new() })
        .indexed()
        .source(vec![
            hit("src/planner.ts", 1, 80, -0.3, &["plan", "source"], &[&decoy.id]),
            hit("src/planner.ts", 101, 160, -0.2, &["plan", "source"], &[&target.id]),
        ]);
    let sel = select(&g, "plan source", 1, 1);
    assert_eq!(ids(&sel), vec![target.id.clone()]);
    assert_eq!(sel.files[0].node_ids, vec![target.id.clone()]);
    assert!(sel.candidates[0].reasons.contains(&"bm25-node".to_string()));
    assert!(!sel.candidates[0].reasons.contains(&"query-phrase-flow".to_string()));
}

#[test]
fn does_not_count_stems_of_one_raw_concept_as_independent_full_query_proof() {
    let wrapper = with(span(in_file(node("function:wrapper", "getSupportedExtensionsWithJsonIfResolveJsonModule", 40), "src/extensions.ts"), 40, 50), |n| {
        n.signature = Some("function getSupportedExtensionsWithJsonIfResolveJsonModule(options: CompilerOptions): string[]".into())
    });
    let aligned = with(span(in_file(node("function:aligned", "getSupportedExtensions", 10), "src/extensions.ts"), 10, 30), |n| {
        n.signature = Some("function getSupportedExtensions(options: CompilerOptions): string[]".into())
    });
    let g = G::new(vec![wrapper.clone(), aligned.clone()])
        .search_all(vec![wrapper.clone(), aligned.clone()])
        .indexed()
        .source(vec![hit("src/extensions.ts", 10, 30, -0.3, &["supported", "compiler", "options"], &[&aligned.id])]);
    let sel = select(&g, "which routine decides supported compiler options", 1, 1);
    assert_eq!(ids(&sel), vec![aligned.id]);
}

#[test]
fn does_not_reserve_a_full_query_bm25_hit_corroborated_only_by_prose() {
    let prose = with(in_file(node("function:prose-only", "utility", 1), "src/utility.ts"), |n| {
        n.docstring = Some("Plan bounded source windows for a request.".into())
    });
    let aligned = with(span(in_file(node("function:aligned-answer", "sourceWindows", 20), "src/planner.ts"), 20, 40), |n| {
        n.signature = Some("function sourceWindows(plan: Plan): SourceWindow[]".into())
    });
    let query = "plan bounded source windows";
    let (p, a) = (prose.clone(), aligned.clone());
    let g = G::new(vec![prose.clone(), aligned.clone()])
        .search(move |q| if q == query { vec![p.clone()] } else { vec![p.clone(), a.clone()] })
        .indexed()
        .source(vec![hit("src/planner.ts", 20, 40, -0.3, &["plan", "bounded", "source", "windows"], &[&aligned.id])]);
    let sel = select(&g, query, 1, 2);
    assert_eq!(ids(&sel), vec![aligned.id]);
}

#[test]
fn reserves_one_production_full_query_declaration_deterministically() {
    let test_hit = with(in_file(node("function:test-hit", "alphaBetaTest", 1), "src/__tests__/pipeline.test.ts"), |n| {
        n.signature = Some("function alphaBetaTest(pipeline: Pipeline): void".into())
    });
    let first = with(in_file(node("function:production-first", "alphaBetaPrimary", 100), "src/pipeline.ts"), |n| {
        n.signature = Some("function alphaBetaPrimary(pipeline: Pipeline): void".into())
    });
    let second = with(in_file(node("function:production-second", "alphaBetaSecondary", 140), "src/pipeline.ts"), |n| {
        n.signature = Some("function alphaBetaSecondary(pipeline: Pipeline): void".into())
    });
    let decoy = span(in_file(node("function:source-decoy", "regionOwner", 1), "src/pipeline.ts"), 1, 80);
    let make = || {
        G::new(vec![test_hit.clone(), first.clone(), second.clone(), decoy.clone()])
            .search_all(vec![test_hit.clone(), first.clone(), second.clone()])
            .files(&[("src/__tests__/pipeline.test.ts", "ok"), ("src/pipeline.ts", "ok")])
            .source(vec![hit("src/pipeline.ts", 1, 80, -0.3, &["alpha", "beta", "pipeline"], &[&decoy.id])])
    };
    let a = select(&make(), "alpha beta pipeline", 2, 2);
    let b = select(&make(), "alpha beta pipeline", 2, 2);
    assert_eq!(ids(&a), vec![first.id.clone(), decoy.id.clone()]);
    assert_eq!(ids(&b), ids(&a));
}

#[test]
fn does_not_turn_sibling_declarations_in_a_broad_chunk_into_candidates() {
    let file = with(span(in_file(node("file:context", "retrievedContext.ts", 1), "src/retrieved-context.ts"), 1, 80), |n| {
        n.kind = "file".into()
    });
    let anchor = span(in_file(node("function:anchor", "quotaGate", 10), "src/retrieved-context.ts"), 10, 30);
    let sibling = span(in_file(node("function:sibling", "legacyNeighborhood", 50), "src/retrieved-context.ts"), 50, 55);
    let callback = span(in_file(node("function:file-callback", "<callback:file[0]>", 60), "src/retrieved-context.ts"), 60, 60);
    let target = span(in_file(node("function:callback-target", "unrelatedCallbackTarget", 70), "src/retrieved-context.ts"), 70, 75);
    let c_anchor = contains(&file, &anchor).at(10, 0);
    let c_sibling = contains(&file, &sibling).at(50, 0);
    let c_callback = contains(&file, &callback).at(60, 0);
    let cb_call = calls(&callback, &target).at(60, 0);
    let mut g = G::new(vec![file.clone(), anchor.clone(), sibling.clone(), callback.clone(), target.clone()])
        .search_all(vec![file.clone()])
        .indexed()
        .source(vec![hit("src/retrieved-context.ts", 1, 80, -0.2, &["retrieved", "context", "token"], &[&anchor.id, &sibling.id])]);
    g.in_edges = vec![c_anchor.clone(), c_sibling.clone()];
    g.out_edges = vec![c_anchor, c_sibling, c_callback, cb_call];
    let sel = select(&g, "prevent retrieved context from exceeding its token allowance", 10, 1);
    let got = ids(&sel);
    assert!(got.contains(&anchor.id));
    for id in [&sibling.id, &callback.id, &target.id] {
        assert!(!got.contains(id), "{:?}", got);
    }
    assert_eq!(sel.files[0].file_path, anchor.file_path);
    assert!(!sel.files[0].text_only);
    assert!(!sel.files[0].node_ids.contains(&sibling.id));

    // The same window, with no lexical or graph support: text-only evidence.
    let weak = G {
        nodes: vec![sibling.clone()],
        ..G::new(vec![])
    }
    .indexed()
    .files(&[("src/retrieved-context.ts", "ok")])
    .source(vec![hit("src/retrieved-context.ts", 1, 80, -0.2, &["retrieved", "context"], &[&sibling.id])]);
    let sel = select(&weak, "retrieved context allowance", 10, 1);
    assert!(sel.candidates.is_empty());
    assert_eq!(sel.files[0].file_path, sibling.file_path);
    assert!(sel.files[0].node_ids.is_empty() && sel.files[0].text_only);
    assert_eq!(sel.files[0].text_hits.len(), 1);
}

#[test]
fn reserves_a_compiler_proven_cross_file_destination_for_an_adjacent_phrase() {
    let schedule = in_file(node("function:schedule", "scheduleCacheInvalidation", 10), "src/coordinator.ts");
    let invalidate = in_file(node("function:invalidate", "cacheInvalidationHandler", 20), "src/cache.ts");
    let distractors: Vec<Node> = (0..3)
        .map(|i| in_file(node(&format!("function:distractor-{}", i), &format!("cacheInvalidationGuide{}", i), 1), &format!("src/guide-{}.ts", i)))
        .collect();
    let edge = calls(&schedule, &invalidate).at(12, 2);
    let later = calls(&schedule, &invalidate).at(29, 4);
    let mut nodes = vec![schedule.clone(), invalidate.clone()];
    nodes.extend(distractors.clone());
    let (s, d) = (schedule.clone(), distractors.clone());
    let g = G::new(nodes)
        .search(move |q| if q.to_lowercase() == "cache invalidation" { vec![s.clone()] } else { d.clone() })
        // Reverse callsite order: storage order must not choose the representative edge.
        .outgoing_only(vec![later, edge.clone()])
        .indexed()
        .source(
            distractors
                .iter()
                .enumerate()
                .map(|(i, n)| hit(&n.file_path, 1, 20, -0.2 + i as f64 * 0.001, &["cache", "invalidation"], &[&n.id]))
                .collect(),
        );
    let sel = select(&g, "How does cache invalidation cross the service boundary?", 12, 2);
    assert!(paths(&sel).contains(&invalidate.file_path), "{:?}", paths(&sel));
    assert!(reasons_of(&sel, &invalidate.id).contains(&"query-phrase-flow".to_string()));
    assert_eq!(first_flow(&sel), vec![edge]);
}

#[test]
fn keeps_filtered_phrase_concepts_attached_to_their_bridge() {
    let source = in_file(node("function:file-resolution-dispatcher", "fileResolutionDispatcher", 10), "src/dispatcher.ts");
    let target = in_file(node("function:file-resolution-handler", "fileResolutionHandler", 20), "src/handler.ts");
    let edge = calls(&source, &target).at(12, 2);
    let s = source.clone();
    let g = G::new(vec![source.clone(), target.clone()])
        .search(move |q| if q.to_lowercase() == "file resolution" { vec![s.clone()] } else { Vec::new() })
        .outgoing_only(vec![edge.clone()])
        .indexed();
    let sel = select(&g, "graph node source file resolution", 12, 2);
    assert!(reasons_of(&sel, &target.id).contains(&"query-phrase-flow".to_string()));
    assert_eq!(sel.covered_terms, vec!["file", "resolution"]);
    assert_eq!(first_flow(&sel), vec![edge]);
}

#[test]
fn retains_a_required_semantic_bridge_when_its_other_concept_is_low_signal() {
    for (phrase, source_name, target_name, signature, doc) in [
        ("graph retrieval", "runGraphScope", "selectScope", "(graph: GraphEngine): GraphScopeSelection", "Broad graph retrieval for a natural-language request."),
        ("build graph", "graphCommandCallback", "runGraph", "(graph: GraphEngine): void", "Build the graph for the current repository."),
    ] {
        let decorate = |n: Node| {
            with(n, |n| {
                n.signature = Some(signature.into());
                n.docstring = Some(doc.into());
            })
        };
        let source = decorate(in_file(node(&format!("function:{}", source_name), source_name, 10), "src/command.ts"));
        let target = decorate(in_file(node(&format!("function:{}", target_name), target_name, 20), "src/implementation.ts"));
        let edge = calls(&source, &target).at(12, 2);
        let g = G::new(vec![source.clone(), target.clone()])
            .search_all(vec![source.clone()])
            .outgoing_only(vec![edge.clone()])
            .indexed();
        let sel = select(&g, phrase, 12, 2);
        assert!(paths(&sel).contains(&target.file_path), "{}: {:?}", phrase, paths(&sel));
        assert!(reasons_of(&sel, &target.id).contains(&"query-phrase-flow".to_string()), "{}", phrase);
        assert_eq!(first_flow(&sel), vec![edge], "{}", phrase);
    }
}

#[test]
fn does_not_promote_a_source_file_bridge_over_focused_retrieval_evidence() {
    let source = in_file(node("function:source-map", "getSourceMapDirectory", 10), "src/emitter.ts");
    let target = in_file(node("function:new-dir", "getSourceFilePathInNewDir", 20), "src/utilities.ts");
    let focused = in_file(node("function:resolver", "resolveModuleName", 30), "src/module-name-resolver.ts");
    let edge = calls(&source, &target).at(12, 2);
    let (s, f) = (source.clone(), focused.clone());
    let g = G::new(vec![source.clone(), target.clone(), focused.clone()])
        .search(move |q| if q.to_lowercase() == "source file" { vec![s.clone()] } else { vec![f.clone()] })
        .outgoing_only(vec![edge.clone()])
        .indexed()
        .source(vec![hit(&focused.file_path, 30, 40, -0.3, &["locate", "imported", "package"], &[&focused.id])]);
    let sel = select(&g, "locate an imported package or source file", 12, 1);
    assert_eq!(paths(&sel), vec![focused.file_path.clone()]);
    assert!(!sel.files.iter().any(|f| f.reasons.contains(&"query-phrase-flow".to_string())));
    assert!(!sel.candidates.iter().any(|c| c.reasons.contains(&"query-phrase-flow".to_string())));
    assert!(!all_steps(&sel).contains(&edge));
}

#[test]
fn bounds_phrase_flow_endpoint_inspection_after_stable_callsite_ordering() {
    let source = in_file(node("function:source", "scheduleCacheInvalidation", 10), "src/source.ts");
    let rejected: Vec<Node> = (0..16)
        .map(|i| in_file(node(&format!("function:rejected-{}", i), &format!("ordinaryHandler{}", i), i + 1), &format!("src/rejected-{}.ts", i)))
        .collect();
    let beyond = in_file(node("function:beyond-limit", "cacheInvalidationHandler", 30), "src/beyond-limit.ts");
    let mut edges: Vec<EdgeInfo> = rejected.iter().enumerate().map(|(i, t)| calls(&source, t).at(i as i64 + 1, 1)).collect();
    edges.push(calls(&source, &beyond).at(17, 1));
    edges.reverse();
    let mut nodes = vec![source.clone(), beyond.clone()];
    nodes.extend(rejected);
    let s = source.clone();
    let g = G::new(nodes)
        .search(move |q| if q.to_lowercase() == "cache invalidation" { vec![s.clone()] } else { Vec::new() })
        .outgoing_only(edges)
        .indexed();
    let sel = select(&g, "cache invalidation across a boundary", 12, 2);
    assert!(!ids(&sel).contains(&beyond.id));
    assert!(!paths(&sel).contains(&beyond.file_path));
    assert!(sel.flows.is_empty(), "{:?}", sel.flows);
}

#[test]
fn does_not_activate_phrase_flow_for_same_file_or_low_confidence_edges() {
    for (same_file, confidence) in [(true, 1.0), (false, 0.75)] {
        let source = in_file(node("function:source", "scheduleCacheInvalidation", 10), "src/source.ts");
        let target = in_file(node("function:target", "cacheInvalidationHandler", 20), if same_file { "src/source.ts" } else { "src/hidden.ts" });
        let distractor = in_file(node("function:distractor", "cacheInvalidationGuide", 1), "src/guide.ts");
        let edge = calls(&source, &target).at(12, 2).with_confidence(confidence);
        let (s, d) = (source.clone(), distractor.clone());
        let g = G::new(vec![source.clone(), target, distractor.clone()])
            .search(move |q| if q.to_lowercase() == "cache invalidation" { vec![s.clone()] } else { vec![d.clone()] })
            .outgoing_only(vec![edge])
            .indexed()
            .source(vec![hit(&distractor.file_path, 1, 20, -0.2, &["cache", "invalidation"], &[&distractor.id])]);
        let sel = select(&g, "cache invalidation across a boundary", 12, 2);
        assert!(!sel.files.iter().any(|f| f.reasons.contains(&"query-phrase-flow".to_string())));
        assert!(!sel.candidates.iter().any(|c| c.reasons.contains(&"query-phrase-flow".to_string())));
    }
}

#[test]
fn requires_phrase_evidence_in_each_endpoint_identity_not_comments() {
    let source = in_file(node("function:source", "scheduleCacheInvalidation", 10), "src/source.ts");
    let target = with(in_file(node("function:target", "clearEntries", 20), "src/hidden.ts"), |n| {
        n.docstring = Some("Performs cache invalidation after a write.".into())
    });
    let distractor = in_file(node("function:distractor", "cacheInvalidationGuide", 1), "src/guide.ts");
    let edge = calls(&source, &target);
    let (s, d) = (source.clone(), distractor.clone());
    let g = G::new(vec![source.clone(), target, distractor.clone()])
        .search(move |q| if q.to_lowercase() == "cache invalidation" { vec![s.clone()] } else { vec![d.clone()] })
        .outgoing_only(vec![edge])
        .indexed()
        .source(vec![hit(&distractor.file_path, 1, 20, -0.2, &["cache", "invalidation"], &[&distractor.id])]);
    let sel = select(&g, "cache invalidation across a boundary", 12, 2);
    assert!(!sel.files.iter().any(|f| f.reasons.contains(&"query-phrase-flow".to_string())));
    assert!(!sel.candidates.iter().any(|c| c.reasons.contains(&"query-phrase-flow".to_string())));
}

#[test]
fn does_not_let_a_low_signal_two_word_phrase_displace_stronger_source_evidence() {
    let source = in_file(node("function:source", "graphDeclarationScanner", 10), "src/scanner.ts");
    let target = in_file(node("function:target", "graphDeclarationRegistry", 20), "src/registry.ts");
    let focused = with(in_file(node("function:focused", "explainArchitecture", 4), "src/architecture.ts"), |n| {
        n.docstring = Some("Explains graph declarations and their ownership.".into())
    });
    let edge = calls(&source, &target).at(12, 2);
    let (s, f) = (source.clone(), focused.clone());
    let g = G::new(vec![source.clone(), target, focused.clone()])
        .search(move |q| if q.to_lowercase() == "graph declarations" { vec![s.clone()] } else { vec![f.clone()] })
        .outgoing_only(vec![edge])
        .indexed()
        .source(vec![hit(&focused.file_path, 1, 20, -0.3, &["graph", "declarations"], &[&focused.id])]);
    let sel = select(&g, "How do graph declarations work?", 12, 1);
    assert_eq!(paths(&sel), vec![focused.file_path.clone()]);
    assert!(!sel.files.iter().any(|f| f.reasons.contains(&"query-phrase-flow".to_string())));
}

#[test]
fn prefers_a_query_correlated_declaration_over_an_earlier_broad_overlap() {
    let broad = span(in_file(node("function:broad-overlap", "formatCodeSpan", 10), "src/diagnostics.ts"), 10, 50);
    let correlated = span(in_file(node("function:correlated", "formatDiagnosticsWithRelatedInformation", 20), "src/diagnostics.ts"), 20, 30);
    let g = G::new(vec![broad.clone(), correlated.clone()])
        .search_all(vec![broad.clone()])
        .indexed()
        .source(vec![hit("src/diagnostics.ts", 10, 50, -0.2, &["format", "diagnostics", "information"], &[&broad.id, &correlated.id])]);
    let sel = select(&g, "format diagnostics information", 1, 1);
    assert_eq!(ids(&sel), vec![correlated.id.clone()]);
    assert_eq!(sel.files[0].node_ids, vec![correlated.id]);
}

#[test]
fn keeps_the_fourth_global_hit_in_the_source_floor_ahead_of_propagated_callsites() {
    let origin = span(in_file(node("function:origin", "originHandler", 1), "src/origin.ts"), 1, 20);
    let direct = in_file(node("function:direct", "directWinner", 1), "src/direct.ts");
    let pa = in_file(node("function:propagated-a", "propagatedOne", 1), "src/propagated-a.ts");
    let pb = in_file(node("function:propagated-b", "propagatedTwo", 1), "src/propagated-b.ts");
    let pc = in_file(node("function:propagated-c", "propagatedThree", 1), "src/propagated-c.ts");
    let h = |n: &Node, rank: f64| hit(&n.file_path, 1, 20, -0.2 + rank * 0.001, &["opaque", "memory", "pipeline"], &[&n.id]);
    let mut o1 = h(&origin, 1.0);
    o1.start_line = 2;
    o1.end_line = 19;
    let mut o2 = h(&origin, 2.0);
    o2.start_line = 3;
    o2.end_line = 18;
    let g = G::new(vec![origin.clone(), direct.clone(), pa.clone(), pb.clone(), pc.clone()])
        .outgoing_only(vec![calls(&origin, &pa).at(5, 2), calls(&origin, &pb).at(6, 2), calls(&origin, &pc).at(7, 2)])
        .indexed()
        .source(vec![h(&origin, 0.0), o1, o2, h(&direct, 3.0), h(&pa, 4.0), h(&pb, 5.0), h(&pc, 6.0)]);
    let sel = select(&g, "opaque memory pipeline", 12, 4);
    let p = paths(&sel);
    assert!(p.contains(&origin.file_path) && p.contains(&direct.file_path), "{:?}", p);
    assert_eq!(p.iter().filter(|x| x.starts_with("src/propagated-")).count(), 2, "{:?}", p);
}

#[test]
fn preserves_three_independent_source_channel_files_before_the_hybrid_fill() {
    let a = in_file(node("function:source-a", "firstAnchor", 1), "src/a.ts");
    let b = in_file(node("function:source-b", "secondAnchor", 1), "src/b.ts");
    let c = in_file(node("function:source-c", "thirdAnchor", 1), "src/c.ts");
    let broad = with(in_file(node("function:broad", "alphaBetaGammaPipeline", 1), "src/broad.ts"), |n| {
        n.signature = Some("function alphaBetaGammaPipeline(alpha: Alpha, beta: Beta, gamma: Gamma): void".into())
    });
    let g = G::new(vec![a.clone(), b.clone(), c.clone(), broad.clone()])
        .search_all(vec![broad.clone()])
        .indexed()
        .source(
            [&a, &b, &c]
                .iter()
                .enumerate()
                .map(|(i, n)| hit(&n.file_path, 1, 20, -0.05 + i as f64 * 0.001, &["alpha", "beta"], &[&n.id]))
                .collect(),
        );
    let sel = select(&g, "alpha beta gamma", 16, 4);
    let p = paths(&sel);
    for f in [&a.file_path, &b.file_path, &c.file_path, &broad.file_path] {
        assert!(p.contains(f), "{:?}", p);
    }
}

#[test]
fn reserves_a_rare_compound_declaration_ahead_of_broader_common_term_files() {
    let class = |id: &str, name: &str, file: &str| with(in_file(node(id, name, 1), file), |n| n.kind = "class".into());
    let framework = class("class:framework", "Framework", "src/framework.ts");
    let rare = class("class:smart-router", "SmartRouter", "src/smart-router.ts");
    let broad: Vec<Node> = (0..5).map(|i| class(&format!("class:broad-{}", i), "RegisteredPathsMatcher", &format!("src/broad-{}.ts", i))).collect();
    let mut nodes = vec![framework.clone(), rare.clone()];
    nodes.extend(broad.clone());
    let (f, r, b) = (framework.clone(), rare.clone(), broad.clone());
    let g = G::new(nodes)
        .search(move |q| {
            let q = q.to_lowercase();
            let all = || {
                let mut v = b.clone();
                v.push(f.clone());
                v.push(r.clone());
                v
            };
            if q.contains("framework's") {
                all()
            } else if q == "framework" {
                vec![f.clone()]
            } else if q == "smart" || q == "router" {
                vec![r.clone()]
            } else if q.contains("register") || q.contains("path") || q.contains("match") {
                b.clone()
            } else {
                all()
            }
        })
        .indexed()
        .source(
            broad[..2]
                .iter()
                .enumerate()
                .map(|(i, n)| hit(&n.file_path, 1, 20, -0.2 + i as f64 * 0.001, &["registered", "path", "paths", "match", "matches"], &[&n.id]))
                .collect(),
        );
    let sel = select(&g, "How does Framework's smart router support registered paths and later matches?", 16, 4);
    let p = paths(&sel);
    for f in [&framework.file_path, &rare.file_path, &broad[0].file_path, &broad[1].file_path] {
        assert!(p.contains(f), "{:?}", p);
    }
    assert!(!broad[2..].iter().any(|n| p.contains(&n.file_path)), "{:?}", p);
    assert!(ids(&sel).contains(&rare.id));
}

#[test]
fn does_not_let_weak_single_concept_hits_evict_strong_graph_files() {
    let a = in_file(node("function:graph-a", "alphaBetaHandler", 1), "src/graph-a.ts");
    let b = in_file(node("function:graph-b", "gammaDeltaHandler", 1), "src/graph-b.ts");
    let weak = ["src/weak-a.ts", "src/weak-b.ts", "src/weak-c.ts"];
    let g = G::new(vec![a.clone(), b.clone()])
        .search_all(vec![a.clone(), b.clone()])
        .files(&[
            ("src/graph-a.ts", "ok"),
            ("src/graph-b.ts", "ok"),
            ("src/weak-a.ts", "failed"),
            ("src/weak-b.ts", "failed"),
            ("src/weak-c.ts", "failed"),
        ])
        .source(weak.iter().enumerate().map(|(i, p)| hit(p, 1, 20, -0.001 + i as f64 * 0.0001, &["alpha"], &[])).collect());
    let sel = select(&g, "alpha beta gamma delta", 16, 4);
    let p = paths(&sel);
    assert!(p.contains(&a.file_path) && p.contains(&b.file_path), "{:?}", p);
    assert_eq!(p.iter().filter(|x| weak.contains(&x.as_str())).count(), 2, "{:?}", p);
}

#[test]
fn uses_conservative_inflection_stems_for_path_candidates() {
    let target = in_file(node("function:assemble", "assemble", 1), "src/runtime/assemble.ts");
    let distractor = in_file(node("function:stages", "stages", 1), "src/runtime/core.ts");
    let g = G::new(vec![distractor.clone(), target.clone()])
        .search_all(vec![distractor, target])
        .files(&[("src/runtime/core.ts", "ok"), ("src/runtime/assemble.ts", "ok")]);
    let sel = select(&g, "where are request stages assembled", 10, 2);
    assert_eq!(sel.files[0].file_path, "src/runtime/assemble.ts");
    assert!(sel.files[0].reasons.contains(&"path-match".to_string()));
}

#[test]
fn propagates_a_reliable_exact_seed_through_two_typed_relevance_hops() {
    let leaf = node("function:leaf", "Leaf", 3);
    let parent = node("function:parent", "parent", 2);
    let top = node("function:top", "top", 1);
    let g = G::new(vec![leaf.clone(), parent.clone(), top.clone()])
        .search_all(vec![leaf.clone()])
        .edges(vec![calls(&parent, &leaf).at(2, 2), calls(&top, &parent).at(1, 2)]);
    let sel = select(&g, "Leaf", 10, 6);
    assert_eq!(ids(&sel), vec![leaf.id.clone(), parent.id.clone(), top.id.clone()]);
    assert!(sel.candidates[0].reasons.contains(&"exact:Leaf".to_string()));
    assert_eq!(sel.candidates[1..].iter().map(|c| c.category).collect::<Vec<_>>(), vec!["neighbor", "neighbor"]);
}

#[test]
fn emits_only_contiguous_directed_paths_within_the_step_budget() {
    let seed = node("function:seed", "Seed", 1);
    let a = node("function:a", "alpha", 2);
    let b = node("function:b", "beta", 3);
    let c = node("function:c", "gamma", 4);
    let d = node("function:d", "delta", 5);
    let g = G::new(vec![seed.clone(), a.clone(), b.clone(), c.clone(), d.clone()])
        .search_all(vec![seed.clone()])
        .outgoing_only(vec![calls(&seed, &a), calls(&seed, &b), calls(&a, &c), calls(&b, &d)])
        .files(&[("src/sample.ts", "ok")]);
    let sel = select(&g, "Seed", 10, 6);
    let shape: Vec<Vec<String>> =
        sel.flows.iter().map(|f| f.steps.iter().map(|s| format!("{}->{}", s.source, s.target)).collect()).collect();
    assert_eq!(
        shape,
        vec![
            vec!["function:seed->function:a", "function:a->function:c"],
            vec!["function:seed->function:b", "function:b->function:d"],
        ]
    );
    let keys: HashSet<String> = all_steps(&sel).iter().map(|s| format!("{}{}{}", s.source, s.target, s.kind)).collect();
    assert_eq!(keys.len(), 4);
    flow_shape_ok(&sel);
}

#[test]
fn traverses_through_a_lexically_invisible_bridge_file_and_promotes_it() {
    let alpha = in_file(node("function:alpha", "Alpha", 1), "src/alpha.ts");
    let bridge = span(in_file(node("function:bridge", "relay", 1), "src/bridge.ts"), 1, 3);
    let gamma = in_file(node("function:gamma", "Gamma", 1), "src/gamma.ts");
    let distractor = in_file(node("function:pipeline", "pipelineGuide", 1), "src/guide.ts");
    let ab = calls(&alpha, &bridge);
    let bg = calls(&bridge, &gamma);
    let (a, gm, d) = (alpha.clone(), gamma.clone(), distractor.clone());
    let g = G::new(vec![alpha.clone(), bridge.clone(), gamma.clone(), distractor.clone()])
        .search(move |q| {
            let q = q.to_lowercase();
            if q == "alpha" {
                vec![a.clone()]
            } else if q == "gamma" {
                vec![gm.clone()]
            } else if q.contains("pipeline") {
                vec![d.clone()]
            } else {
                vec![a.clone(), gm.clone(), d.clone()]
            }
        })
        .edges(vec![ab.clone(), bg.clone()])
        .indexed()
        // A lexical-only file inside the initial three-file budget; flow promotion evicts it.
        .source(vec![hit(&distractor.file_path, 1, 1, -5.0, &["pipeline"], &[])]);
    let sel = select(&g, "Alpha Gamma pipeline", 10, 3);
    let p = paths(&sel);
    assert_eq!(p.len(), 3);
    for f in [&alpha.file_path, &bridge.file_path, &gamma.file_path] {
        assert!(p.contains(f), "{:?}", p);
    }
    assert!(!p.contains(&distractor.file_path));
    let bf = sel.files.iter().find(|f| f.file_path == bridge.file_path).unwrap();
    assert!(bf.node_ids.contains(&bridge.id) && !bf.text_only && bf.reasons.contains(&"flow-spine".to_string()));
    assert!(ids(&sel).contains(&bridge.id));
    assert_eq!(first_flow(&sel), vec![ab, bg]);
    let ranges = plan_file_source("export function relay() {\n  return Gamma();\n}\n", std::slice::from_ref(&bridge), &[], &[], 160, &[]);
    assert_eq!((ranges[0].node_ids.clone(), ranges[0].reason), (vec![bridge.id], "whole-file"));
    assert!(ranges[0].content.contains("return Gamma()"));
}

#[test]
fn prefers_a_query_region_callsite_into_an_outside_file_over_an_incidental_flow() {
    let source = span(in_file(node("function:source", "pipelineEntry", 1), "src/source.ts"), 1, 40);
    let target = in_file(node("function:target", "semanticTarget", 1), "src/target.ts");
    let incidental = span(in_file(node("function:incidental", "pipelineDispatchCoordinator", 1), "src/incidental.ts"), 1, 100);
    let helper = in_file(node("function:helper", "internalHelper", 80), "src/incidental.ts");
    let st = calls(&source, &target).at(10, 2);
    let ih = calls(&incidental, &helper).at(80, 2);
    let g = G::new(vec![source.clone(), target.clone(), incidental.clone(), helper])
        .search_all(vec![incidental.clone()])
        .outgoing_only(vec![st.clone(), ih])
        .indexed()
        .source(vec![
            hit(&source.file_path, 1, 20, -0.2, &["pipeline", "dispatch"], &[&source.id]),
            hit(&incidental.file_path, 1, 20, -0.19, &["pipeline", "dispatch"], &[&incidental.id]),
        ]);
    let sel = select(&g, "pipeline dispatch", 12, 2);
    assert_eq!(first_flow(&sel), vec![st]);
    let p = paths(&sel);
    assert!(p.contains(&source.file_path) && p.contains(&target.file_path), "{:?}", p);
    assert!(!p.contains(&incidental.file_path));
    let r = reasons_of(&sel, &target.id);
    assert!(r.contains(&"graph:calls".to_string()) && r.contains(&"source-region:callsite".to_string()), "{:?}", r);
    assert!(!ids(&sel).contains(&incidental.id));
}

#[test]
fn promotes_the_cohesive_destination_of_a_broad_query_region_callsite_set() {
    let origin = span(in_file(node("function:origin", "entry", 1), "src/origin.ts"), 1, 100);
    let incidental = exported(in_file(node("function:incidental", "opaquePackagePipeline", 1), "src/incidental.ts"));
    let noise = exported(in_file(node("function:noise", "packagePipelineHelper", 1), "src/noise.ts"));
    let prepare = exported(in_file(node("function:prepare", "prepareState", 1), "src/resolver.ts"));
    let load = exported(in_file(node("function:load", "loadScope", 2), "src/resolver.ts"));
    let resolve = exported(in_file(node("function:resolve", "resolveDependency", 3), "src/resolver.ts"));
    let helpers: Vec<Node> =
        (0..6).map(|i| exported(in_file(node(&format!("function:helper-{}", i), &format!("helper{}", i), 1), &format!("src/helper-{}.ts", i)))).collect();
    let call = |t: &Node, line: i64| calls(&origin, t).at(line, 2);
    let mut edges: Vec<EdgeInfo> = (0..5).map(|i| call(&incidental, 5 + i)).collect();
    edges.push(call(&noise, 10));
    edges.extend(helpers.iter().enumerate().map(|(i, h)| call(h, 20 + i as i64)));
    let resolve_call = call(&resolve, 50);
    edges.extend([call(&prepare, 40), call(&load, 45), resolve_call.clone()]);
    let mut nodes = vec![origin.clone(), incidental.clone(), noise.clone(), prepare, load, resolve.clone()];
    nodes.extend(helpers);
    let g = G::new(nodes)
        .search_all(vec![incidental.clone(), noise.clone(), origin.clone()])
        .outgoing_only(edges)
        .indexed()
        .source(vec![hit(&origin.file_path, 1, 100, -0.2, &["opaque", "package", "pipeline"], &[&origin.id])]);
    let sel = select(&g, "opaque package pipeline", 16, 3);
    assert_eq!(first_flow(&sel), vec![resolve_call]);
    let p = paths(&sel);
    for f in [&origin.file_path, &incidental.file_path, &resolve.file_path] {
        assert!(p.contains(f), "{:?}", p);
    }
    assert!(!p.contains(&noise.file_path));
    assert!(ids(&sel).contains(&resolve.id));
}

#[test]
fn prefers_the_query_relevant_construction_edge_over_a_later_wrapper_call() {
    let method = |id: &str, name: &str, s: i64, e: i64| with(span(in_file(node(id, name, s), "src/context.ts"), s, e), |n| n.kind = "method".into());
    let private = method("method:new-response", "#newResponse", 608, 656);
    let wrapper = method("method:response-wrapper", "newResponse", 658, 658);
    let create = span(in_file(node("function:create-response", "createResponseInstance", 288), "src/context.ts"), 288, 291);
    let wrapper_call = calls(&wrapper, &private).at(658, 47);
    let construction = calls(&private, &create).at(652, 11);
    let g = G::new(vec![private.clone(), wrapper.clone(), create.clone()])
        .search_all(vec![private.clone(), wrapper.clone(), create])
        .edges(vec![wrapper_call.clone(), construction.clone()])
        .files(&[("src/context.ts", "ok")])
        .source(vec![hit("src/context.ts", 601, 680, -0.2, &["context", "response", "headers", "status"], &[&private.id, &wrapper.id])]);
    let sel = select(&g, "context constructs response with prepared headers and status", 2, 1);
    assert_eq!(first_flow(&sel), vec![construction]);
    assert!(all_steps(&sel).contains(&wrapper_call));
    flow_shape_ok(&sel);
}

#[test]
fn reserves_two_relevant_same_region_targets_ahead_of_the_latest_helper() {
    let file = "src/middleware/cache/index.ts";
    let cache = span(in_file(node("function:cache", "cache", 183), file), 183, 324);
    let digest = span(in_file(node("function:digest", "createQueryDigest", 95), file), 95, 153);
    let key = span(in_file(node("function:key", "createCacheKey", 51), file), 51, 70);
    let skip = span(in_file(node("function:skip", "shouldSkipCache", 72), file), 72, 80);
    let helpers: Vec<Node> = (0..7).map(|i| in_file(node(&format!("function:cache-helper-{}", i), &format!("opaqueHelper{}", i), i + 1), file)).collect();
    let call = |t: &Node, line: i64| calls(&cache, t).at(line, 2);
    let (dc, kc, sc) = (call(&digest, 277), call(&key, 296), call(&skip, 313));
    let mut edges: Vec<EdgeInfo> = helpers.iter().enumerate().map(|(i, h)| call(h, 300 + i as i64)).collect();
    edges.extend([dc.clone(), kc.clone(), sc.clone()]);
    let mut nodes = vec![cache.clone(), digest, key, skip];
    nodes.extend(helpers);
    let g = G::new(nodes.clone())
        .search_all(nodes)
        .edges(edges)
        .files(&[(file, "ok")])
        .source(vec![hit(file, 241, 320, -0.2, &["cache", "query", "digest", "key"], &[&cache.id])]);
    let sel = select(&g, "cache query digest key", 3, 1);
    let firsts: Vec<EdgeInfo> = sel.flows.iter().take(2).map(|f| f.steps[0].clone()).collect();
    assert_eq!(firsts, vec![dc, kc]);
    assert!(!firsts.contains(&sc));
    flow_shape_ok(&sel);
}

#[test]
fn keeps_a_query_region_callsite_flow_below_the_candidate_quota() {
    let alpha = in_file(node("function:alpha", "AlphaBeta", 1), "src/source.ts");
    let gamma = in_file(node("function:gamma", "GammaDelta", 2), "src/source.ts");
    let theta = in_file(node("function:theta", "ThetaSigma", 3), "src/source.ts");
    let callsite = span(in_file(node("function:callsite", "hiddenDispatcher", 40), "src/source.ts"), 40, 60);
    let target = exported(in_file(node("function:target", "semanticTarget", 1), "src/target.ts"));
    let distractor = in_file(node("function:distractor", "unrelatedHelper", 1), "src/noise.ts");
    let ct = calls(&callsite, &target).at(55, 2);
    let competing = calls(&alpha, &distractor).at(1, 2);
    let nodes = vec![alpha.clone(), gamma.clone(), theta.clone(), callsite.clone(), target, distractor];
    let all = nodes.clone();
    let (a, gm, t) = (alpha.clone(), gamma, theta);
    let g = G::new(nodes)
        .search(move |q| match all.iter().find(|n| n.name.to_lowercase() == q.to_lowercase()) {
            Some(n) => vec![n.clone()],
            None => vec![a.clone(), gm.clone(), t.clone()],
        })
        .outgoing_only(vec![ct.clone(), competing.clone()])
        .indexed()
        .source(vec![hit("src/source.ts", 40, 60, -0.2, &["pipeline", "target", "operation"], &[&callsite.id])]);
    let sel = select(&g, "AlphaBeta GammaDelta ThetaSigma pipeline target operation", 3, 3);
    assert!(!ids(&sel).contains(&callsite.id));
    assert_eq!(first_flow(&sel), vec![ct]);
    assert!(all_steps(&sel).contains(&competing));
}

/// Reference `callPairCallbackCompletionFixture`.
struct CallPairFixture {
    g: G,
    task: &'static str,
    spine: Vec<EdgeInfo>,
    entry: Node,
    planner: Node,
}

fn call_pair_fixture(terminal_name: &str, containment: f64, container: Option<&str>, prove: bool) -> CallPairFixture {
    let task = "How does an agent expand a selected graph node into bounded source code?";
    let entry = exported(with(span(in_file(node("function:callback-entry", "runGraphGet", 1), "src/agent.ts"), 1, 20), |n| {
        n.signature = Some(if prove {
            "(ids: string[], deps?: AgentCommandDeps, rawOptions?: RawOptions): void".into()
        } else {
            "(ids: string[]): void".into()
        });
        n.docstring = prove.then(|| "Targeted source expansion by graph node id. Output is source records.".into());
    }));
    let planner = with(span(in_file(node("function:callback-planner", "planSource", 30), "src/agent.ts"), 30, 70), |n| {
        n.signature = Some("(ledger: BudgetLedger, nodes: GraphNode[], opts: AgentOptions): SourceRange[]".into())
    });
    let callback = with(span(in_file(node("function:callback-planner-map", "<callback:fileNodes.map[0]>", 40), "src/agent.ts"), 40, 50), |n| {
        n.qualified_name = "planSource::<callback:fileNodes.map[0]>".into();
        n.container_id = Some(container.unwrap_or("function:callback-planner").to_string());
    });
    let terminal = exported(span(in_file(node("function:callback-terminal", terminal_name, 1), "src/source.ts"), 1, 15));
    let distractors: Vec<Node> =
        (0..8).map(|i| exported(in_file(node(&format!("function:callback-noise-{}", i), &format!("opaqueBranch{}", i), 1), "src/agent.ts"))).collect();
    let fillers: Vec<Node> =
        (0..8).map(|i| in_file(node(&format!("function:callback-corpus-{}", i), &format!("unrelatedCorpusSymbol{}", i), 1), &format!("src/corpus-{}.ts", i))).collect();
    let entry_planner = calls(&entry, &planner).at(10, 2);
    let planner_callback = contains(&planner, &callback).at(40, 2).with_confidence(containment);
    let callback_terminal = calls(&callback, &terminal).at(45, 4);
    let mut edges = vec![entry_planner.clone(), planner_callback.clone(), callback_terminal.clone()];
    edges.extend(distractors.iter().enumerate().map(|(i, d)| calls(&entry, d).at(2 + i as i64, 2)));
    let mut search: Vec<Node> = fillers.clone();
    search.extend(distractors.clone());
    search.extend([entry.clone(), planner.clone()]);
    let mut nodes = vec![entry.clone(), planner.clone(), callback, terminal];
    nodes.extend(distractors);
    nodes.extend(fillers);
    let g = G::new(nodes)
        .search_all(search)
        .edges(edges)
        .honoring_kinds()
        .indexed()
        .source(vec![
            hit("src/agent.ts", 1, 20, -0.3, &["expand", "selected", "graph", "node"], &[&entry.id]),
            hit("src/agent.ts", 30, 35, -0.29, &["bounded", "source", "node"], &[&planner.id]),
        ]);
    CallPairFixture {
        g,
        task,
        spine: vec![entry_planner, planner_callback, callback_terminal],
        entry,
        planner,
    }
}

#[test]
fn reserves_one_bounded_whole_query_call_pair_callback_completion() {
    let mut flows: Vec<Vec<ScopeFlow>> = Vec::new();
    for max_files in [4, 5, 6] {
        let f = call_pair_fixture("readNodeSource", 1.0, None, true);
        let sel = select(&f.g, f.task, 16, max_files);
        for id in [&f.entry.id, &f.planner.id] {
            assert!(reasons_of(&sel, id).contains(&"bm25-call-pair".to_string()), "{} {:?}", id, ids(&sel));
        }
        assert_eq!(first_flow(&sel), f.spine);
        flow_shape_ok(&sel);
        flows.push(sel.flows);
    }
    assert_eq!(flows[1], flows[0]);
    assert_eq!(flows[2], flows[0]);
}

#[test]
fn does_not_reserve_a_same_file_callback_prefix_without_call_pair_proof() {
    let f = call_pair_fixture("readNodeSource", 1.0, None, false);
    let sel = select(&f.g, f.task, 16, 4);
    let got = ids(&sel);
    assert!(got.contains(&f.entry.id) && got.contains(&f.planner.id), "{:?}", got);
    for id in [&f.entry.id, &f.planner.id] {
        assert!(!reasons_of(&sel, id).contains(&"bm25-call-pair".to_string()));
    }
    assert!(!sel.flows.iter().any(|fl| fl.steps == f.spine));
    flow_shape_ok(&sel);
}

#[test]
fn does_not_reserve_a_callback_completion_that_fails_a_trust_gate() {
    for (terminal, containment, container) in [
        ("flushCache", 1.0, None),
        ("readNodeSource", 0.79, None),
        ("readNodeSource", 1.0, Some("function:other-owner")),
    ] {
        let f = call_pair_fixture(terminal, containment, container, true);
        let sel = select(&f.g, f.task, 16, 12);
        assert_ne!(first_flow(&sel), f.spine, "{} {} {:?}", terminal, containment, container);
        flow_shape_ok(&sel);
    }
}

#[test]
fn does_not_let_a_pair_only_seed_displace_a_query_region_callback_owner() {
    let task = "request validation input header makes validated data available to handlers";
    let (request, validation) = ("src/request.ts", "src/validation.ts");
    let pair_target = with(in_file(node("method:pair-target", "requestHeader", 1), request), |n| {
        n.kind = "method".into();
        n.signature = Some("(request: Request, header: string): string".into());
    });
    let owner = span(in_file(node("function:validation-owner", "validateInput", 1), validation), 1, 100);
    let callback = with(span(in_file(node("function:validation-callback", "<callback:validateInput[0]>", 10), validation), 10, 30), |n| {
        n.qualified_name = "validateInput::<callback:validateInput[0]>".into();
        n.container_id = Some(owner.id.clone());
    });
    let terminal = exported(with(in_file(node("method:validated-data", "addValidatedData", 40), request), |n| n.kind = "method".into()));
    let fillers: Vec<Node> = (0..15)
        .map(|i| in_file(node(&format!("function:selected-{:02}", i), &format!("inputHandler{}", i), i + 50), if i % 2 == 0 { validation } else { request }))
        .collect();
    let pair_source = exported(with(span(in_file(node("function:pair-source", "validationRequestMiddleware", 1), "src/guard.ts"), 1, 20), |n| {
        n.signature = Some("(): MiddlewareHandler".into());
        n.docstring = Some("Validation checks the request header and makes validated input available to handlers.".into());
    }));
    let seeds: Vec<Node> = (0..13)
        .map(|i| span(in_file(node(&format!("function:provenance-{:02}", i), &format!("opaqueRegion{}", i), 1), &format!("src/region-{:02}.ts", i)), 1, 10))
        .collect();
    let sink = in_file(node("function:provenance-sink", "opaqueSink", 1), "src/sink.ts");
    let contains_e = contains(&owner, &callback).at(10, 2);
    let validated = calls(&callback, &terminal).at(20, 4);
    let mut edges = vec![contains_e.clone(), validated.clone(), calls(&pair_source, &pair_target).at(5, 2)];
    edges.extend(seeds.iter().map(|s| calls(s, &sink).at(5, 2)));
    edges.extend(fillers.iter().enumerate().map(|(i, t)| calls(&owner, t).at(50 + i as i64, 2)));
    let mut whole = vec![pair_target.clone(), owner.clone(), terminal.clone()];
    whole.extend(fillers.clone());
    whole.push(pair_source.clone());
    whole.extend(seeds.clone());
    let mut nodes = whole.clone();
    nodes.extend([callback, sink]);
    let mut source = vec![hit(validation, 1, 30, -0.3, &["request", "validation", "input", "validated", "data"], &[&owner.id])];
    source.extend(seeds.iter().enumerate().map(|(i, s)| hit(&s.file_path, 1, 10, -0.2 + i as f64 / 1000.0, &["request", "validation", "input"], &[&s.id])));
    let g = G::new(nodes).search_all(whole).edges(edges).honoring_kinds().indexed().source(source);
    let sel = select(&g, task, 18, 2);
    assert_eq!(first_flow(&sel), vec![contains_e, validated]);
    assert!(reasons_of(&sel, &pair_target.id).contains(&"bm25-call-pair".to_string()), "{:?}", ids(&sel));
    assert!(!ids(&sel).contains(&pair_source.id));
    flow_shape_ok(&sel);
}

#[test]
fn uses_one_real_containment_edge_to_enter_an_anonymous_callback_flow() {
    let outer = node("function:outer", "compose", 1);
    let callback = with(node("function:callback", "<callback:ReturnStatement>", 2), |n| {
        n.qualified_name = "compose::<callback:ReturnStatement>".into()
    });
    let inner = with(node("function:inner", "dispatch", 3), |n| n.qualified_name = "compose::<callback:ReturnStatement>::dispatch".into());
    let c = contains(&outer, &callback);
    let k = calls(&callback, &inner);
    let (o, i) = (outer.clone(), inner.clone());
    let g = G::new(vec![outer.clone(), callback, inner.clone()])
        .search(move |q| match q.to_lowercase().as_str() {
            "compose" => vec![o.clone()],
            "dispatch" => vec![i.clone()],
            _ => vec![o.clone(), i.clone()],
        })
        .edges(vec![c.clone(), k.clone()])
        .files(&[("src/sample.ts", "ok")]);
    let sel = select(&g, "compose dispatch", 10, 6);
    assert_eq!(first_flow(&sel), vec![c, k]);
    flow_shape_ok(&sel);
}

#[test]
fn replaces_an_anonymous_callback_suffix_with_its_named_owner_flow() {
    let validator = span(node("function:validator", "validator", 10), 10, 35);
    let callback = with(span(node("function:validator-callback", "<callback:validator[0]>", 20), 20, 30), |n| {
        n.qualified_name = "validator::<callback:validator[0]>".into()
    });
    let add = node("method:add-validated-data", "addValidatedData", 40);
    let c = contains(&validator, &callback).at(20, 2);
    let k = calls(&callback, &add).at(25, 4);
    let g = G::new(vec![validator.clone(), callback.clone(), add])
        .search_all(vec![callback])
        .edges(vec![c.clone(), k.clone()])
        .files(&[("src/sample.ts", "ok")])
        .source(vec![hit("src/sample.ts", 10, 30, -0.2, &["validator", "validated", "data"], &[&validator.id])]);
    let sel = select(&g, "validator adds validated data", 10, 1);
    assert_eq!(first_flow(&sel), vec![c, k.clone()]);
    assert!(!sel.flows.iter().any(|f| f.steps == vec![k.clone()]));
    flow_shape_ok(&sel);
}

#[test]
fn prefers_a_non_recursive_named_owner_over_a_later_callback_suffix() {
    let compose = span(node("function:compose", "compose", 15), 15, 73);
    let compose_cb = with(span(node("function:compose-callback", "<callback:ReturnStatement>", 20), 20, 30), |n| {
        n.qualified_name = "compose::<callback:ReturnStatement>".into();
        n.container_id = Some("function:compose".into());
    });
    let dispatch = span(node("function:dispatch", "dispatch", 32), 32, 71);
    let recursive_cb = with(span(node("function:dispatch-callback", "<callback:handler[1]>", 45), 45, 55), |n| {
        n.qualified_name = "dispatch::<callback:handler[1]>".into();
        n.container_id = Some("function:dispatch".into());
    });
    let c1 = contains(&compose, &compose_cb).at(20, 2);
    let cd = calls(&compose_cb, &dispatch).at(23, 4);
    let c2 = contains(&dispatch, &recursive_cb).at(45, 2);
    let rd = calls(&recursive_cb, &dispatch).at(51, 4);
    let g = G::new(vec![compose.clone(), compose_cb, dispatch.clone(), recursive_cb])
        .search_all(vec![compose.clone(), dispatch.clone()])
        .edges(vec![c1.clone(), cd.clone(), c2, rd.clone()])
        .files(&[("src/sample.ts", "ok")])
        .source(vec![hit("src/sample.ts", 15, 60, -0.2, &["compose", "dispatch", "middleware"], &[&compose.id, &dispatch.id])]);
    let sel = select(&g, "compose middleware dispatch", 10, 1);
    assert_eq!(first_flow(&sel), vec![c1, cd]);
    assert!(!all_steps(&sel).contains(&rd));
    flow_shape_ok(&sel);
}

#[test]
fn ranks_a_cohesive_fork_ahead_of_a_lexical_leafs_downstream_branch() {
    let cache = node("function:cache", "cache", 1);
    let digest = node("function:digest", "createQueryDigest", 2);
    let key = node("function:key", "createCacheKey", 3);
    let down = node("function:downstream", "cloneRawRequest", 4);
    let (cd, ck, dd) = (calls(&cache, &digest), calls(&cache, &key), calls(&digest, &down));
    let (c, d, k) = (cache.clone(), digest.clone(), key.clone());
    let g = G::new(vec![cache, digest, key, down])
        .search(move |q| match q.to_lowercase().as_str() {
            "cache" => vec![c.clone(), k.clone()],
            "digest" => vec![d.clone()],
            "key" => vec![k.clone()],
            _ => vec![d.clone(), k.clone(), c.clone()],
        })
        .outgoing_only(vec![cd.clone(), ck.clone(), dd])
        .files(&[("src/sample.ts", "ok")]);
    let sel = select(&g, "cache digest key", 10, 6);
    let firsts: Vec<EdgeInfo> = sel.flows.iter().map(|f| f.steps[0].clone()).collect();
    assert!(firsts.contains(&cd) && firsts.contains(&ck), "{:?}", sel.flows);
}

#[test]
fn promotes_an_owning_type_and_reconstructs_a_matched_functions_call_flow() {
    let plan = with(in_file(node("function:plan", "planSource", 10), "src/source.ts"), |n| {
        n.docstring = Some("Plan bounded source expansion for selected graph node identifiers.".into())
    });
    let owner = with(in_file(node("class:ledger", "BudgetLedger", 1), "src/budget.ts"), |n| n.kind = "class".into());
    let method = with(in_file(node("method:tokens", "estimatedTokens", 2), "src/budget.ts"), |n| {
        n.kind = "method".into();
        n.qualified_name = "BudgetLedger.estimatedTokens".into();
    });
    let caller = in_file(node("function:get", "runGraphGet", 4), "src/get.ts");
    let callee = in_file(node("function:read", "readNodeSource", 20), "src/read.ts");
    let make = || {
        let (m, p) = (method.clone(), plan.clone());
        let mut g = G::new(vec![plan.clone(), owner.clone(), method.clone(), caller.clone(), callee.clone()])
            .search(move |q| if q.to_lowercase().contains("token") { vec![m.clone()] } else { vec![p.clone()] });
        g.in_edges = vec![contains(&owner, &method), calls(&caller, &plan)];
        g.out_edges = vec![calls(&plan, &callee)];
        g
    };
    let budget = ids(&select(&make(), "maximum output tokens", 5, 6));
    assert!(budget.contains(&owner.id), "{:?}", budget);
    let flow = ids(&select(&make(), "planSource expands selected identifiers into bounded source", 5, 6));
    for id in [&plan.id, &caller.id, &callee.id] {
        assert!(flow.contains(id), "{:?}", flow);
    }
}

#[test]
fn reserves_an_externally_invoked_declaration_for_a_distinct_strong_intent_facet() {
    let run = |external: bool| {
        let aligned = span(in_file(node("function:aligned", "scopeRecord", 10), "src/service.ts"), 10, 30);
        let boundary = exported(in_file(node("function:boundary", "serveGraphScope", 40), "src/service.ts"));
        let ext = in_file(node("function:external", "commandRoute", 1), "src/command.ts");
        let ab = calls(&aligned, &boundary).at(20, 2);
        let eb = calls(&ext, &boundary).at(2, 2);
        let mut edges = vec![ab];
        if external {
            edges.push(eb);
        }
        let g = G::new(vec![aligned.clone(), boundary.clone(), ext])
            .search_all(vec![aligned.clone()])
            .edges(edges)
            .indexed()
            .source(vec![hit("src/service.ts", 10, 30, -0.2, &["build", "graph", "answer", "scope", "request"], &[&aligned.id])]);
        (select(&g, "build graph answer scope request", 4, 2), aligned, boundary)
    };
    let (sel, _, boundary) = run(true);
    assert_eq!(sel.candidates[0].id, boundary.id);
    assert_eq!(sel.files.iter().find(|f| f.file_path == boundary.file_path).unwrap().node_ids[0], boundary.id);
    let (sel, aligned, _) = run(false);
    assert_eq!(sel.candidates[0].id, aligned.id);
}

#[test]
fn admits_only_a_call_pair_whose_callee_adds_independent_concepts() {
    let run = |adds: bool| {
        let entry = exported(with(in_file(node("function:entry", "openGraphItem", 1), "src/adapter.ts"), |n| {
            n.signature = Some("(client: AgentClient): void".into());
            n.docstring = Some("Node source operation.".into());
        }));
        let target = with(in_file(node("function:target", if adds { "stageNodeSource" } else { "stageGraphAgent" }, 20), "src/adapter.ts"), |n| {
            n.signature = Some(if adds { "(node: GraphNode): SourceRange".into() } else { "(client: AgentClient): void".into() })
        });
        let noise: Vec<Node> = (0..35).map(|i| in_file(node(&format!("function:noise-{}", i), &format!("opaque{}", i), i + 40), &format!("src/noise-{}.ts", i))).collect();
        let task = "agent graph node source";
        let mut results = noise.clone();
        results.extend([entry.clone(), target.clone()]);
        let mut nodes = vec![entry.clone(), target.clone()];
        nodes.extend(noise);
        let g = G::new(nodes)
            .search(move |q| if q == task { results.clone() } else { Vec::new() })
            .edges(vec![calls(&entry, &target).at(8, 2)])
            .files(&[("src/adapter.ts", "ok")]);
        (select(&g, task, 4, 1), entry, target)
    };
    let (sel, entry, target) = run(true);
    assert_eq!(ids(&sel)[..2].to_vec(), vec![entry.id.clone(), target.id.clone()]);
    assert!(sel.candidates[..2].iter().all(|c| c.reasons.contains(&"bm25-call-pair".to_string())));
    let (sel, entry, target) = run(false);
    let got = ids(&sel);
    assert!(!(got.contains(&entry.id) && got.contains(&target.id)), "{:?}", got);
}

#[test]
fn bounds_whole_query_call_pair_adjacency_before_loading_outgoing_edges() {
    let task = "agent graph node source";
    let nodes: Vec<Node> = (0..80)
        .map(|i| exported(in_file(node(&format!("function:pair-bound-{:02}", i), &format!("agentGraphNodeSource{}", i), i + 1), &format!("src/pair-{}.ts", i))))
        .collect();
    let results = nodes.clone();
    let g = G::new(nodes.clone()).search(move |q| if q == task { results.clone() } else { Vec::new() });
    select(&g, task, 4, 1);
    let kinds = knobyte::graph::scope::CALL_EDGE_KINDS.join(",");
    let lookups: Vec<String> = g
        .calls_logged("out:")
        .into_iter()
        .filter(|l| l.ends_with(&format!(":{}", kinds)))
        .map(|l| l.trim_start_matches("out:").trim_end_matches(&format!(":{}", kinds)).to_string())
        .collect();
    assert_eq!(lookups, nodes[..64].iter().map(|n| n.id.clone()).collect::<Vec<_>>());
}

#[test]
fn deduplicates_node_searches_by_normalized_query_text() {
    let seed = node("function:seed", "Seed", 1);
    let g = G::new(vec![seed.clone()]).search_all(vec![seed]);
    select(&g, "Seed", 10, 6);
    let normalized: Vec<String> =
        g.calls_logged("search:").iter().map(|q| q.trim_start_matches("search:").split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()).collect();
    let unique: HashSet<&String> = normalized.iter().collect();
    assert_eq!(unique.len(), normalized.len());
    assert_eq!(normalized.iter().filter(|q| *q == "seed").count(), 1);
}

#[test]
fn loads_repeated_source_region_node_ids_once_per_selection() {
    let source = span(in_file(node("function:source", "processPipeline", 1), "src/pipeline.ts"), 1, 20);
    let g = G::new(vec![source.clone()])
        .indexed()
        .source((1..=3).map(|s| hit("src/pipeline.ts", s, s + 5, -0.2, &["process", "pipeline"], &[&source.id, &source.id])).collect());
    select(&g, "process pipeline", 10, 6);
    assert_eq!(g.calls_logged("node:"), vec![format!("node:{}", source.id)]);
}

#[test]
fn keeps_adjacency_work_fixed_when_an_exact_seed_has_hundreds_of_callers() {
    let run = |fan_in: usize| -> usize {
        let seed = node("function:seed", "Seed", 1);
        let callers: Vec<Node> = (0..fan_in).map(|i| node(&format!("function:caller-{:04}", i), &format!("caller{:04}", i), i as i64 + 2)).collect();
        let mut nodes = vec![seed.clone()];
        nodes.extend(callers.clone());
        let mut edges: Vec<EdgeInfo> = callers.iter().map(|c| calls(c, &seed)).collect();
        let mut children: Vec<Node> = Vec::new();
        for c in &callers {
            for k in 0..8 {
                let child = node(&format!("{}:child-{}", c.id, k), &format!("child{}", k), k + 1);
                edges.push(calls(c, &child));
                children.push(child);
            }
        }
        let mut g = G::new(nodes).search_all(vec![seed.clone()]);
        g.in_edges = edges.iter().filter(|e| e.target == seed.id).cloned().collect();
        g.out_edges = edges;
        // Children resolve as adjacency endpoints only (getNode knows seed and callers).
        let child_lookup = children;
        let g = G {
            nodes: {
                let mut v = g.nodes.clone();
                v.extend(child_lookup);
                v
            },
            ..g
        };
        select(&g, "Seed", 1, 6);
        g.calls_logged("in:").len() + g.calls_logged("out:").len()
    };
    let bounded = run(40);
    let extreme = run(500);
    assert_eq!(extreme, bounded);
    assert!(extreme <= 220, "{}", extreme);
}
