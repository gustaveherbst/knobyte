//! Scope retrieval: file-first hybrid ranking over independent channels fused by reciprocal
//! rank (whole-task node BM25, explicit identifiers, per-concept node search, adjacent-phrase
//! node search, source-chunk FTS, path matches and — optionally — Cozo vector similarity).
//!
//! The selection runs in admission phases:
//!
//! 1. **Semantic phrase bridges.** Each adjacent pair of query concepts (`cache invalidation`)
//!    is searched as a phrase; a hit whose identity carries both concepts and that has a
//!    compiler-trusted (confidence >= 0.8) call/reference into *another file* whose target also
//!    carries both concepts reserves that destination file (at most two bridges, one per
//!    target file). Same-file, low-confidence and comment-only matches never qualify.
//! 2. **Source-region alignment.** Every source-chunk hit is bridged to the declarations it
//!    overlaps; compiler-resolved callsites inside the strongest regions are remembered so the
//!    callee can inherit the region's evidence.
//! 3. **Whole-query call pairs.** A public declaration ranked by the whole-task BM25 channel
//!    that calls another whole-task hit adding an independent concept is reserved as a pair.
//! 4. **Seeds and gated expansion.** Phrase sources, call pairs, callback owners and callsite
//!    regions seed first, then one seed per file, then the global pool; expansion follows only
//!    trusted typed edges for two hops and propagates callsite evidence to callees.
//! 5. **File admission.** Explicit identifiers pin a file; phrase destinations come next; a
//!    bounded source floor (direct FTS files plus the best propagated callsite destination)
//!    is reserved; strong compound declarations fill the graph slots; the fused ranking fills
//!    the rest. The primary directed flow may then evict the weakest unprotected file.
//! 6. **Declaration admission.** Explicit identifiers, call-pair endpoints, the best
//!    whole-query declaration, strong intent facets and each file's best source-aligned
//!    declaration are reserved before per-file representatives and the global fill.
//! 7. **Flows.** Directed call paths with provenance reserves (query-region callsites,
//!    named-owner callback repair, call-pair callback completion) under an eight-step budget.
//!
//! Every ordering decision has a deterministic tie-break (score, then path / id). Graph access
//! goes through [`ScopeGraph`], so the ranking can run on the SQLite index or a fixture graph.

use rusqlite::{params, Connection, OptionalExtension, Result};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;

use crate::graph::chunks::{search_chunks, ChunkHit};
use crate::graph::engine::{map_node_row, NODE_COLUMNS, NODE_COLUMNS_N};
use crate::graph::models::Node;
use crate::graph::query_plan::{
    component_matches, idf, is_low_value_graph_path, is_test_path, plan_graph_query, search_components,
    GraphQueryPlan, QueryConcept,
};

const RRF_K: f64 = 60.0;
const SOURCE_FILE_RRF_WEIGHT: f64 = 30.0;
/// Edge kinds followed by relevance expansion.
pub const RELEVANCE_EDGE_KINDS: &[&str] = &[
    "calls", "calls_trait_method", "instantiates", "references", "imports", "exports", "extends",
    "implements", "overrides", "type_of", "returns", "contains", "impl_of", "aliases",
];
/// Edge kinds that make a directed flow.
pub const FLOW_EDGE_KINDS: &[&str] = &["calls", "calls_trait_method", "instantiates", "references"];
/// Flow edges plus the one structural edge that may enter an anonymous callback.
pub const FLOW_TRAVERSAL_EDGE_KINDS: &[&str] =
    &["calls", "calls_trait_method", "instantiates", "references", "contains"];
/// Executable call edges (call-pair proof, callback completion).
pub const CALL_EDGE_KINDS: &[&str] = &["calls", "calls_trait_method", "instantiates"];
const STRONG_FLOW_RELEVANCE: f64 = 2.0;
const SOURCE_NEAR_CUTOFF_RATIO: f64 = 0.85;
const MAX_FLOW_SEEDS: usize = 32;
const FLOW_TRAVERSAL_WORK_BUDGET: usize = 256;
const FLOW_BRANCH_LIMIT: usize = 8;
const MAX_FLOW_STEPS: usize = 8;
const CALL_PAIR_CALLBACKS_LIMIT: usize = 2;
const CALL_PAIR_CALLBACK_EDGES_LIMIT: usize = 4;
const MIN_CALL_PAIR_CALLBACK_TERMINAL_NAME_RELEVANCE: f64 = 1.0;
const MAX_SEMANTIC_PHRASE_SEARCHES: usize = 8;
const MAX_SEMANTIC_PHRASE_CANDIDATES: usize = 8;
const MAX_SEMANTIC_PHRASE_OUTGOING_PER_SOURCE: usize = 16;
const MAX_SEMANTIC_PHRASE_BRIDGES: usize = 2;
const MIN_SEMANTIC_PHRASE_CONCEPT_WEIGHT: f64 = 1.0;
const MAX_FULL_QUERY_CALL_PAIR_RANK: usize = 64;
const MAX_FULL_QUERY_CALL_PAIR_SEEDS: usize = 2;
const MAX_STRONG_INTENT_FACET_DECLARATIONS: usize = 2;
const MIN_TRUSTED_CONFIDENCE: f64 = 0.8;
const MAX_TEST_NEIGHBOURS: usize = 3;
/// Rows read per adjacency lookup (hub guard).
const MAX_ADJACENCY_ROWS: usize = 500;
/// Node kinds never returned as scope candidates.
const NON_CANDIDATE_KINDS: &str = "'file', 'module', 'parameter'";
const INF: usize = usize::MAX;

/// A graph edge as seen by retrieval.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EdgeInfo {
    pub source: String,
    pub target: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<i64>,
    pub confidence: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
}

impl EdgeInfo {
    /// A trusted edge between two ids (tests and fixtures).
    pub fn new(source: &str, target: &str, kind: &str) -> Self {
        Self {
            source: source.to_string(),
            target: target.to_string(),
            kind: kind.to_string(),
            line: None,
            column: None,
            confidence: 1.0,
            resolution_method: None,
            provenance: None,
        }
    }

    /// The same edge at a call site.
    pub fn at(mut self, line: i64, column: i64) -> Self {
        self.line = Some(line);
        self.column = Some(column);
        self
    }

    pub fn with_confidence(mut self, confidence: f64) -> Self {
        self.confidence = confidence;
        self
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScopedCandidate {
    pub id: String,
    pub score: f64,
    pub reasons: Vec<String>,
    /// `direct`, `neighbor` or `test`.
    pub category: &'static str,
    /// Human-readable explanations of graph relationships that selected it.
    pub explanations: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RankedScopeFile {
    pub file_path: String,
    pub score: f64,
    pub reasons: Vec<String>,
    pub node_ids: Vec<String>,
    pub text_hits: Vec<ChunkHit>,
    pub text_only: bool,
    pub parse_status: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ScopeFlow {
    pub steps: Vec<EdgeInfo>,
}

#[derive(Debug, Clone, Default)]
pub struct ScopeSelection {
    pub candidates: Vec<ScopedCandidate>,
    pub files: Vec<RankedScopeFile>,
    pub flows: Vec<ScopeFlow>,
    /// Test declarations exercising the selected nodes (test-neighbour inclusion).
    pub tests: Vec<ScopedCandidate>,
    pub matched_count: usize,
    pub covered_terms: Vec<String>,
    /// `strong`, `moderate`, `weak` or `none`.
    pub evidence_strength: &'static str,
    /// Every node referenced by candidates, files and flows.
    pub nodes: HashMap<String, Node>,
    pub plan: GraphQueryPlan,
}

/// Options of one selection.
#[derive(Debug, Clone, Default)]
pub struct ScopeRequest {
    pub max_nodes: usize,
    pub max_files: usize,
    /// Optional semantic channel: (node id, similarity) from a vector index, best first.
    pub vector_hits: Vec<(String, f64)>,
}

// ---------------------------------------------------------------------------
// Graph access
// ---------------------------------------------------------------------------

/// The read operations scope retrieval needs. [`SqliteScopeGraph`] serves them from the index;
/// tests can serve them from a fixture.
pub trait ScopeGraph {
    /// Lexical declaration search, best first.
    fn search_nodes(&self, query: &str, limit: usize) -> Vec<Node>;
    /// Source-chunk search for `query` (planned as `terms`: term, weight, prefix), best first.
    fn search_source(&self, query: &str, terms: &[(String, f64, bool)], limit: usize) -> Vec<ChunkHit>;
    fn node(&self, id: &str) -> Option<Node>;
    /// Edges into `id` of `kinds`, with their source nodes.
    fn incoming(&self, id: &str, kinds: &[&str]) -> Vec<(Node, EdgeInfo)>;
    /// Edges out of `id` of `kinds`, with their target nodes.
    fn outgoing(&self, id: &str, kinds: &[&str]) -> Vec<(Node, EdgeInfo)>;
    /// (path, parse status) of every indexed file.
    fn indexed_files(&self) -> Vec<(String, String)>;
    /// Declarations whose name or qualified name is exactly `identifier` (an index-level
    /// fallback for explicit identifiers the lexical search ranked out).
    fn nodes_named(&self, _identifier: &str) -> Vec<Node> {
        Vec::new()
    }
}

/// [`ScopeGraph`] over the SQLite graph index.
pub struct SqliteScopeGraph<'a> {
    pub conn: &'a Connection,
}

impl SqliteScopeGraph<'_> {
    fn adjacency(&self, id: &str, kinds: &[&str], outgoing: bool) -> Vec<(Node, EdgeInfo)> {
        if kinds.is_empty() {
            return Vec::new();
        }
        let (join, filter) = if outgoing { ("e.target", "e.source") } else { ("e.source", "e.target") };
        let kinds = kinds.iter().map(|k| format!("'{}'", k.replace('\'', ""))).collect::<Vec<_>>().join(", ");
        let sql = format!(
            "SELECT {cols}, e.source, e.target, e.kind, e.line, e.col, e.confidence, e.resolution_method, e.provenance \
             FROM edges e JOIN nodes n ON n.id = {join} WHERE {filter} = ?1 AND e.kind IN ({kinds}) \
             ORDER BY e.line, e.col, n.id, e.id LIMIT {limit}",
            cols = NODE_COLUMNS_N,
            limit = MAX_ADJACENCY_ROWS
        );
        self.conn
            .prepare_cached(&sql)
            .and_then(|mut s| {
                s.query_map(params![id], |r| {
                    Ok((
                        map_node_row(r)?,
                        EdgeInfo {
                            source: r.get(22)?,
                            target: r.get(23)?,
                            kind: r.get(24)?,
                            line: r.get(25)?,
                            column: r.get(26)?,
                            confidence: r.get::<_, Option<f64>>(27)?.unwrap_or(1.0),
                            resolution_method: r.get(28)?,
                            provenance: r.get(29)?,
                        },
                    ))
                })
                .map(|it| it.flatten().collect())
            })
            .unwrap_or_default()
    }
}

impl ScopeGraph for SqliteScopeGraph<'_> {
    fn search_nodes(&self, query: &str, limit: usize) -> Vec<Node> {
        search_nodes(self.conn, query, limit)
    }

    fn search_source(&self, _query: &str, terms: &[(String, f64, bool)], limit: usize) -> Vec<ChunkHit> {
        search_chunks(self.conn, terms, limit)
    }

    fn node(&self, id: &str) -> Option<Node> {
        self.conn
            .query_row(&format!("SELECT {} FROM nodes WHERE id = ?1", NODE_COLUMNS), params![id], map_node_row)
            .optional()
            .ok()
            .flatten()
    }

    fn incoming(&self, id: &str, kinds: &[&str]) -> Vec<(Node, EdgeInfo)> {
        self.adjacency(id, kinds, false)
    }

    fn outgoing(&self, id: &str, kinds: &[&str]) -> Vec<(Node, EdgeInfo)> {
        self.adjacency(id, kinds, true)
    }

    fn indexed_files(&self) -> Vec<(String, String)> {
        self.conn
            .prepare_cached("SELECT path, parse_status FROM files ORDER BY path")
            .and_then(|mut s| s.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).map(|r| r.flatten().collect()))
            .unwrap_or_default()
    }

    fn nodes_named(&self, identifier: &str) -> Vec<Node> {
        self.conn
            .prepare_cached(&format!(
                "SELECT {} FROM nodes WHERE (name = ?1 OR qualified_name = ?1) AND kind NOT IN ({}) ORDER BY id LIMIT 20",
                NODE_COLUMNS, NON_CANDIDATE_KINDS
            ))
            .and_then(|mut s| s.query_map(params![identifier], map_node_row).map(|r| r.flatten().collect::<Vec<_>>()))
            .unwrap_or_default()
    }
}

/// Per-request cache over a [`ScopeGraph`]: every node and adjacency list is loaded once.
pub(crate) struct Access<'a> {
    graph: &'a dyn ScopeGraph,
    nodes: RefCell<HashMap<String, Option<Node>>>,
    #[allow(clippy::type_complexity)]
    adjacency: RefCell<HashMap<(String, bool, String), Vec<(Node, EdgeInfo)>>>,
}

impl<'a> Access<'a> {
    pub fn new(graph: &'a dyn ScopeGraph) -> Self {
        Self {
            graph,
            nodes: RefCell::new(HashMap::new()),
            adjacency: RefCell::new(HashMap::new()),
        }
    }

    pub fn remember(&self, node: &Node) {
        self.nodes.borrow_mut().insert(node.id.clone(), Some(node.clone()));
    }

    pub fn node(&self, id: &str) -> Option<Node> {
        if let Some(n) = self.nodes.borrow().get(id) {
            return n.clone();
        }
        let n = self.graph.node(id);
        self.nodes.borrow_mut().insert(id.to_string(), n.clone());
        n
    }

    fn cached(&self, id: &str, kinds: &[&str], outgoing: bool) -> Vec<(Node, EdgeInfo)> {
        let mut sorted: Vec<&str> = kinds.to_vec();
        sorted.sort();
        sorted.dedup();
        let key = (id.to_string(), outgoing, sorted.join(","));
        if let Some(v) = self.adjacency.borrow().get(&key) {
            return v.clone();
        }
        let rows = if outgoing { self.graph.outgoing(id, kinds) } else { self.graph.incoming(id, kinds) };
        {
            let mut nodes = self.nodes.borrow_mut();
            for (n, _) in &rows {
                nodes.insert(n.id.clone(), Some(n.clone()));
            }
        }
        self.adjacency.borrow_mut().insert(key, rows.clone());
        rows
    }

    pub fn incoming(&self, id: &str, kinds: &[&str]) -> Vec<(Node, EdgeInfo)> {
        self.cached(id, kinds, false)
    }

    pub fn outgoing(&self, id: &str, kinds: &[&str]) -> Vec<(Node, EdgeInfo)> {
        self.cached(id, kinds, true)
    }
}

/// Lexical node search: FTS5 BM25 over names/signatures/docs, then a name-component pass that
/// reaches camelCase / snake_case inner components FTS tokenisation cannot.
pub fn search_nodes(conn: &Connection, query: &str, limit: usize) -> Vec<Node> {
    let plan = plan_graph_query(query);
    let mut terms: Vec<String> = Vec::new();
    for t in &plan.terms {
        if !terms.contains(&t.term) {
            terms.push(t.term.clone());
        }
    }
    if terms.is_empty() {
        return Vec::new();
    }
    let fts = terms
        .iter()
        .map(|t| format!("\"{}\"*", t.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" OR ");
    let mut out: Vec<Node> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let sql = format!(
        "SELECT {cols} FROM nodes_fts f JOIN nodes n ON n.rowid = f.rowid \
         WHERE nodes_fts MATCH ?1 AND n.kind NOT IN ({skip}) \
         ORDER BY bm25(nodes_fts, 0.0, 10.0, 4.0, 1.0, 2.0), n.id LIMIT ?2",
        cols = NODE_COLUMNS_N,
        skip = NON_CANDIDATE_KINDS
    );
    if let Ok(mut stmt) = conn.prepare_cached(&sql) {
        if let Ok(rows) = stmt.query_map(params![fts, (limit * 2) as i64], map_node_row) {
            for n in rows.flatten() {
                if seen.insert(n.id.clone()) {
                    out.push(n);
                }
            }
        }
    }
    let like_sql = format!(
        "SELECT {cols} FROM nodes WHERE lower(name) LIKE ?1 AND kind NOT IN ({skip}) \
         ORDER BY length(name), id LIMIT 24",
        cols = NODE_COLUMNS,
        skip = NON_CANDIDATE_KINDS
    );
    if let Ok(mut stmt) = conn.prepare_cached(&like_sql) {
        for t in plan.terms.iter().filter(|t| !t.stem && t.term.len() >= 4) {
            if let Ok(rows) = stmt.query_map(params![format!("%{}%", t.term)], map_node_row) {
                for n in rows.flatten() {
                    let comps = search_components(&n.name);
                    if comps.contains(&t.term) && seen.insert(n.id.clone()) {
                        out.push(n);
                    }
                }
            }
        }
    }
    out.truncate(limit);
    out
}

// ---------------------------------------------------------------------------
// Evidence
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct WTerm {
    term: String,
    weight: f64,
    stem: bool,
}

#[derive(Debug, Clone)]
struct NodeEv {
    node: Node,
    rrf: f64,
    /// Best zero-based rank in the whole-task node BM25 channel.
    full_query_rank: usize,
    region: f64,
    direct_region: f64,
    /// Indices into the request's chunk hits.
    region_hits: Vec<usize>,
    best_region_rank: usize,
    best_region_overlap_rank: usize,
    /// Trusted callsites inside matched regions (edge keys).
    region_callsites: BTreeSet<String>,
    /// Trusted structural entries into one anonymous callback within a matched region.
    region_callback_callsites: BTreeSet<String>,
    /// Best source-chunk rank of a trusted callsite that reached this node.
    best_callsite_rank: usize,
    exact: bool,
    exact_ids: BTreeSet<String>,
    graph: f64,
    terms: BTreeSet<String>,
    name_terms: BTreeSet<String>,
    reasons: BTreeSet<String>,
    reliable: bool,
    /// Human-readable relationship explanations (`called by `x``).
    explanations: BTreeSet<String>,
    vector: f64,
}

impl NodeEv {
    fn new(node: Node) -> Self {
        Self {
            node,
            rrf: 0.0,
            full_query_rank: INF,
            region: 0.0,
            direct_region: 0.0,
            region_hits: Vec::new(),
            best_region_rank: INF,
            best_region_overlap_rank: INF,
            region_callsites: BTreeSet::new(),
            region_callback_callsites: BTreeSet::new(),
            best_callsite_rank: INF,
            exact: false,
            exact_ids: BTreeSet::new(),
            graph: 0.0,
            terms: BTreeSet::new(),
            name_terms: BTreeSet::new(),
            reasons: BTreeSet::new(),
            reliable: false,
            explanations: BTreeSet::new(),
            vector: 0.0,
        }
    }

    /// Repeated overlapping windows are supporting evidence, not independent votes.
    fn region_strength(&self) -> f64 {
        self.region + (self.region_hits.len().saturating_sub(1) as f64 * 0.06).min(0.3)
    }

    fn direct_region_strength(&self) -> f64 {
        self.direct_region + (self.region_hits.len().saturating_sub(1) as f64 * 0.06).min(0.3)
    }

    fn fused(&self) -> f64 {
        self.rrf + self.graph + self.vector
    }
}

#[derive(Debug, Clone, Default)]
struct FileEv {
    score: f64,
    region: f64,
    callsite_region: f64,
    callsite_query_weight: f64,
    best_callsite_rank: Option<usize>,
    terms: BTreeSet<String>,
    reasons: BTreeSet<String>,
    node_ids: BTreeSet<String>,
    text_hits: Vec<usize>,
    best_text_rank: Option<usize>,
    exact: bool,
    exact_ids: BTreeSet<String>,
    reliable_graph: bool,
}

type Nodes = BTreeMap<String, NodeEv>;
type Files = BTreeMap<String, FileEv>;

fn ensure_node<'m>(nodes: &'m mut Nodes, node: &Node) -> &'m mut NodeEv {
    nodes.entry(node.id.clone()).or_insert_with(|| NodeEv::new(node.clone()))
}

fn ensure_file<'m>(files: &'m mut Files, path: &str) -> &'m mut FileEv {
    files.entry(path.to_string()).or_default()
}

fn cmp_f64(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

fn node_contains(node: &Node, term: &str, prefix: bool) -> bool {
    let mut comps = search_components(&node.name);
    comps.extend(search_components(&node.qualified_name));
    comps.extend(search_components(node.signature.as_deref().unwrap_or("")));
    comps.extend(search_components(node.docstring.as_deref().unwrap_or("")));
    component_matches(&comps, term, prefix)
}

fn concept_matches_terms(c: &QueryConcept, terms: &BTreeSet<String>) -> bool {
    c.terms.iter().any(|(t, _)| terms.contains(t))
}

fn concept_matches_components(c: &QueryConcept, comps: &HashSet<String>) -> bool {
    c.terms.iter().any(|(t, s)| component_matches(comps, t, *s))
}

fn node_matches_concept(node: &Node, c: &QueryConcept) -> bool {
    c.terms.iter().any(|(t, s)| node_contains(node, t, *s))
}

/// The concept appears in the declaration's identity (name, qualified name, signature), not
/// only in its comments.
fn node_matches_concept_in_identity(node: &Node, c: &QueryConcept) -> bool {
    let mut comps = search_components(&node.name);
    comps.extend(search_components(&node.qualified_name));
    comps.extend(search_components(node.signature.as_deref().unwrap_or("")));
    concept_matches_components(c, &comps)
}

fn add_concept_evidence(e: &mut NodeEv, c: &QueryConcept) {
    let name_comps = search_components(&e.node.name);
    for (t, s) in &c.terms {
        if node_contains(&e.node, t, *s) {
            e.terms.insert(t.clone());
        }
        if component_matches(&name_comps, t, *s) {
            e.name_terms.insert(t.clone());
        }
    }
}

fn add_term_evidence(e: &mut NodeEv, node: &Node, weighted: &[WTerm]) {
    let name_comps = search_components(&node.name);
    for t in weighted {
        if node_contains(node, &t.term, t.stem) {
            e.terms.insert(t.term.clone());
        }
        if component_matches(&name_comps, &t.term, t.stem) {
            e.name_terms.insert(t.term.clone());
        }
    }
}

fn exact_identifier_match(node: &Node, identifier: &str, fold: bool) -> bool {
    let norm = |s: &str| {
        let s = s.replace('.', "::");
        if fold {
            s.to_lowercase()
        } else {
            s
        }
    };
    let wanted = norm(identifier);
    let name = if fold { node.name.to_lowercase() } else { node.name.clone() };
    let q = norm(&node.qualified_name);
    name == wanted || q == wanted || q.ends_with(&format!("::{}", wanted))
}

/// `Seed`, `Leaf`: a single capitalised word may match case-insensitively.
fn is_capitalised_word(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f.is_ascii_uppercase()) && s.len() > 1 && c.all(|ch| ch.is_ascii_lowercase())
}

fn is_unnamed_flow_node(node: &Node) -> bool {
    node.name.is_empty() || (node.name.starts_with('<') && node.name.ends_with('>')) || node.name.starts_with("<callback:")
}

fn is_source_anchor_kind(node: &Node) -> bool {
    if node.name.starts_with("<callback:") {
        return false;
    }
    matches!(
        node.kind.as_str(),
        "function" | "method" | "class" | "struct" | "trait" | "component" | "route" | "interface" | "protocol"
            | "type_alias"
    )
}

fn is_non_candidate(node: &Node) -> bool {
    matches!(node.kind.as_str(), "file" | "module" | "parameter")
}

fn is_public(node: &Node) -> bool {
    node.is_exported || node.visibility.as_deref() == Some("public")
}

fn selection_priority(node: &Node) -> f64 {
    if node.name.starts_with("<callback:") {
        return 0.0;
    }
    match node.kind.as_str() {
        "function" | "method" | "class" | "struct" | "trait" | "component" | "impl" => 3.0,
        "interface" | "protocol" | "type_alias" | "enum" | "namespace" => 2.0,
        "constant" | "variable" | "route" | "table" | "view" => 1.5,
        "property" | "field" | "parameter" => 1.0,
        _ => 0.5,
    }
}

fn is_call_kind(kind: &str) -> bool {
    CALL_EDGE_KINDS.contains(&kind)
}

fn compare_ev(a: &NodeEv, b: &NodeEv) -> std::cmp::Ordering {
    b.exact
        .cmp(&a.exact)
        .then(cmp_f64(selection_priority(&b.node), selection_priority(&a.node)))
        .then(a.best_callsite_rank.cmp(&b.best_callsite_rank))
        .then(cmp_f64(b.region_strength(), a.region_strength()))
        .then(b.name_terms.len().cmp(&a.name_terms.len()))
        .then(b.terms.len().cmp(&a.terms.len()))
        .then(cmp_f64(b.fused(), a.fused()))
        .then(a.node.id.cmp(&b.node.id))
}

fn is_reliable_lexical_seed(e: &NodeEv) -> bool {
    if e.exact {
        return true;
    }
    let term_reasons: Vec<&str> = e.reasons.iter().filter_map(|r| r.strip_prefix("term:")).collect();
    if term_reasons.is_empty() || e.name_terms.is_empty() {
        return false;
    }
    let name = e.node.name.trim_start_matches('#').to_lowercase();
    if term_reasons.contains(&name.as_str()) {
        return true;
    }
    // BM25 plus a distinctive name component (or two ordinary components) is a corroborated
    // symbol hit. Docstring/signature-only hits never qualify.
    let generic = ["class", "code", "file", "function", "graph", "method", "node", "source", "symbol"];
    let matched: Vec<&&str> = term_reasons.iter().filter(|t| e.name_terms.contains(**t)).collect();
    e.reasons.contains("bm25-node") && (matched.len() >= 2 || matched.iter().any(|t| t.len() >= 5 && !generic.contains(*t)))
}

fn is_reliable_region_seed(e: &NodeEv, hits: &[ChunkHit]) -> bool {
    let distinct = |h: usize| hits[h].matched_terms.iter().collect::<HashSet<_>>().len();
    let primary_body = e.best_region_overlap_rank == 0 && e.region_hits.iter().any(|&h| distinct(h) >= 3);
    if e.region_strength() >= 0.55 && (!e.name_terms.is_empty() || primary_body) {
        return true;
    }
    // A named declaration that owns an anonymous callback inside a two-concept hit.
    !e.region_callback_callsites.is_empty()
        && e.direct_region_strength() >= 0.4
        && e.region_hits.iter().any(|&h| distinct(h) >= 2)
}

/// Only declarations with declaration-local lexical evidence or a trusted relationship become
/// candidates; source-chunk terms describe a whole window, not each overlapping sibling.
fn trustworthy(e: &NodeEv, hits: &[ChunkHit]) -> bool {
    e.exact || e.reliable || is_reliable_lexical_seed(e) || is_reliable_region_seed(e, hits) || e.vector > 0.0
}

fn edge_key(e: &EdgeInfo) -> String {
    format!(
        "{}\0{}\0{}\0{}\0{}",
        e.source,
        e.target,
        e.kind,
        e.line.unwrap_or(-1),
        e.column.unwrap_or(-1)
    )
}

fn flow_key(steps: &[EdgeInfo]) -> String {
    steps.iter().map(edge_key).collect::<Vec<_>>().join("\n")
}

/// Total callsite order; storage order never decides.
fn compare_edge_callsite(a: &EdgeInfo, b: &EdgeInfo) -> std::cmp::Ordering {
    a.line
        .unwrap_or(i64::MAX)
        .cmp(&b.line.unwrap_or(i64::MAX))
        .then(a.column.unwrap_or(i64::MAX).cmp(&b.column.unwrap_or(i64::MAX)))
        .then(a.source.cmp(&b.source))
        .then(a.target.cmp(&b.target))
        .then(a.kind.cmp(&b.kind))
        .then(a.resolution_method.as_deref().unwrap_or("").cmp(b.resolution_method.as_deref().unwrap_or("")))
        .then(a.provenance.as_deref().unwrap_or("").cmp(b.provenance.as_deref().unwrap_or("")))
}

fn compare_neighbor(a: &(Node, EdgeInfo), b: &(Node, EdgeInfo)) -> std::cmp::Ordering {
    cmp_f64(b.1.confidence, a.1.confidence)
        .then(is_low_value_graph_path(&a.0.file_path).cmp(&is_low_value_graph_path(&b.0.file_path)))
        .then(a.1.line.unwrap_or(i64::MAX).cmp(&b.1.line.unwrap_or(i64::MAX)))
        .then(a.0.id.cmp(&b.0.id))
}

fn line_in_hit(line: Option<i64>, hit: &ChunkHit) -> bool {
    line.is_some_and(|l| l >= hit.start_line && l <= hit.end_line)
}

fn query_callsite_hit(current_id: &str, edge: &EdgeInfo, region_hits: &[usize], hits: &[ChunkHit]) -> bool {
    edge.source == current_id && edge.line.is_some() && region_hits.iter().any(|&h| line_in_hit(edge.line, &hits[h]))
}

fn query_callsite_weight(edge: &EdgeInfo, region_hits: &[usize], hits: &[ChunkHit], concepts: &[QueryConcept]) -> f64 {
    if edge.line.is_none() {
        return 0.0;
    }
    region_hits
        .iter()
        .filter(|&&h| line_in_hit(edge.line, &hits[h]))
        .map(|&h| {
            let ts: BTreeSet<String> = hits[h].matched_terms.iter().cloned().collect();
            concepts.iter().filter(|c| concept_matches_terms(c, &ts)).map(|c| c.weight).sum::<f64>()
        })
        .fold(0.0, f64::max)
}

/// Trusted relevance neighbours. Containment may promote a declaration to its non-file owner or
/// enter one anonymous callback; it never fans out from a file/class into named children.
fn reliable_neighbors(access: &Access, id: &str) -> Vec<(Node, EdgeInfo)> {
    let current = access.node(id);
    let current_is_file = current.as_ref().is_some_and(|c| c.kind == "file");
    let mut out: Vec<(Node, EdgeInfo)> = access
        .incoming(id, RELEVANCE_EDGE_KINDS)
        .into_iter()
        .chain(access.outgoing(id, RELEVANCE_EDGE_KINDS))
        .filter(|(n, e)| {
            if e.confidence < MIN_TRUSTED_CONFIDENCE || is_non_candidate(n) {
                return false;
            }
            if e.kind != "contains" {
                return true;
            }
            if e.source == id {
                !current_is_file && is_unnamed_flow_node(n)
            } else {
                n.kind != "file" && !current_is_file
            }
        })
        .collect();
    out.sort_by(compare_neighbor);
    out
}

fn compare_query_neighbor(
    current_id: &str,
    a: &(Node, EdgeInfo),
    b: &(Node, EdgeInfo),
    file_hits: &[usize],
    hits: &[ChunkHit],
    terms: &[WTerm],
) -> std::cmp::Ordering {
    let callsite = |x: &(Node, EdgeInfo)| -> u8 {
        if x.1.source != current_id || x.1.line.is_none() {
            return 0;
        }
        u8::from(file_hits.iter().any(|&h| line_in_hit(x.1.line, &hits[h])))
    };
    let name = |x: &(Node, EdgeInfo)| -> f64 {
        let cs = search_components(&x.0.name);
        terms.iter().filter(|t| component_matches(&cs, &t.term, t.stem)).map(|t| t.weight).sum()
    };
    callsite(b)
        .cmp(&callsite(a))
        .then(cmp_f64(name(b), name(a)))
        .then(compare_neighbor(a, b))
}

fn compare_semantic_phrase_neighbor(a: &(Node, EdgeInfo), b: &(Node, EdgeInfo)) -> std::cmp::Ordering {
    cmp_f64(b.1.confidence, a.1.confidence)
        .then(compare_edge_callsite(&a.1, &b.1))
        .then(a.0.file_path.cmp(&b.0.file_path))
        .then(a.0.id.cmp(&b.0.id))
}

fn normalize_node_search(q: &str) -> String {
    q.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn weighted_terms(plan: &GraphQueryPlan) -> Vec<WTerm> {
    let mut out: Vec<WTerm> = Vec::new();
    for t in &plan.terms {
        match out.iter_mut().find(|w| w.term == t.term) {
            Some(w) => {
                w.weight = w.weight.max(t.weight);
                w.stem = w.stem && t.stem;
            }
            None => out.push(WTerm {
                term: t.term.clone(),
                weight: t.weight,
                stem: t.stem,
            }),
        }
    }
    out
}

fn component_idf_signal(comps: &HashSet<String>, concepts: &[QueryConcept], concept_idf: &dyn Fn(&QueryConcept) -> f64) -> f64 {
    comps
        .iter()
        .map(|comp| {
            let one: HashSet<String> = [comp.clone()].into_iter().collect();
            concepts
                .iter()
                .filter(|c| concept_matches_components(c, &one))
                .map(concept_idf)
                .fold(0.0, f64::max)
        })
        .sum()
}

// ---------------------------------------------------------------------------
// Reserved declarations
// ---------------------------------------------------------------------------

/// A whole-query call pair: a public whole-task hit calling another whole-task hit that adds
/// an independent concept.
#[derive(Debug, Clone)]
struct CallPair {
    source: String,
    target: String,
    edge: EdgeInfo,
}

fn best_full_query_call_pairs(nodes: &Nodes, concepts: &[QueryConcept], access: &Access, limit: usize) -> Vec<CallPair> {
    let mut eligible: Vec<&NodeEv> = nodes
        .values()
        .filter(|e| {
            e.full_query_rank < MAX_FULL_QUERY_CALL_PAIR_RANK
                && !is_low_value_graph_path(&e.node.file_path)
                && is_source_anchor_kind(&e.node)
                && is_public(&e.node)
        })
        .collect();
    eligible.sort_by(|a, b| a.full_query_rank.cmp(&b.full_query_rank).then(a.node.id.cmp(&b.node.id)));
    eligible.truncate(MAX_FULL_QUERY_CALL_PAIR_RANK);
    struct Scored<'n> {
        entry: &'n NodeEv,
        declaration: usize,
        identity: usize,
        weight: f64,
        strong: usize,
        target: Option<(&'n NodeEv, EdgeInfo, f64, usize)>,
    }
    let mut scored: Vec<Scored> = eligible
        .into_iter()
        .map(|entry| {
            let declaration: Vec<&QueryConcept> = concepts.iter().filter(|c| node_matches_concept(&entry.node, c)).collect();
            let identity: Vec<&QueryConcept> =
                concepts.iter().filter(|c| node_matches_concept_in_identity(&entry.node, c)).collect();
            let weight: f64 = declaration.iter().map(|c| c.weight).sum();
            let strong = declaration.iter().filter(|c| c.weight >= 1.0).count();
            let identity_keys: HashSet<&str> = identity.iter().map(|c| c.key.as_str()).collect();
            let mut targets: Vec<(&NodeEv, EdgeInfo, f64, usize)> = access
                .outgoing(&entry.node.id, CALL_EDGE_KINDS)
                .into_iter()
                .filter(|(_, e)| e.confidence >= MIN_TRUSTED_CONFIDENCE)
                .filter_map(|(n, e)| {
                    let target = nodes.get(&n.id)?;
                    if target.full_query_rank >= MAX_FULL_QUERY_CALL_PAIR_RANK
                        || is_low_value_graph_path(&target.node.file_path)
                        || !is_source_anchor_kind(&target.node)
                    {
                        return None;
                    }
                    let target_concepts: Vec<&QueryConcept> =
                        concepts.iter().filter(|c| node_matches_concept_in_identity(&target.node, c)).collect();
                    if target_concepts.len() < 2 || !target_concepts.iter().any(|c| !identity_keys.contains(c.key.as_str())) {
                        return None;
                    }
                    let mut union: BTreeMap<&str, &QueryConcept> = BTreeMap::new();
                    for c in identity.iter().chain(target_concepts.iter()) {
                        union.insert(c.key.as_str(), c);
                    }
                    let union_weight: f64 = union.values().map(|c| c.weight).sum();
                    let union_strong = union.values().filter(|c| c.weight >= 1.0).count();
                    if !((union_strong > 0 && union.len() >= 3 && union_weight >= 1.7)
                        || (union.len() >= 4 && union_weight >= 1.4))
                    {
                        return None;
                    }
                    Some((target, e, union_weight, union.len()))
                })
                .collect();
            targets.sort_by(|a, b| {
                a.0.full_query_rank
                    .cmp(&b.0.full_query_rank)
                    .then(cmp_f64(b.2, a.2))
                    .then(b.3.cmp(&a.3))
                    .then(cmp_f64(b.1.confidence, a.1.confidence))
                    .then(compare_edge_callsite(&a.1, &b.1))
            });
            Scored {
                entry,
                declaration: declaration.len(),
                identity: identity.len(),
                weight,
                strong,
                target: targets.into_iter().next(),
            }
        })
        .filter(|s| {
            s.identity > 0
                && s.declaration >= 3
                && ((s.strong > 0 && s.weight >= 1.7) || (s.declaration >= 4 && s.identity >= 2 && s.weight >= 1.4))
                && s.target.is_some()
        })
        .collect();
    scored.sort_by(|a, b| {
        a.entry
            .full_query_rank
            .cmp(&b.entry.full_query_rank)
            .then(a.target.as_ref().unwrap().0.full_query_rank.cmp(&b.target.as_ref().unwrap().0.full_query_rank))
            .then(b.strong.cmp(&a.strong))
            .then(cmp_f64(b.weight, a.weight))
            .then(b.identity.cmp(&a.identity))
            .then(compare_ev(a.entry, b.entry))
    });
    scored
        .into_iter()
        .take(limit / 2)
        .map(|s| {
            let (t, e, _, _) = s.target.unwrap();
            CallPair {
                source: s.entry.node.id.clone(),
                target: t.node.id.clone(),
                edge: e,
            }
        })
        .collect()
}

/// Declarations for distinct strong responsibilities of a compound request.
fn best_strong_intent_facets(entries: &[&NodeEv], concepts: &[QueryConcept], access: &Access, limit: usize) -> Vec<String> {
    struct Cand<'n> {
        entry: &'n NodeEv,
        matched: usize,
        strong: Vec<(String, f64)>,
        weight: f64,
    }
    let mut lexical: Vec<Cand> = entries
        .iter()
        .map(|e| {
            let matched: Vec<&QueryConcept> = concepts.iter().filter(|c| node_matches_concept_in_identity(&e.node, c)).collect();
            let strong: Vec<(String, f64)> = matched
                .iter()
                .filter(|c| c.weight >= MIN_SEMANTIC_PHRASE_CONCEPT_WEIGHT)
                .map(|c| (c.key.clone(), c.weight))
                .collect();
            let weight: f64 = matched.iter().map(|c| c.weight).sum();
            let has_low = matched.iter().any(|c| c.weight < MIN_SEMANTIC_PHRASE_CONCEPT_WEIGHT);
            (
                Cand {
                    entry: e,
                    matched: matched.len(),
                    strong,
                    weight,
                },
                has_low,
            )
        })
        .filter(|(c, has_low)| {
            !is_low_value_graph_path(&c.entry.node.file_path)
                && is_source_anchor_kind(&c.entry.node)
                && is_public(&c.entry.node)
                && c.matched >= 2
                && !c.strong.is_empty()
                && *has_low
                && c.weight >= 1.0
        })
        .map(|(c, _)| c)
        .collect();
    lexical.sort_by(|a, b| {
        cmp_f64(b.weight, a.weight)
            .then(b.matched.cmp(&a.matched))
            .then(compare_ev(a.entry, b.entry))
    });
    lexical.truncate(16);
    let candidates: Vec<Cand> = lexical
        .into_iter()
        .filter(|c| {
            let incoming: Vec<(Node, EdgeInfo)> = access
                .incoming(&c.entry.node.id, FLOW_EDGE_KINDS)
                .into_iter()
                .filter(|(n, e)| e.confidence >= MIN_TRUSTED_CONFIDENCE && !is_low_value_graph_path(&n.file_path))
                .collect();
            incoming.is_empty() || incoming.iter().any(|(n, _)| n.file_path != c.entry.node.file_path)
        })
        .collect();
    let mut selected: Vec<String> = Vec::new();
    let mut covered: HashSet<String> = HashSet::new();
    while selected.len() < limit {
        let marginal = |c: &Cand| -> (f64, usize) {
            let m: Vec<&(String, f64)> = c.strong.iter().filter(|(k, _)| !covered.contains(k)).collect();
            (m.iter().map(|(_, w)| w).sum(), m.len())
        };
        let next = candidates
            .iter()
            .filter(|c| !selected.contains(&c.entry.node.id) && c.strong.iter().any(|(k, _)| !covered.contains(k)))
            .min_by(|a, b| {
                let (wa, na) = marginal(a);
                let (wb, nb) = marginal(b);
                cmp_f64(wb, wa)
                    .then(nb.cmp(&na))
                    .then(cmp_f64(b.weight, a.weight))
                    .then(b.matched.cmp(&a.matched))
                    .then(compare_ev(a.entry, b.entry))
            });
        let Some(next) = next else { break };
        selected.push(next.entry.node.id.clone());
        for (k, _) in &next.strong {
            covered.insert(k.clone());
        }
    }
    selected
}

fn has_independent_name_concepts(node: &Node, concepts: &[QueryConcept]) -> bool {
    let comps = search_components(&node.name);
    let matched: Vec<&QueryConcept> = concepts.iter().filter(|c| concept_matches_components(c, &comps)).collect();
    matched.len() >= 2 && matched.iter().map(|c| c.weight).sum::<f64>() >= 1.0
}

/// The best production declaration of the whole-task BM25 channel whose leaf name carries two
/// independent concepts (or exactly names a term).
fn best_full_query_declaration(entries: &[&NodeEv], concepts: &[QueryConcept]) -> Option<String> {
    entries
        .iter()
        .filter(|e| {
            e.full_query_rank != INF
                && !is_low_value_graph_path(&e.node.file_path)
                && (has_independent_name_concepts(&e.node, concepts)
                    || (is_reliable_lexical_seed(e)
                        && e.reasons.contains(&format!("term:{}", e.node.name.trim_start_matches('#').to_lowercase()))))
        })
        .min_by(|a, b| a.full_query_rank.cmp(&b.full_query_rank).then(compare_ev(a, b)).then(a.node.id.cmp(&b.node.id)))
        .map(|e| e.node.id.clone())
}

fn best_source_aligned_declaration(entries: &[&NodeEv]) -> Option<String> {
    entries
        .iter()
        .filter(|e| e.direct_region > 0.0 && e.best_region_rank != INF)
        .min_by(|a, b| {
            a.best_region_rank
                .cmp(&b.best_region_rank)
                .then(b.name_terms.len().cmp(&a.name_terms.len()))
                .then(cmp_f64(b.direct_region_strength(), a.direct_region_strength()))
                .then(a.best_region_overlap_rank.cmp(&b.best_region_overlap_rank))
                .then(compare_ev(a, b))
        })
        .map(|e| e.node.id.clone())
}

fn compare_provenance_flow_seed(a: &NodeEv, b: &NodeEv) -> std::cmp::Ordering {
    a.best_region_rank
        .cmp(&b.best_region_rank)
        .then(a.best_region_overlap_rank.cmp(&b.best_region_overlap_rank))
        .then((!b.region_callback_callsites.is_empty()).cmp(&!a.region_callback_callsites.is_empty()))
        .then(b.region_callsites.len().cmp(&a.region_callsites.len()))
        .then(compare_ev(a, b))
}

/// Ordered id set (insertion order, unique).
#[derive(Default, Clone)]
struct OrderedSet {
    order: Vec<String>,
    set: HashSet<String>,
}

impl OrderedSet {
    fn insert(&mut self, id: &str) -> bool {
        if self.set.insert(id.to_string()) {
            self.order.push(id.to_string());
            true
        } else {
            false
        }
    }
    fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }
    fn len(&self) -> usize {
        self.order.len()
    }
    fn remove(&mut self, id: &str) {
        if self.set.remove(id) {
            self.order.retain(|x| x != id);
        }
    }
}

/// Candidate declarations of the selected files: explicit identifiers, call-pair endpoints,
/// the whole-query declaration, intent facets and each file's source-aligned declaration are
/// reserved before the per-file representatives and the global fill.
#[allow(clippy::too_many_arguments)]
fn pick_declarations(
    ranked_files: &[String],
    nodes: &Nodes,
    hits: &[ChunkHit],
    plan: &GraphQueryPlan,
    concepts: &[QueryConcept],
    pair_seeds: &[String],
    access: &Access,
    max_nodes: usize,
    final_pass: bool,
) -> OrderedSet {
    let selected: HashSet<&String> = ranked_files.iter().collect();
    let mut ranked: Vec<&NodeEv> = nodes
        .values()
        .filter(|e| selected.contains(&e.node.file_path) && trustworthy(e, hits) && !is_non_candidate(&e.node))
        .collect();
    ranked.sort_by(|a, b| compare_ev(a, b));
    let mut picked = OrderedSet::default();
    for ident in &plan.explicit_identifiers {
        if let Some(e) = ranked.iter().find(|e| e.exact_ids.contains(ident)) {
            if picked.len() < max_nodes {
                picked.insert(&e.node.id);
            }
        }
    }
    for id in pair_seeds {
        if picked.len() >= max_nodes {
            break;
        }
        if nodes.get(id).is_some_and(|e| selected.contains(&e.node.file_path)) {
            picked.insert(id);
        }
    }
    if let Some(id) = best_full_query_declaration(&ranked, concepts) {
        if picked.contains(&id) || picked.len() < max_nodes {
            picked.insert(&id);
        }
    }
    for id in best_strong_intent_facets(&ranked, concepts, access, MAX_STRONG_INTENT_FACET_DECLARATIONS) {
        if picked.len() >= max_nodes {
            break;
        }
        picked.insert(&id);
    }
    let by_file = |p: &String| -> Vec<&NodeEv> { ranked.iter().filter(|e| &e.node.file_path == p).copied().collect() };
    for p in ranked_files {
        if picked.len() >= max_nodes {
            break;
        }
        if let Some(id) = best_source_aligned_declaration(&by_file(p)) {
            picked.insert(&id);
        }
    }
    let file_of = |id: &String| nodes.get(id).map(|e| e.node.file_path.clone()).unwrap_or_default();
    for p in ranked_files {
        let entries = by_file(p);
        let (limit, representatives): (usize, Vec<&NodeEv>) = if final_pass {
            let mut direct: Vec<&NodeEv> = entries.iter().filter(|e| !e.region_hits.is_empty()).copied().collect();
            direct.sort_by(|a, b| cmp_f64(b.direct_region_strength(), a.direct_region_strength()).then(compare_ev(a, b)));
            let mut callsite: Vec<&NodeEv> = entries.iter().filter(|e| e.best_callsite_rank != INF).copied().collect();
            callsite.sort_by(|a, b| a.best_callsite_rank.cmp(&b.best_callsite_rank).then(compare_ev(a, b)));
            let mut reps: Vec<&NodeEv> = Vec::new();
            if let Some(d) = direct.first() {
                reps.push(d);
            }
            for e in callsite.into_iter().take(2) {
                if !reps.iter().any(|r| r.node.id == e.node.id) {
                    reps.push(e);
                }
            }
            for e in &entries {
                if reps.len() >= 4 {
                    break;
                }
                if !reps.iter().any(|r| r.node.id == e.node.id) {
                    reps.push(e);
                }
            }
            (4, reps)
        } else {
            (3, entries)
        };
        let mut represented = picked.order.iter().filter(|id| &file_of(id) == p).count();
        for e in representatives {
            if represented >= limit || picked.len() >= max_nodes {
                break;
            }
            if picked.insert(&e.node.id) {
                represented += 1;
            }
        }
    }
    for e in &ranked {
        if picked.len() >= max_nodes {
            break;
        }
        picked.insert(&e.node.id);
    }
    picked
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

/// Select the files, declarations and flows most relevant to `task` from the SQLite index.
pub fn select_scope(conn: &Connection, task: &str, req: &ScopeRequest) -> ScopeSelection {
    select_scope_in(&SqliteScopeGraph { conn }, task, req)
}

/// Select the files, declarations and flows most relevant to `task` from any [`ScopeGraph`].
pub fn select_scope_in(graph: &dyn ScopeGraph, task: &str, req: &ScopeRequest) -> ScopeSelection {
    let plan = plan_graph_query(task);
    let mut sel = ScopeSelection {
        evidence_strength: "none",
        plan: plan.clone(),
        ..Default::default()
    };
    if req.max_nodes == 0 || req.max_files == 0 || (plan.terms.is_empty() && plan.explicit_identifiers.is_empty()) {
        return sel;
    }
    let max_files = req.max_files;
    let max_nodes = req.max_nodes;
    let access = Access::new(graph);
    let weighted = weighted_terms(&plan);
    let weighted_tuples: Vec<(String, f64, bool)> = weighted.iter().map(|t| (t.term.clone(), t.weight, t.stem)).collect();
    let concepts = plan.concepts();
    let mut nodes: Nodes = BTreeMap::new();
    let mut files: Files = BTreeMap::new();

    // Adjacent concept pairs searched as phrases (semantic phrase bridges).
    struct PhrasePair {
        left: usize,
        right: usize,
        query: String,
        index: usize,
    }
    let pairs: Vec<PhrasePair> = (0..concepts.len().saturating_sub(1))
        .map(|i| PhrasePair {
            left: i,
            right: i + 1,
            query: format!("{} {}", concepts[i].raw, concepts[i + 1].raw),
            index: i,
        })
        .filter(|p| {
            concepts[p.left].weight >= MIN_SEMANTIC_PHRASE_CONCEPT_WEIGHT
                || concepts[p.right].weight >= MIN_SEMANTIC_PHRASE_CONCEPT_WEIGHT
        })
        .take(MAX_SEMANTIC_PHRASE_SEARCHES)
        .collect();

    // Every node search of the request, deduplicated by normalized text, runs once.
    let mut requests: Vec<(String, String, usize)> = Vec::new();
    let mut want = |q: &str, limit: usize| {
        let key = normalize_node_search(q);
        match requests.iter_mut().find(|(k, _, _)| *k == key) {
            Some(r) => r.2 = r.2.max(limit),
            None => requests.push((key, q.to_string(), limit)),
        }
    };
    want(task, 80);
    for ident in &plan.explicit_identifiers {
        want(ident, 60);
    }
    for c in concepts.iter().take(16) {
        want(&c.raw, 30);
    }
    for p in &pairs {
        want(&p.query, 24);
    }
    let mut search_cache: HashMap<String, Vec<Node>> = HashMap::new();
    for (key, query, limit) in &requests {
        let results = graph.search_nodes(query, *limit);
        for n in &results {
            access.remember(n);
        }
        search_cache.insert(key.clone(), results);
    }
    let cached_search = |q: &str, limit: usize| -> Vec<Node> {
        search_cache
            .get(&normalize_node_search(q))
            .map(|v| v.iter().take(limit).cloned().collect())
            .unwrap_or_default()
    };
    let add_channel = |nodes: &mut Nodes, ranked: &[Node], reason: &dyn Fn(&Node) -> String, exact: Option<&str>, full: bool| {
        for (i, n) in ranked.iter().enumerate() {
            let e = ensure_node(nodes, n);
            e.rrf += 1.0 / (RRF_K + i as f64 + 1.0);
            if full {
                e.full_query_rank = e.full_query_rank.min(i);
            }
            if let Some(id) = exact {
                e.exact = true;
                e.exact_ids.insert(id.to_string());
            }
            e.reasons.insert(reason(n));
            add_term_evidence(e, n, &weighted);
        }
    };

    // 1. Whole-task BM25.
    add_channel(&mut nodes, &cached_search(task, 80), &|_| "bm25-node".to_string(), None, true);
    // 2. Explicit identifiers (exact; case-folded for a single capitalised word).
    for ident in &plan.explicit_identifiers {
        let cands = cached_search(ident, 60);
        let mut exact: Vec<Node> = cands.iter().filter(|n| exact_identifier_match(n, ident, false)).cloned().collect();
        if exact.is_empty() {
            exact = graph.nodes_named(ident);
            for n in &exact {
                access.remember(n);
            }
        }
        if exact.is_empty() && is_capitalised_word(ident) {
            exact = cands.iter().filter(|n| exact_identifier_match(n, ident, true)).cloned().collect();
        }
        let reason = format!("exact:{}", ident);
        add_channel(&mut nodes, &exact, &|_| reason.clone(), Some(ident), false);
    }
    // 3. Per-concept search.
    for c in concepts.iter().take(16) {
        let mut matched: HashMap<String, String> = HashMap::new();
        let ranked: Vec<Node> = cached_search(&c.raw, 30)
            .into_iter()
            .filter(|n| match c.terms.iter().find(|(t, s)| node_contains(n, t, *s)) {
                Some((t, _)) => {
                    matched.insert(n.id.clone(), t.clone());
                    true
                }
                None => false,
            })
            .collect();
        let key = c.key.clone();
        add_channel(
            &mut nodes,
            &ranked,
            &|n| format!("term:{}", matched.get(&n.id).cloned().unwrap_or_else(|| key.clone())),
            None,
            false,
        );
    }
    // 4. Optional vector channel (hybrid re-rank, off by default).
    for (i, (id, sim)) in req.vector_hits.iter().enumerate() {
        let Some(n) = access.node(id) else { continue };
        if is_non_candidate(&n) {
            continue;
        }
        let e = ensure_node(&mut nodes, &n);
        e.vector = e.vector.max(sim / (1.0 + i as f64 / 10.0) * 0.05);
        e.rrf += 0.5 / (RRF_K + i as f64 + 1.0);
        e.reasons.insert("vector".to_string());
    }

    // 5. Semantic phrase bridges: an adjacent phrase matching both endpoints of a real,
    // cross-file, compiler-trusted edge reserves the destination file.
    struct BridgeCand {
        source: Node,
        target: Node,
        edge: EdgeInfo,
        source_rank: usize,
        phrase_index: usize,
        left: usize,
        right: usize,
    }
    let mut bridge_cands: Vec<BridgeCand> = Vec::new();
    for p in &pairs {
        let (l, r) = (&concepts[p.left], &concepts[p.right]);
        let sources: Vec<(usize, Node)> = cached_search(&p.query, 24)
            .into_iter()
            .enumerate()
            .filter(|(_, s)| {
                node_matches_concept(s, l)
                    && node_matches_concept(s, r)
                    && (node_matches_concept_in_identity(s, l) || node_matches_concept_in_identity(s, r))
                    && (plan.asks_for_tests || !is_low_value_graph_path(&s.file_path))
            })
            .take(MAX_SEMANTIC_PHRASE_CANDIDATES)
            .collect();
        for (source_rank, source) in sources {
            let mut out = access.outgoing(&source.id, FLOW_EDGE_KINDS);
            out.sort_by(compare_semantic_phrase_neighbor);
            out.truncate(MAX_SEMANTIC_PHRASE_OUTGOING_PER_SOURCE);
            for (target, edge) in out {
                if edge.confidence >= MIN_TRUSTED_CONFIDENCE
                    && source.file_path != target.file_path
                    && is_source_anchor_kind(&target)
                    && node_matches_concept(&target, l)
                    && node_matches_concept(&target, r)
                    && (node_matches_concept_in_identity(&target, l) || node_matches_concept_in_identity(&target, r))
                    && (plan.asks_for_tests || !is_low_value_graph_path(&target.file_path))
                {
                    bridge_cands.push(BridgeCand {
                        source: source.clone(),
                        target,
                        edge,
                        source_rank,
                        phrase_index: p.index,
                        left: p.left,
                        right: p.right,
                    });
                }
            }
        }
    }
    bridge_cands.sort_by(|a, b| {
        a.source_rank
            .cmp(&b.source_rank)
            .then(a.phrase_index.cmp(&b.phrase_index))
            .then(cmp_f64(b.edge.confidence, a.edge.confidence))
            .then(compare_edge_callsite(&a.edge, &b.edge))
            .then(a.source.file_path.cmp(&b.source.file_path))
            .then(a.source.id.cmp(&b.source.id))
            .then(a.target.file_path.cmp(&b.target.file_path))
            .then(a.target.id.cmp(&b.target.id))
    });
    // (source id, edge)
    let mut bridges: Vec<(String, EdgeInfo)> = Vec::new();
    let mut bridge_target_paths: Vec<String> = Vec::new();
    for b in bridge_cands {
        if bridge_target_paths.contains(&b.target.file_path) {
            continue;
        }
        {
            let s = ensure_node(&mut nodes, &b.source);
            s.rrf += 1.0 / (RRF_K + b.source_rank as f64 + 1.0);
            s.reasons.insert("query-phrase-flow".into());
            s.reliable = true;
            add_concept_evidence(s, &concepts[b.left]);
            add_concept_evidence(s, &concepts[b.right]);
        }
        {
            let t = ensure_node(&mut nodes, &b.target);
            t.graph = t.graph.max(0.16 * b.edge.confidence);
            t.reasons.insert(format!("graph:{}", b.edge.kind));
            t.reasons.insert("query-phrase-flow".into());
            t.reliable = true;
            t.explanations.insert(describe_edge(&b.edge, &b.source.id, &b.source.name));
            add_concept_evidence(t, &concepts[b.left]);
            add_concept_evidence(t, &concepts[b.right]);
        }
        let f = ensure_file(&mut files, &b.target.file_path);
        f.node_ids.insert(b.target.id.clone());
        f.reliable_graph = true;
        f.reasons.insert("query-phrase-flow".into());
        bridges.push((b.source.id.clone(), b.edge.clone()));
        bridge_target_paths.push(b.target.file_path.clone());
        if bridges.len() >= MAX_SEMANTIC_PHRASE_BRIDGES {
            break;
        }
    }

    // 6. Source chunks, bridged to the declarations they overlap.
    let indexed = graph.indexed_files();
    let parse_status: HashMap<String, String> = indexed.iter().cloned().collect();
    let hits = graph.search_source(task, &weighted_tuples, 100);
    let mut callsite_ranks: HashMap<String, usize> = HashMap::new();
    let mut callsite_weights: HashMap<String, f64> = HashMap::new();
    for (index, hit) in hits.iter().enumerate() {
        {
            let f = ensure_file(&mut files, &hit.file_path);
            f.reasons.insert("source-fts".into());
            f.text_hits.push(index);
            f.best_text_rank = Some(f.best_text_rank.map_or(index, |b| b.min(index)));
        }
        let hit_terms: BTreeSet<String> = hit.matched_terms.iter().cloned().collect();
        let local: Vec<&QueryConcept> = concepts.iter().filter(|c| concept_matches_terms(c, &hit_terms)).collect();
        let local_count = local.len();
        let local_weight: f64 = local.iter().map(|c| c.weight).sum();
        for (overlap, id) in hit.node_ids.iter().enumerate() {
            let Some(node) = access.node(id) else { continue };
            if node.file_path != hit.file_path || !is_anchor_for_region(&node) {
                continue;
            }
            let name_comps = search_components(&node.name);
            let name_concepts = concepts.iter().filter(|c| concept_matches_components(c, &name_comps)).count();
            let starts_inside = node.start_line >= hit.start_line && node.start_line <= hit.end_line;
            let encloses = node.start_line <= hit.start_line && node.end_line >= hit.end_line;
            let alignment = (local_count.saturating_sub(1) as f64 * 0.18
                + name_concepts as f64 * 0.32
                + if starts_inside {
                    0.08
                } else if encloses {
                    0.04
                } else {
                    0.0
                }
                + (hit.rank.abs() * 2.0).min(0.1)
                + 20.0 / (RRF_K + index as f64 + overlap as f64 + 1.0))
                .min(1.2);
            let inspect = index < 24 || (index < 64 && overlap == 0 && local_count >= 2);
            let outgoing = if inspect {
                access
                    .outgoing(&node.id, FLOW_TRAVERSAL_EDGE_KINDS)
                    .into_iter()
                    .filter(|(n, e)| e.confidence >= MIN_TRUSTED_CONFIDENCE && (e.kind != "contains" || is_unnamed_flow_node(n)))
                    .collect()
            } else {
                Vec::new()
            };
            let e = ensure_node(&mut nodes, &node);
            if index < e.best_region_rank {
                e.best_region_rank = index;
                e.best_region_overlap_rank = overlap;
            } else if index == e.best_region_rank {
                e.best_region_overlap_rank = e.best_region_overlap_rank.min(overlap);
            }
            for t in &hit_terms {
                e.terms.insert(t.clone());
            }
            for t in &weighted {
                if component_matches(&name_comps, &t.term, t.stem) {
                    e.name_terms.insert(t.term.clone());
                }
            }
            e.region = e.region.max(alignment);
            e.direct_region = e.direct_region.max(alignment);
            if !e.region_hits.iter().any(|&h| {
                hits[h].file_path == hit.file_path && hits[h].start_line == hit.start_line && hits[h].end_line == hit.end_line
            }) {
                e.region_hits.push(index);
            }
            for (n, edge) in outgoing {
                if !line_in_hit(edge.line, hit) {
                    continue;
                }
                let key = edge_key(&edge);
                e.region_callsites.insert(key.clone());
                let r = callsite_ranks.entry(key.clone()).or_insert(index);
                *r = (*r).min(index);
                let w = callsite_weights.entry(key.clone()).or_insert(0.0);
                *w = w.max(local_weight);
                if edge.kind == "contains" && is_unnamed_flow_node(&n) {
                    e.region_callback_callsites.insert(key);
                }
            }
            e.rrf += 1.0 / (RRF_K + index as f64 + overlap as f64 + 1.0);
            e.reasons.insert("source-region".into());
            let f = ensure_file(&mut files, &hit.file_path);
            f.region = f.region.max(alignment);
            f.node_ids.insert(node.id.clone());
        }
    }
    for e in nodes.values() {
        let f = ensure_file(&mut files, &e.node.file_path);
        f.region = f.region.max(e.region_strength());
    }

    // 7. Whole-query call pairs.
    let call_pairs = best_full_query_call_pairs(&nodes, &concepts, &access, MAX_FULL_QUERY_CALL_PAIR_SEEDS);
    let pair_seeds: Vec<String> = call_pairs.iter().flat_map(|p| [p.source.clone(), p.target.clone()]).collect();
    let pair_seed_ids: HashSet<String> = pair_seeds.iter().cloned().collect();
    for id in &pair_seeds {
        let Some(e) = nodes.get_mut(id) else { continue };
        e.reliable = true;
        e.reasons.insert("bm25-call-pair".into());
        let node = e.node.clone();
        for c in &concepts {
            if node_matches_concept(&node, c) {
                add_concept_evidence(e, c);
            }
        }
        let f = ensure_file(&mut files, &node.file_path);
        f.node_ids.insert(node.id.clone());
        f.reliable_graph = true;
        f.reasons.insert("bm25-call-pair".into());
    }

    // 8. Path matches.
    let literal = plan.literal_terms();
    let path_terms: Vec<String> = literal.iter().filter(|t| t.len() >= 3).cloned().collect();
    let adjacent: Vec<(String, String)> = path_terms.windows(2).map(|w| (w[0].clone(), w[1].clone())).collect();
    let mut path_matches: Vec<(&String, usize, usize, HashSet<String>)> = indexed
        .iter()
        .filter_map(|(p, _)| {
            let comps = search_components(p);
            let base = search_components(p.rsplit('/').next().unwrap_or(p));
            let matched: Vec<&QueryConcept> = concepts.iter().filter(|c| concept_matches_components(c, &comps)).collect();
            let base_matched = matched.iter().filter(|c| concept_matches_components(c, &base)).count();
            (!matched.is_empty()).then_some((p, matched.len(), base_matched, comps))
        })
        .collect();
    path_matches.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(b.0)));
    for (i, (p, _, _, comps)) in path_matches.iter().enumerate() {
        let f = ensure_file(&mut files, p);
        f.score += 1.0 / (RRF_K + i as f64 + 1.0);
        f.reasons.insert("path-match".into());
        for c in &concepts {
            if !concept_matches_components(c, comps) {
                continue;
            }
            for (t, s) in &c.terms {
                if component_matches(comps, t, *s) {
                    f.terms.insert(t.clone());
                }
            }
        }
    }
    for e in nodes.values() {
        let f = ensure_file(&mut files, &e.node.file_path);
        f.node_ids.insert(e.node.id.clone());
        f.exact |= e.exact;
        f.exact_ids.extend(e.exact_ids.iter().cloned());
        f.terms.extend(e.terms.iter().cloned());
        f.reasons.extend(e.reasons.iter().cloned());
    }
    for (path, f) in files.iter_mut() {
        let comps = search_components(path);
        for t in &path_terms {
            if comps.contains(t) {
                f.terms.insert(t.clone());
            }
        }
        for &h in &f.text_hits {
            for t in &hits[h].matched_terms {
                f.terms.insert(t.clone());
            }
            if hits[h].rank < 0.0 {
                f.score += (hits[h].rank.abs() / 100.0).min(0.08);
            }
        }
    }

    // 9. Reliable seeds: phrase sources, call pairs, callback owners and callsite regions
    // first, then one seed per file, then the global pool; gated two-hop expansion.
    let mut pool: Vec<&NodeEv> = nodes
        .values()
        .filter(|e| is_reliable_lexical_seed(e) || is_reliable_region_seed(e, &hits) || e.vector > 0.0)
        .collect();
    pool.sort_by(|a, b| compare_ev(a, b));
    let mut by_region: BTreeMap<usize, Vec<&NodeEv>> = BTreeMap::new();
    for e in pool.iter().filter(|e| !e.region_callsites.is_empty()) {
        by_region.entry(e.best_region_rank).or_default().push(e);
    }
    let mut callsite_seeds: Vec<String> = Vec::new();
    for (_, mut bucket) in by_region.into_iter().take(6) {
        bucket.sort_by(|a, b| {
            a.best_region_overlap_rank
                .cmp(&b.best_region_overlap_rank)
                .then(compare_ev(a, b))
                .then(b.region_callsites.len().cmp(&a.region_callsites.len()))
        });
        callsite_seeds.extend(bucket.into_iter().take(2).map(|e| e.node.id.clone()));
    }
    let mut callback_seeds: Vec<&NodeEv> = pool.iter().filter(|e| !e.region_callback_callsites.is_empty()).copied().collect();
    callback_seeds.sort_by(|a, b| a.best_region_rank.cmp(&b.best_region_rank).then(compare_ev(a, b)));
    let callback_seeds: Vec<String> = callback_seeds.into_iter().take(4).map(|e| e.node.id.clone()).collect();
    let phrase_seeds: Vec<String> = bridges.iter().map(|(s, _)| s.clone()).collect();
    let mut seeds = OrderedSet::default();
    let mut seeded_files: HashSet<String> = HashSet::new();
    for id in phrase_seeds.iter().chain(&pair_seeds).chain(&callback_seeds).chain(&callsite_seeds) {
        if seeds.contains(id) {
            continue;
        }
        if seeds.len() >= 12 {
            break;
        }
        seeds.insert(id);
        if let Some(e) = nodes.get(id) {
            seeded_files.insert(e.node.file_path.clone());
        }
    }
    for e in &pool {
        if seeds.len() >= 18 {
            break;
        }
        if seeded_files.contains(&e.node.file_path) {
            continue;
        }
        seeded_files.insert(e.node.file_path.clone());
        seeds.insert(&e.node.id);
    }
    for e in &pool {
        if seeds.len() >= 24 {
            break;
        }
        seeds.insert(&e.node.id);
    }
    drop(pool);
    let reliable_seeds: Vec<String> = seeds.order.clone();
    for id in &reliable_seeds {
        if let Some(e) = nodes.get_mut(id) {
            e.reliable = true;
            let p = e.node.file_path.clone();
            ensure_file(&mut files, &p).reliable_graph = true;
        }
    }
    let mut queue: Vec<(String, usize, bool)> = reliable_seeds
        .iter()
        .map(|id| {
            let typed = nodes.get(id).is_some_and(is_reliable_lexical_seed) || pair_seed_ids.contains(id);
            (id.clone(), 0, typed)
        })
        .collect();
    let mut visited: HashSet<String> = reliable_seeds.iter().cloned().collect();
    let mut cursor = 0;
    while cursor < queue.len() && cursor < 180 {
        let (id, depth, typed) = queue[cursor].clone();
        cursor += 1;
        if depth >= 2 {
            continue;
        }
        let Some(current) = nodes.get(&id).cloned() else { continue };
        let file_hits = files.get(&current.node.file_path).map(|f| f.text_hits.clone()).unwrap_or_default();
        let mut neigh: Vec<(Node, EdgeInfo)> = reliable_neighbors(&access, &id)
            .into_iter()
            .filter(|(_, e)| typed || query_callsite_hit(&id, e, &current.region_hits, &hits))
            .collect();
        neigh.sort_by(|a, b| compare_query_neighbor(&id, a, b, &file_hits, &hits, &weighted));
        for (n, edge) in neigh.into_iter().take(18) {
            let mut propagated = 0.0;
            let mut propagated_weight = 0.0;
            let callsite = query_callsite_hit(&id, &edge, &current.region_hits, &hits);
            if callsite {
                propagated = (current.region_strength() + 1.0).min(2.0);
                propagated_weight = query_callsite_weight(&edge, &current.region_hits, &hits, &concepts);
                let key = edge_key(&edge);
                if let Some(c) = nodes.get_mut(&id) {
                    c.region_callsites.insert(key.clone());
                }
                let r = callsite_ranks.entry(key.clone()).or_insert(current.best_region_rank);
                *r = (*r).min(current.best_region_rank);
                let w = callsite_weights.entry(key).or_insert(0.0);
                *w = w.max(propagated_weight);
            }
            let e = ensure_node(&mut nodes, &n);
            e.reliable = true;
            e.graph = e.graph.max((2 - depth) as f64 * 0.08 * edge.confidence);
            e.reasons.insert(format!("graph:{}", edge.kind));
            e.explanations.insert(describe_edge(&edge, &id, &current.node.name));
            if callsite {
                e.region = e.region.max(propagated);
                e.best_callsite_rank = e.best_callsite_rank.min(current.best_region_rank);
                e.reasons.insert("source-region:callsite".into());
                if edge.kind == "contains" && is_unnamed_flow_node(&n) {
                    // Keep the matched window across the one anonymous callback bridge.
                    for &h in &current.region_hits {
                        if !e.region_hits.iter().any(|&x| {
                            hits[x].file_path == hits[h].file_path
                                && hits[x].start_line == hits[h].start_line
                                && hits[x].end_line == hits[h].end_line
                        }) {
                            e.region_hits.push(h);
                        }
                    }
                    e.best_region_rank = e.best_region_rank.min(current.best_region_rank);
                    e.best_region_overlap_rank = e.best_region_overlap_rank.min(current.best_region_overlap_rank);
                }
            }
            add_term_evidence(e, &n, &weighted);
            let lexical = is_reliable_lexical_seed(e);
            let f = ensure_file(&mut files, &n.file_path);
            f.reliable_graph = true;
            f.callsite_region = f.callsite_region.max(propagated);
            f.callsite_query_weight = f.callsite_query_weight.max(propagated_weight);
            if propagated > 0.0 {
                f.best_callsite_rank = Some(f.best_callsite_rank.map_or(current.best_region_rank, |b| b.min(current.best_region_rank)));
            }
            f.node_ids.insert(n.id.clone());
            f.reasons.insert(format!("graph:{}", edge.kind));
            if visited.insert(n.id.clone()) {
                queue.push((n.id.clone(), depth + 1, typed || propagated > 0.0 || lexical));
            }
        }
    }

    // 10. File scoring: strongest node / chunk signals, IDF-weighted concept coverage, path,
    // declaration-aligned region and propagated callsite evidence.
    let mut node_scores: HashMap<String, Vec<f64>> = HashMap::new();
    for e in nodes.values() {
        let f = ensure_file(&mut files, &e.node.file_path);
        f.node_ids.insert(e.node.id.clone());
        f.exact |= e.exact;
        f.exact_ids.extend(e.exact_ids.iter().cloned());
        f.terms.extend(e.terms.iter().cloned());
        f.reasons.extend(e.reasons.iter().cloned());
        node_scores.entry(e.node.file_path.clone()).or_default().push(e.fused());
    }
    let file_count = files.len().max(1);
    let df: HashMap<String, usize> = concepts
        .iter()
        .map(|c| (c.key.clone(), files.values().filter(|f| concept_matches_terms(c, &f.terms)).count()))
        .collect();
    let concept_idf = |c: &QueryConcept| c.weight * idf(file_count, df.get(&c.key).copied().unwrap_or(0));
    let total_idf = {
        let t: f64 = concepts.iter().map(concept_idf).sum();
        if t == 0.0 {
            1.0
        } else {
            t
        }
    };
    let best_ident_by_file: HashMap<String, usize> = {
        let mut m: HashMap<String, usize> = HashMap::new();
        for e in nodes.values() {
            let cs = search_components(&e.node.name);
            let n = weighted.iter().filter(|t| component_matches(&cs, &t.term, t.stem)).count();
            let v = m.entry(e.node.file_path.clone()).or_insert(0);
            *v = (*v).max(n);
        }
        m
    };
    for (path, f) in files.iter_mut() {
        let mut ns = node_scores.get(path).cloned().unwrap_or_default();
        ns.sort_by(|a, b| cmp_f64(*b, *a));
        f.score += ns.first().copied().unwrap_or(0.0) + ns.get(1).copied().unwrap_or(0.0) * 0.45
            + ns.get(2).copied().unwrap_or(0.0) * 0.2;
        let mut chunk_scores: Vec<f64> = f.text_hits.iter().map(|&h| hits[h].rank.abs()).collect();
        chunk_scores.sort_by(|a, b| cmp_f64(*b, *a));
        f.score += chunk_scores.first().copied().unwrap_or(0.0) + chunk_scores.get(1).copied().unwrap_or(0.0) * 0.35;
        if let Some(r) = f.best_text_rank {
            f.score += SOURCE_FILE_RRF_WEIGHT / (RRF_K + r as f64 + 1.0);
        }
        let covered: f64 = concepts.iter().filter(|c| concept_matches_terms(c, &f.terms)).map(concept_idf).sum();
        let covered_count = concepts.iter().filter(|c| concept_matches_terms(c, &f.terms)).count();
        f.score += (covered / total_idf) * 0.4 + covered_count.saturating_sub(1) as f64 * 0.015;
        let comps = search_components(path);
        let base = search_components(path.rsplit('/').next().unwrap_or(path));
        let adjacent_matches = adjacent.iter().filter(|(a, b)| comps.contains(a) && comps.contains(b)).count();
        let best_local = f
            .text_hits
            .iter()
            .map(|&h| {
                let ts: BTreeSet<String> = hits[h].matched_terms.iter().cloned().collect();
                let matched: Vec<&QueryConcept> = concepts.iter().filter(|c| concept_matches_terms(c, &ts)).collect();
                (matched.iter().map(|c| concept_idf(c)).sum::<f64>(), matched.len())
            })
            .fold((0.0, 0), |best, cur| if cur.0 > best.0 { cur } else { best });
        let best_ident = best_ident_by_file.get(path).copied().unwrap_or(0);
        f.score += adjacent_matches as f64 * 0.45
            + component_idf_signal(&comps, &concepts, &concept_idf) * 0.08
            + component_idf_signal(&base, &concepts, &concept_idf) * 0.18
            + (best_local.0 / total_idf) * 2.0
            + best_local.1.saturating_sub(1) as f64 * 0.2
            + best_ident.saturating_sub(1) as f64 * 0.2;
        // Direct alignment and a compiler-proven callsite are independent channels.
        f.score += f.region * 1.4 + f.callsite_region * 0.7;
        if f.exact {
            f.score += 0.25;
        }
        if !plan.asks_for_tests && is_low_value_graph_path(path) {
            f.score *= 0.55;
        }
    }

    // 11. File admission.
    let score_of = |files: &Files, p: &str| files.get(p).map(|f| f.score).unwrap_or(0.0);
    let mut all_ranked: Vec<String> = files.keys().cloned().collect();
    all_ranked.sort_by(|a, b| cmp_f64(score_of(&files, b), score_of(&files, a)).then(a.cmp(b)));
    let mut selected = OrderedSet::default();
    // One best file per explicitly named symbol.
    for ident in &plan.explicit_identifiers {
        if let Some(p) = all_ranked.iter().find(|p| files[*p].exact_ids.contains(ident)) {
            if selected.len() < max_files {
                selected.insert(p);
            }
        }
    }
    // Phrase destinations.
    for p in &bridge_target_paths {
        if selected.len() >= max_files {
            break;
        }
        if files.contains_key(p) {
            selected.insert(p);
        }
    }
    let source_reserve = 3.min(max_files.saturating_sub(1));
    let multi_concept = |f: &FileEv| {
        f.text_hits.iter().any(|&h| {
            let ts: BTreeSet<String> = hits[h].matched_terms.iter().cloned().collect();
            concepts.iter().filter(|c| concept_matches_terms(c, &ts)).count() >= 2
        })
    };
    let strongly_qualified: HashSet<&String> =
        files.iter().filter(|(_, f)| f.region >= 0.55 || multi_concept(f)).map(|(p, _)| p).collect();
    // Strong graph files: the best trustworthy compound declaration name per file.
    struct Strong<'n> {
        entry: &'n NodeEv,
        pinned: bool,
        score: f64,
        count: usize,
    }
    let better = |c: &Strong, cur: &Strong| -> bool {
        (c.pinned && !cur.pinned)
            || (c.pinned == cur.pinned && c.score > cur.score)
            || (c.pinned == cur.pinned && c.score == cur.score && c.count > cur.count)
            || (c.pinned == cur.pinned
                && c.score == cur.score
                && c.count == cur.count
                && compare_ev(c.entry, cur.entry) == std::cmp::Ordering::Less)
    };
    let mut strong_by_file: BTreeMap<String, Strong> = BTreeMap::new();
    for e in nodes.values() {
        if !is_reliable_lexical_seed(e) && !is_reliable_region_seed(e, &hits) {
            continue;
        }
        let comps = search_components(&e.node.name);
        let matched: Vec<&QueryConcept> = concepts.iter().filter(|c| concept_matches_components(c, &comps)).collect();
        let pinned = e.exact && selected.contains(&e.node.file_path);
        if !pinned && matched.len() < 2 {
            continue;
        }
        let cand = Strong {
            entry: e,
            pinned,
            score: matched.iter().map(|c| concept_idf(c)).sum(),
            count: matched.len(),
        };
        let replace = strong_by_file.get(&e.node.file_path).is_none_or(|cur| better(&cand, cur));
        if replace {
            strong_by_file.insert(e.node.file_path.clone(), cand);
        }
    }
    let mut strong_files: Vec<Strong> = strong_by_file.into_values().collect();
    strong_files.sort_by(|a, b| {
        b.pinned
            .cmp(&a.pinned)
            .then(cmp_f64(b.score, a.score))
            .then(b.count.cmp(&a.count))
            .then(compare_ev(a.entry, b.entry))
            .then(cmp_f64(score_of(&files, &b.entry.node.file_path), score_of(&files, &a.entry.node.file_path)))
            .then(a.entry.node.file_path.cmp(&b.entry.node.file_path))
    });
    let strong_list: Vec<(String, String)> =
        strong_files.iter().map(|s| (s.entry.node.file_path.clone(), s.entry.node.kind.clone())).collect();
    drop(strong_files);
    // Source floor: direct FTS files (declaration-aligned, multi-concept or near the fused
    // cutoff) plus the best propagated callsite destination.
    let fused_cutoff = if all_ranked.is_empty() {
        0.0
    } else {
        score_of(&files, &all_ranked[max_files.min(all_ranked.len()) - 1])
    };
    let eligible: HashSet<&String> = files
        .iter()
        .filter(|(_, f)| {
            f.region >= 0.55 || multi_concept(f) || (fused_cutoff > 0.0 && f.score >= fused_cutoff * SOURCE_NEAR_CUTOFF_RATIO)
        })
        .map(|(p, _)| p)
        .collect();
    let mut direct_sources: Vec<String> = Vec::new();
    for h in &hits {
        if !direct_sources.contains(&h.file_path) {
            direct_sources.push(h.file_path.clone());
        }
    }
    direct_sources.retain(|p| eligible.contains(p) && strongly_qualified.contains(p));
    let low = |p: &str| !plan.asks_for_tests && is_low_value_graph_path(p);
    direct_sources.sort_by(|a, b| {
        let (fa, fb) = (&files[a], &files[b]);
        low(a)
            .cmp(&low(b))
            .then(fa.best_text_rank.unwrap_or(INF).cmp(&fb.best_text_rank.unwrap_or(INF)))
            .then(cmp_f64(fb.region, fa.region))
            .then(cmp_f64(fb.score, fa.score))
            .then(a.cmp(b))
    });
    let mut semantic: Vec<&String> = files
        .iter()
        .filter(|(_, f)| f.callsite_region > 0.0 && f.callsite_query_weight > 0.0 && f.best_callsite_rank.is_some())
        .map(|(p, _)| p)
        .collect();
    semantic.sort_by(|a, b| {
        let (fa, fb) = (&files[*a], &files[*b]);
        low(a)
            .cmp(&low(b))
            .then(cmp_f64(fb.callsite_query_weight, fa.callsite_query_weight))
            .then(fa.best_callsite_rank.cmp(&fb.best_callsite_rank))
            .then(cmp_f64(fb.callsite_region, fa.callsite_region))
            .then(cmp_f64(fb.score, fa.score))
            .then(a.cmp(b))
    });
    let semantic_winner: Option<String> = semantic.first().map(|p| (*p).clone());
    let source_floor: Vec<String> = if source_reserve == 0 {
        Vec::new()
    } else {
        match &semantic_winner {
            None => direct_sources.iter().take(source_reserve).cloned().collect(),
            Some(w) if source_reserve == 1 => match direct_sources.first() {
                None => vec![w.clone()],
                Some(d) if d == w => vec![w.clone()],
                Some(d) => {
                    let mut two = [d.clone(), w.clone()];
                    two.sort_by(|a, b| cmp_f64(score_of(&files, b), score_of(&files, a)).then(a.cmp(b)));
                    vec![two[0].clone()]
                }
            },
            Some(w) => {
                let mut v = vec![w.clone()];
                v.extend(direct_sources.iter().filter(|p| *p != w).take(source_reserve - 1).cloned());
                v
            }
        }
    };
    let unrepresented = source_floor.iter().filter(|p| !selected.contains(p)).count();
    let strong_limit = selected
        .len()
        .max(max_files - unrepresented.min(max_files.saturating_sub(selected.len())));
    for (p, _) in &strong_list {
        if selected.len() >= strong_limit {
            break;
        }
        selected.insert(p);
    }
    let mut represented = selected.order.iter().filter(|p| source_floor.contains(p)).count();
    for p in &source_floor {
        if represented >= source_floor.len() || selected.len() >= max_files {
            break;
        }
        if !files.contains_key(p) || selected.contains(p) {
            continue;
        }
        selected.insert(p);
        represented += 1;
    }
    for p in &all_ranked {
        if selected.len() >= max_files {
            break;
        }
        selected.insert(p);
    }
    let rank_files = |selected: &OrderedSet, files: &Files| -> Vec<String> {
        let mut v = selected.order.clone();
        v.sort_by(|a, b| cmp_f64(score_of(files, b), score_of(files, a)).then(a.cmp(b)));
        v
    };
    let ranked_files = rank_files(&selected, &files);

    // 12. Declarations of the selected files.
    let picked = pick_declarations(&ranked_files, &nodes, &hits, &plan, &concepts, &pair_seeds, &access, max_nodes, false);

    // 13. Directed flows.
    let df_term = |t: &str| df.get(t).copied().unwrap_or(0);
    let mut seed_relevance: HashMap<String, f64> = nodes
        .values()
        .map(|e| {
            let lexical: f64 = weighted
                .iter()
                .filter(|t| e.name_terms.contains(&t.term))
                .map(|t| t.weight * idf(file_count, df_term(&t.term)) * if t.stem { 1.0 } else { 4.0 })
                .sum();
            (
                e.node.id.clone(),
                (if e.exact { 6.0 } else { 0.0 }) + lexical + e.terms.len() as f64 * 0.1 + e.rrf + e.region_strength() * 2.0,
            )
        })
        .collect();
    let file_priority: HashMap<String, f64> = ranked_files
        .iter()
        .enumerate()
        .map(|(i, p)| (p.clone(), (ranked_files.len() - i) as f64))
        .collect();
    let mut node_terms: HashMap<String, BTreeSet<String>> =
        nodes.values().map(|e| (e.node.id.clone(), e.terms.clone())).collect();
    let term_weights: Vec<(String, f64)> =
        weighted.iter().map(|t| (t.term.clone(), t.weight * idf(file_count, df_term(&t.term)))).collect();
    let relevance_for = |n: &Node, known: &HashMap<String, f64>| -> f64 {
        if let Some(v) = known.get(&n.id) {
            return *v;
        }
        let cs = search_components(&n.name);
        weighted
            .iter()
            .filter(|t| component_matches(&cs, &t.term, t.stem))
            .map(|t| t.weight * idf(file_count, df_term(&t.term)) * if t.stem { 1.0 } else { 4.0 })
            .sum()
    };
    let mut picked_flow: Vec<&NodeEv> = picked.order.iter().filter_map(|id| nodes.get(id)).filter(|e| e.reliable).collect();
    picked_flow.sort_by(|a, b| compare_ev(a, b));
    picked_flow.truncate(MAX_FLOW_SEEDS);
    let picked_flow_ids: HashSet<&String> = picked_flow.iter().map(|e| &e.node.id).collect();
    let mut phrase_flow: Vec<&NodeEv> = Vec::new();
    for (s, _) in &bridges {
        if let Some(e) = nodes.get(s) {
            if !picked_flow_ids.contains(&e.node.id) && !phrase_flow.iter().any(|x| x.node.id == e.node.id) {
                phrase_flow.push(e);
            }
        }
    }
    phrase_flow.truncate(MAX_FLOW_SEEDS - picked_flow.len());
    let phrase_flow_ids: HashSet<&String> = phrase_flow.iter().map(|e| &e.node.id).collect();
    let mut provenance_flow: Vec<&NodeEv> = reliable_seeds
        .iter()
        .filter_map(|id| nodes.get(id))
        .filter(|e| {
            !picked_flow_ids.contains(&e.node.id)
                && !phrase_flow_ids.contains(&e.node.id)
                && (!e.region_callsites.is_empty() || !e.region_callback_callsites.is_empty())
        })
        .collect();
    provenance_flow.sort_by(|a, b| compare_provenance_flow_seed(a, b));
    provenance_flow.truncate(MAX_FLOW_SEEDS - picked_flow.len() - phrase_flow.len());
    let direct_flow: Vec<&NodeEv> = phrase_flow.iter().chain(&provenance_flow).chain(&picked_flow).copied().collect();
    let mut flow_seeds: Vec<Node> = Vec::new();
    let mut flow_seed_ids: HashSet<String> = HashSet::new();
    for e in &direct_flow {
        if flow_seed_ids.insert(e.node.id.clone()) {
            flow_seeds.push(e.node.clone());
        }
    }
    let mut seed_callsite_ranks: HashMap<String, usize> = HashMap::new();
    for e in &direct_flow {
        if !e.region_callsites.is_empty() && e.best_region_rank != INF {
            seed_callsite_ranks.insert(e.node.id.clone(), e.best_region_rank);
        }
    }
    struct Derived {
        node: Node,
        relevance: f64,
        callsite_rank: usize,
        confidence: f64,
    }
    let compare_derived = |a: &Derived, b: &Derived| {
        (b.callsite_rank != INF)
            .cmp(&(a.callsite_rank != INF))
            .then(a.callsite_rank.cmp(&b.callsite_rank))
            .then(cmp_f64(b.relevance, a.relevance))
            .then(cmp_f64(b.confidence, a.confidence))
            .then(is_low_value_graph_path(&a.node.file_path).cmp(&is_low_value_graph_path(&b.node.file_path)))
            .then(a.node.id.cmp(&b.node.id))
    };
    let remaining_slots = MAX_FLOW_SEEDS.saturating_sub(flow_seeds.len());
    if remaining_slots > 0 {
        let mut derived: BTreeMap<String, Derived> = BTreeMap::new();
        let remember = |d: Derived, derived: &mut BTreeMap<String, Derived>| {
            if flow_seed_ids.contains(&d.node.id) {
                return;
            }
            let replace = derived
                .get(&d.node.id)
                .is_none_or(|prior| compare_derived(&d, prior) == std::cmp::Ordering::Less);
            if replace {
                derived.insert(d.node.id.clone(), d);
            }
        };
        for e in &direct_flow {
            for (n, edge) in access.incoming(&e.node.id, FLOW_EDGE_KINDS) {
                if edge.confidence < MIN_TRUSTED_CONFIDENCE {
                    continue;
                }
                let d = Derived {
                    relevance: relevance_for(&n, &seed_relevance),
                    callsite_rank: callsite_ranks.get(&edge_key(&edge)).copied().unwrap_or(INF),
                    confidence: edge.confidence,
                    node: n,
                };
                remember(d, &mut derived);
            }
        }
        // Calls inside a callback are owned by the enclosing named declaration.
        let mut owners: Vec<&Derived> = derived.values().collect();
        owners.sort_by(|a, b| compare_derived(a, b));
        let owners: Vec<(String, usize, f64)> = owners
            .into_iter()
            .filter(|d| is_unnamed_flow_node(&d.node))
            .take(remaining_slots)
            .map(|d| (d.node.id.clone(), d.callsite_rank, d.confidence))
            .collect();
        for (cb, rank, conf) in owners {
            for (owner, edge) in access.incoming(&cb, &["contains"]) {
                if edge.confidence < MIN_TRUSTED_CONFIDENCE {
                    continue;
                }
                let d = Derived {
                    relevance: relevance_for(&owner, &seed_relevance),
                    callsite_rank: rank,
                    confidence: conf.min(edge.confidence),
                    node: owner,
                };
                remember(d, &mut derived);
            }
        }
        let mut all: Vec<Derived> = derived.into_values().collect();
        all.sort_by(|a, b| compare_derived(a, b));
        for d in all.into_iter().take(remaining_slots) {
            let terms: BTreeSet<String> =
                weighted.iter().filter(|t| node_contains(&d.node, &t.term, t.stem)).map(|t| t.term.clone()).collect();
            seed_relevance.insert(d.node.id.clone(), d.relevance);
            seed_callsite_ranks.insert(d.node.id.clone(), d.callsite_rank);
            node_terms.insert(d.node.id.clone(), terms);
            flow_seed_ids.insert(d.node.id.clone());
            flow_seeds.push(d.node);
        }
    }
    let pair_flows: Vec<(Node, Node, EdgeInfo)> = call_pairs
        .iter()
        .filter_map(|p| Some((nodes.get(&p.source)?.node.clone(), nodes.get(&p.target)?.node.clone(), p.edge.clone())))
        .collect();
    let ctx = FlowContext {
        access: &access,
        selected_ids: picked.set.clone(),
        seed_relevance: &seed_relevance,
        file_priority: &file_priority,
        node_terms: &node_terms,
        term_weights: &term_weights,
        callsite_ranks: &callsite_ranks,
        callsite_weights: &callsite_weights,
        seed_callsite_ranks: &seed_callsite_ranks,
        call_pairs: &pair_flows,
    };
    let traversed = build_directed_flows(&ctx, &flow_seeds, MAX_FLOW_STEPS);
    // Phrase bridges passed stricter evidence than ordinary seeds: emit them first.
    let mut flows: Vec<ScopeFlow> = Vec::new();
    let mut seen_flow: HashSet<String> = HashSet::new();
    let mut remaining_steps = MAX_FLOW_STEPS;
    for f in bridges.iter().map(|(_, e)| ScopeFlow { steps: vec![e.clone()] }).chain(traversed) {
        let key = flow_key(&f.steps);
        if seen_flow.contains(&key) || f.steps.len() > remaining_steps {
            continue;
        }
        seen_flow.insert(key);
        remaining_steps -= f.steps.len();
        flows.push(f);
        if remaining_steps == 0 {
            break;
        }
    }

    // 14. Flow-spine promotion: the primary flow's files may evict the weakest unprotected file.
    let mut pinned_paths: HashSet<String> =
        selected.order.iter().filter(|p| files.get(*p).is_some_and(|f| !f.exact_ids.is_empty())).cloned().collect();
    pinned_paths.extend(source_floor.iter().cloned());
    for p in &bridge_target_paths {
        if selected.contains(p) {
            pinned_paths.insert(p.clone());
        }
    }
    for (p, kind) in &strong_list {
        let architectural = matches!(kind.as_str(), "class" | "struct" | "trait" | "component" | "interface" | "protocol");
        if architectural && selected.contains(p) {
            pinned_paths.insert(p.clone());
        }
    }
    let mut protected: HashSet<String> = HashSet::new();
    let mut flow_node_ids: Vec<String> = Vec::new();
    let primary_ids: HashSet<String> = flows
        .first()
        .map(|f| f.steps.iter().flat_map(|s| [s.source.clone(), s.target.clone()]).collect())
        .unwrap_or_default();
    let all_flow_ids: Vec<String> = flows.iter().flat_map(|f| f.steps.iter()).flat_map(|s| [s.source.clone(), s.target.clone()]).collect();
    for id in all_flow_ids {
        if flow_node_ids.contains(&id) {
            continue;
        }
        flow_node_ids.push(id.clone());
        let Some(n) = access.node(&id) else { continue };
        {
            let f = ensure_file(&mut files, &n.file_path);
            f.node_ids.insert(n.id.clone());
            f.reliable_graph = true;
            f.reasons.insert("flow-spine".into());
        }
        if !primary_ids.contains(&id) {
            continue;
        }
        if selected.contains(&n.file_path) {
            protected.insert(n.file_path.clone());
            continue;
        }
        if selected.len() >= max_files {
            let victim = selected
                .order
                .iter()
                .filter(|p| !pinned_paths.contains(*p) && !protected.contains(*p))
                .min_by(|a, b| cmp_f64(score_of(&files, a), score_of(&files, b)).then(b.cmp(a)))
                .cloned();
            let Some(v) = victim else { continue };
            selected.remove(&v);
        }
        selected.insert(&n.file_path);
        protected.insert(n.file_path.clone());
    }
    let ranked_files = rank_files(&selected, &files);
    let final_picked =
        pick_declarations(&ranked_files, &nodes, &hits, &plan, &concepts, &pair_seeds, &access, max_nodes, true);

    // 15. Test neighbours: test declarations calling the picked production declarations.
    let mut tests: Vec<ScopedCandidate> = Vec::new();
    let mut test_seen: HashSet<String> = HashSet::new();
    for id in &final_picked.order {
        if tests.len() >= MAX_TEST_NEIGHBOURS {
            break;
        }
        let Some(n) = access.node(id) else { continue };
        if is_test_path(&n.file_path) {
            continue;
        }
        for (caller, edge) in access.incoming(id, FLOW_EDGE_KINDS) {
            if tests.len() >= MAX_TEST_NEIGHBOURS {
                break;
            }
            if !is_test_path(&caller.file_path) || final_picked.contains(&caller.id) || !test_seen.insert(caller.id.clone()) {
                continue;
            }
            let _ = edge;
            tests.push(ScopedCandidate {
                id: caller.id.clone(),
                score: 0.0,
                reasons: vec![format!("test-of:{}", n.name)],
                category: "test",
                explanations: vec![format!("tests `{}`", n.name)],
            });
        }
    }
    sel.tests = tests;

    // 16. Assemble.
    let candidate_rank: HashMap<&String, usize> = final_picked.order.iter().enumerate().map(|(i, id)| (id, i)).collect();
    let flow_rank: HashMap<&String, usize> = flow_node_ids.iter().enumerate().map(|(i, id)| (id, i)).collect();
    sel.covered_terms = {
        let mut v: Vec<String> = literal
            .iter()
            .filter(|t| ranked_files.iter().any(|p| files.get(p).is_some_and(|f| f.terms.contains(*t))))
            .cloned()
            .collect();
        v.sort();
        v
    };
    for p in &ranked_files {
        let f = files.get(p).cloned().unwrap_or_default();
        let mut ids: Vec<String> = f
            .node_ids
            .iter()
            .filter(|id| candidate_rank.contains_key(id) || flow_rank.contains_key(id))
            .cloned()
            .collect();
        ids.sort_by(|a, b| {
            candidate_rank
                .get(a)
                .unwrap_or(&INF)
                .cmp(candidate_rank.get(b).unwrap_or(&INF))
                .then(flow_rank.get(a).unwrap_or(&INF).cmp(flow_rank.get(b).unwrap_or(&INF)))
                .then(a.cmp(b))
        });
        let status = parse_status.get(p).cloned().unwrap_or_else(|| "failed".into());
        let mut text_hits: Vec<ChunkHit> = Vec::new();
        for &h in &f.text_hits {
            if text_hits.len() >= 4 {
                break;
            }
            let hit = &hits[h];
            if !text_hits.iter().any(|t| t.start_line == hit.start_line && t.end_line == hit.end_line) {
                text_hits.push(hit.clone());
            }
        }
        sel.files.push(RankedScopeFile {
            file_path: p.clone(),
            score: (f.score * 1e6).round() / 1e6,
            reasons: f.reasons.iter().cloned().collect(),
            text_only: ids.is_empty() || !f.reliable_graph || status != "ok",
            node_ids: ids,
            text_hits,
            parse_status: status,
        });
    }
    for id in &final_picked.order {
        let Some(e) = nodes.get(id) else { continue };
        sel.candidates.push(ScopedCandidate {
            id: id.clone(),
            score: ((e.fused() + e.region_strength() + if e.exact { 2.0 } else { 0.0 }) * 1e6).round() / 1e6,
            reasons: e.reasons.iter().cloned().collect(),
            explanations: e.explanations.iter().cloned().collect(),
            category: if is_low_value_graph_path(&e.node.file_path) {
                "test"
            } else if e.graph > e.rrf {
                "neighbor"
            } else {
                "direct"
            },
        });
    }
    sel.flows = flows;
    sel.matched_count = nodes.len();
    sel.evidence_strength = if sel.files.is_empty() {
        "none"
    } else if sel.files.iter().any(|f| f.reasons.iter().any(|r| r.starts_with("exact:")))
        || sel.covered_terms.len() >= path_terms.len().min(3)
    {
        "strong"
    } else if !sel.covered_terms.is_empty() {
        "moderate"
    } else {
        "weak"
    };
    let mut all_ids: Vec<String> = final_picked.order.clone();
    all_ids.extend(flow_node_ids);
    all_ids.extend(sel.tests.iter().map(|t| t.id.clone()));
    for f in &sel.files {
        all_ids.extend(f.node_ids.iter().cloned());
    }
    for id in all_ids {
        if let Some(n) = access.node(&id) {
            sel.nodes.insert(id, n);
        }
    }
    sel
}

/// Declarations a source-chunk hit may be bridged to.
fn is_anchor_for_region(node: &Node) -> bool {
    is_source_anchor_kind(node)
}

/// `called by `x`` / `calls `x`` / `extends `x`` ... from the perspective of the neighbour.
fn describe_edge(edge: &EdgeInfo, current_id: &str, current_name: &str) -> String {
    let outgoing = edge.source == current_id;
    let call = matches!(edge.kind.as_str(), "calls" | "calls_trait_method" | "possible_call");
    match (call, outgoing, edge.kind.as_str()) {
        (true, true, _) => format!("called by `{}`", current_name),
        (true, false, _) => format!("calls `{}`", current_name),
        (_, true, "instantiates") => format!("instantiated by `{}`", current_name),
        (_, false, "instantiates") => format!("instantiates `{}`", current_name),
        (_, _, "implements" | "impl_of" | "extends" | "overrides") => {
            format!("implements or implemented by `{}`", current_name)
        }
        (_, true, "contains") => format!("contained in `{}`", current_name),
        (_, false, "contains") => format!("contains `{}`", current_name),
        (_, true, k) => format!("target of {} from `{}`", k, current_name),
        (_, false, k) => format!("{} `{}`", k, current_name),
    }
}

/// Human-readable explanation of a selection reason.
pub fn explain_reason(reason: &str) -> Option<String> {
    if let Some(id) = reason.strip_prefix("exact:") {
        return Some(format!("defines `{}`", id));
    }
    if let Some(t) = reason.strip_prefix("term:") {
        return Some(format!("matches term `{}`", t));
    }
    match reason {
        "bm25-node" => Some("matched full-text search".into()),
        "source-region" => Some("matched source text".into()),
        "source-region:callsite" => Some("called from matched source text".into()),
        "query-phrase-flow" => Some("bridges a query phrase across files".into()),
        "bm25-call-pair" => Some("calls or is called by another full-query match".into()),
        "vector" => Some("semantically similar (vector)".into()),
        "flow-spine" => Some("on the directed call flow".into()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Directed flows
// ---------------------------------------------------------------------------

struct FlowContext<'c, 'a> {
    access: &'c Access<'a>,
    selected_ids: HashSet<String>,
    seed_relevance: &'c HashMap<String, f64>,
    file_priority: &'c HashMap<String, f64>,
    node_terms: &'c HashMap<String, BTreeSet<String>>,
    term_weights: &'c [(String, f64)],
    callsite_ranks: &'c HashMap<String, usize>,
    callsite_weights: &'c HashMap<String, f64>,
    seed_callsite_ranks: &'c HashMap<String, usize>,
    /// (source, target, edge) of whole-query call pairs.
    call_pairs: &'c [(Node, Node, EdgeInfo)],
}

impl FlowContext<'_, '_> {
    fn relevance(&self, id: &str) -> f64 {
        self.seed_relevance.get(id).copied().unwrap_or(0.0)
    }
    fn rank(&self, e: &EdgeInfo) -> Option<usize> {
        self.callsite_ranks.get(&edge_key(e)).copied().filter(|r| *r != INF)
    }
    fn weight(&self, e: &EdgeInfo) -> f64 {
        self.callsite_weights.get(&edge_key(e)).copied().unwrap_or(0.0)
    }
    fn endpoint_name_relevance(&self, node: &Node) -> f64 {
        let cs = search_components(&node.name);
        self.term_weights
            .iter()
            .filter(|(t, _)| {
                cs.contains(t) || (t.len() > 3 && t.ends_with('s') && cs.contains(&t[..t.len() - 1]))
            })
            .map(|(_, w)| w)
            .sum()
    }
}

#[derive(Clone)]
struct FlowCand {
    steps: Vec<EdgeInfo>,
    starts_selected: bool,
    starts_in_test: bool,
    selected_endpoints: usize,
    relevant_endpoints: usize,
    execution_priority: u8,
    endpoint_relevance: f64,
    endpoint_file_priority: f64,
    path_relevance: f64,
    terminal_relevance: f64,
    terminal_name_relevance: f64,
    seed_relevance: f64,
    query_callsite_steps: usize,
    best_query_callsite_rank: usize,
    best_query_callsite_weight: f64,
    terminal_public: bool,
    crosses_file: bool,
    terminal_file_callsite_count: usize,
    terms: BTreeSet<String>,
}

fn is_contiguous_subpath(candidate: &[EdgeInfo], container: &[EdgeInfo]) -> bool {
    if candidate.len() > container.len() {
        return false;
    }
    let ck: Vec<String> = candidate.iter().map(edge_key).collect();
    let tk: Vec<String> = container.iter().map(edge_key).collect();
    (0..=tk.len() - ck.len()).any(|off| ck.iter().enumerate().all(|(i, k)| *k == tk[off + i]))
}

fn is_terminal_subpath(candidate: &[EdgeInfo], container: &[EdgeInfo]) -> bool {
    if candidate.len() > container.len() {
        return false;
    }
    let off = container.len() - candidate.len();
    candidate.iter().enumerate().all(|(i, e)| edge_key(e) == edge_key(&container[off + i]))
}

/// Preserve destination-file diversity inside the per-node branch cap: one culminating
/// semantic call per (query region, destination file), then the original relevance order.
fn diverse_flow_branches(entries: Vec<(Node, EdgeInfo)>, limit: usize, ctx: &FlowContext) -> Vec<(Node, EdgeInfo)> {
    if entries.len() <= limit {
        return entries;
    }
    let index: HashMap<String, usize> = entries.iter().enumerate().map(|(i, e)| (edge_key(&e.1), i)).collect();
    let mut groups: BTreeMap<(usize, String), Vec<&(Node, EdgeInfo)>> = BTreeMap::new();
    for e in &entries {
        if let Some(rank) = ctx.rank(&e.1) {
            groups.entry((rank, e.0.file_path.clone())).or_default().push(e);
        }
    }
    let semantic_priority = |e: &EdgeInfo| -> u8 {
        if is_call_kind(&e.kind) {
            2
        } else if e.kind == "references" {
            1
        } else {
            0
        }
    };
    let mut reps: Vec<(usize, usize, &(Node, EdgeInfo))> = groups
        .into_iter()
        .filter_map(|((rank, _), group)| {
            let distinct: HashSet<&String> = group.iter().filter(|e| is_call_kind(&e.1.kind)).map(|e| &e.0.id).collect();
            let rep = group.into_iter().min_by(|a, b| {
                semantic_priority(&b.1)
                    .cmp(&semantic_priority(&a.1))
                    .then(b.1.line.unwrap_or(-1).cmp(&a.1.line.unwrap_or(-1)))
                    .then(a.1.column.unwrap_or(i64::MAX).cmp(&b.1.column.unwrap_or(i64::MAX)))
                    .then(index[&edge_key(&a.1)].cmp(&index[&edge_key(&b.1)]))
            })?;
            Some((rank, distinct.len(), rep))
        })
        .collect();
    reps.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(b.1.cmp(&a.1))
            .then(index[&edge_key(&a.2 .1)].cmp(&index[&edge_key(&b.2 .1)]))
    });
    let mut out: Vec<(Node, EdgeInfo)> = reps.into_iter().take(limit).map(|(_, _, e)| e.clone()).collect();
    let mut keys: HashSet<String> = out.iter().map(|e| edge_key(&e.1)).collect();
    for e in &entries {
        if out.len() >= limit {
            break;
        }
        if keys.insert(edge_key(&e.1)) {
            out.push(e.clone());
        }
    }
    out
}

fn build_directed_flows(ctx: &FlowContext, seeds: &[Node], max_steps: usize) -> Vec<ScopeFlow> {
    let access = ctx.access;
    let hop_limit = 7.min(max_steps);
    let mut remaining_work = FLOW_TRAVERSAL_WORK_BUDGET;
    let mut ordered: Vec<&Node> = seeds.iter().collect();
    ordered.sort_by(|a, b| {
        let ra = ctx.seed_callsite_ranks.get(&a.id).copied().unwrap_or(INF);
        let rb = ctx.seed_callsite_ranks.get(&b.id).copied().unwrap_or(INF);
        (rb != INF)
            .cmp(&(ra != INF))
            .then(ra.cmp(&rb))
            .then(cmp_f64(ctx.relevance(&b.id), ctx.relevance(&a.id)))
            .then(ctx.selected_ids.contains(&b.id).cmp(&ctx.selected_ids.contains(&a.id)))
            .then(is_low_value_graph_path(&a.file_path).cmp(&is_low_value_graph_path(&b.file_path)))
            .then(a.id.cmp(&b.id))
    });
    struct Item {
        seed: Node,
        node: Node,
        steps: Vec<EdgeInfo>,
        visited: HashSet<String>,
        unnamed: usize,
        root: bool,
    }
    let mut queue: VecDeque<Item> = ordered
        .iter()
        .map(|s| Item {
            seed: (*s).clone(),
            node: (*s).clone(),
            steps: Vec::new(),
            visited: [s.id.clone()].into_iter().collect(),
            unnamed: 0,
            root: true,
        })
        .collect();
    let mut pending_roots = ordered.len();
    let mut flows: Vec<FlowCand> = Vec::new();
    while remaining_work > 0 {
        let Some(cur) = queue.pop_front() else { break };
        if cur.steps.len() >= hop_limit {
            continue;
        }
        if cur.root {
            pending_roots -= 1;
        }
        remaining_work -= 1;
        let mut outgoing: Vec<(Node, EdgeInfo)> = access
            .outgoing(&cur.node.id, FLOW_TRAVERSAL_EDGE_KINDS)
            .into_iter()
            .filter(|(n, e)| e.confidence >= MIN_TRUSTED_CONFIDENCE && (e.kind != "contains" || is_unnamed_flow_node(n)))
            .collect();
        outgoing.sort_by(|a, b| {
            let (ra, rb) = (ctx.rank(&a.1).unwrap_or(INF), ctx.rank(&b.1).unwrap_or(INF));
            (rb != INF)
                .cmp(&(ra != INF))
                .then(cmp_f64(ctx.weight(&b.1), ctx.weight(&a.1)))
                .then(ra.cmp(&rb))
                .then(ctx.selected_ids.contains(&b.0.id).cmp(&ctx.selected_ids.contains(&a.0.id)))
                .then(b.0.is_exported.cmp(&a.0.is_exported))
                .then(cmp_f64(ctx.relevance(&b.0.id), ctx.relevance(&a.0.id)))
                .then(compare_neighbor(a, b))
        });
        let mut callsite_targets: HashMap<(usize, String), HashSet<String>> = HashMap::new();
        for (n, e) in &outgoing {
            if !is_call_kind(&e.kind) {
                continue;
            }
            if let Some(rank) = ctx.rank(e) {
                callsite_targets.entry((rank, n.file_path.clone())).or_default().insert(n.id.clone());
            }
        }
        for (n, edge) in diverse_flow_branches(outgoing, FLOW_BRANCH_LIMIT, ctx) {
            if remaining_work <= pending_roots {
                break;
            }
            if cur.visited.contains(&n.id) {
                continue;
            }
            let unnamed = cur.unnamed + usize::from(is_unnamed_flow_node(&n));
            if unnamed > 1 {
                continue;
            }
            let mut steps = cur.steps.clone();
            steps.push(edge.clone());
            let mut visited = cur.visited.clone();
            visited.insert(n.id.clone());
            remaining_work -= 1;
            let seed = &cur.seed;
            let mut endpoint_ids: Vec<String> = vec![seed.id.clone()];
            for s in &steps {
                for id in [&s.source, &s.target] {
                    if !endpoint_ids.contains(id) {
                        endpoint_ids.push(id.clone());
                    }
                }
            }
            let execution_priority: u8 = if steps.iter().any(|s| is_call_kind(&s.kind)) {
                2
            } else if steps.iter().any(|s| s.kind == "references") {
                1
            } else {
                0
            };
            if execution_priority == 0 || edge.kind == "contains" {
                if steps.len() < hop_limit {
                    queue.push_back(Item {
                        seed: seed.clone(),
                        node: n,
                        steps,
                        visited,
                        unnamed,
                        root: false,
                    });
                }
                continue;
            }
            let start_rel = ctx.relevance(&seed.id);
            let terminal_rel = ctx.relevance(&n.id);
            let ranks: Vec<usize> = steps.iter().filter_map(|s| ctx.rank(s)).collect();
            let weights: Vec<f64> = steps.iter().map(|s| ctx.weight(s)).collect();
            let terms: BTreeSet<String> = endpoint_ids
                .iter()
                .flat_map(|id| ctx.node_terms.get(id).cloned().unwrap_or_default())
                .collect();
            flows.push(FlowCand {
                steps: steps.clone(),
                starts_selected: ctx.selected_ids.contains(&seed.id),
                starts_in_test: is_low_value_graph_path(&seed.file_path),
                seed_relevance: start_rel,
                selected_endpoints: endpoint_ids.iter().filter(|id| ctx.selected_ids.contains(*id)).count(),
                relevant_endpoints: [start_rel, terminal_rel].iter().filter(|s| **s >= STRONG_FLOW_RELEVANCE).count(),
                execution_priority,
                endpoint_relevance: start_rel + terminal_rel,
                endpoint_file_priority: ctx.file_priority.get(&seed.file_path).copied().unwrap_or(0.0)
                    + ctx.file_priority.get(&n.file_path).copied().unwrap_or(0.0),
                path_relevance: endpoint_ids
                    .iter()
                    .map(|id| ctx.relevance(id))
                    .filter(|s| *s >= STRONG_FLOW_RELEVANCE)
                    .sum(),
                terminal_relevance: terminal_rel,
                terminal_name_relevance: ctx.endpoint_name_relevance(&n),
                query_callsite_steps: ranks.len(),
                best_query_callsite_rank: ranks.iter().copied().min().unwrap_or(INF),
                best_query_callsite_weight: weights.iter().copied().fold(0.0, f64::max),
                terminal_public: n.is_exported,
                crosses_file: seed.file_path != n.file_path,
                terminal_file_callsite_count: ranks
                    .iter()
                    .map(|r| callsite_targets.get(&(*r, n.file_path.clone())).map(|s| s.len()).unwrap_or(0))
                    .max()
                    .unwrap_or(0),
                terms,
            });
            if steps.len() < hop_limit {
                queue.push_back(Item {
                    seed: seed.clone(),
                    node: n,
                    steps,
                    visited,
                    unnamed,
                    root: false,
                });
            }
        }
    }
    flows.sort_by(|a, b| {
        a.starts_in_test
            .cmp(&b.starts_in_test)
            .then(b.execution_priority.cmp(&a.execution_priority))
            .then((b.query_callsite_steps > 0).cmp(&(a.query_callsite_steps > 0)))
            .then(cmp_f64(b.best_query_callsite_weight, a.best_query_callsite_weight))
            .then(a.best_query_callsite_rank.cmp(&b.best_query_callsite_rank))
            .then(b.terminal_public.cmp(&a.terminal_public))
            .then(b.query_callsite_steps.cmp(&a.query_callsite_steps))
            .then(b.relevant_endpoints.cmp(&a.relevant_endpoints))
            .then(cmp_f64(b.path_relevance, a.path_relevance))
            .then(cmp_f64(b.endpoint_relevance, a.endpoint_relevance))
            .then(cmp_f64(b.terminal_relevance, a.terminal_relevance))
            .then(cmp_f64(b.endpoint_file_priority, a.endpoint_file_priority))
            .then(b.starts_selected.cmp(&a.starts_selected))
            .then(b.selected_endpoints.cmp(&a.selected_endpoints))
            .then(cmp_f64(b.seed_relevance, a.seed_relevance))
            .then(a.steps.len().cmp(&b.steps.len()))
            .then(flow_key(&a.steps).cmp(&flow_key(&b.steps)))
    });
    let mut seen: HashSet<String> = HashSet::new();
    let unique: Vec<FlowCand> = flows.into_iter().filter(|f| seen.insert(flow_key(&f.steps))).collect();
    let unique_order: HashMap<String, usize> = unique.iter().enumerate().map(|(i, f)| (flow_key(&f.steps), i)).collect();
    let mut provenance_ranks: Vec<usize> = unique
        .iter()
        .filter(|f| f.steps.len() == 1 && f.best_query_callsite_rank != INF)
        .map(|f| f.best_query_callsite_rank)
        .collect();
    provenance_ranks.sort();
    provenance_ranks.dedup();
    let edge_line_within = |e: &EdgeInfo, n: &Node| e.line.is_none_or(|l| l >= n.start_line && l <= n.end_line);

    // A proven whole-query call pair may complete through one direct planner callback.
    let mut completions: Vec<(Vec<EdgeInfo>, f64, usize, BTreeSet<String>)> = Vec::new();
    for (pair_order, (source, target, pair_edge)) in ctx.call_pairs.iter().enumerate() {
        if source.id == target.id
            || pair_edge.source != source.id
            || pair_edge.target != target.id
            || !is_call_kind(&pair_edge.kind)
            || pair_edge.confidence < MIN_TRUSTED_CONFIDENCE
            || is_unnamed_flow_node(source)
            || is_unnamed_flow_node(target)
            || !is_source_anchor_kind(source)
            || !is_source_anchor_kind(target)
            || is_low_value_graph_path(&source.file_path)
            || is_low_value_graph_path(&target.file_path)
            || source.file_path != target.file_path
        {
            continue;
        }
        let pk = edge_key(pair_edge);
        let prefix = access
            .outgoing(&source.id, CALL_EDGE_KINDS)
            .into_iter()
            .filter(|(n, e)| {
                edge_key(e) == pk
                    && e.confidence >= MIN_TRUSTED_CONFIDENCE
                    && e.source == source.id
                    && e.target == target.id
                    && n.id == target.id
                    && edge_line_within(e, source)
            })
            .min_by(|a, b| {
                cmp_f64(b.1.confidence, a.1.confidence)
                    .then(compare_edge_callsite(&a.1, &b.1))
                    .then(edge_key(&a.1).cmp(&edge_key(&b.1)))
            });
        let Some((_, prefix)) = prefix else { continue };
        let mut callbacks: Vec<(Node, EdgeInfo)> = access
            .outgoing(&target.id, &["contains"])
            .into_iter()
            .filter(|(cb, e)| {
                e.kind == "contains"
                    && e.confidence >= MIN_TRUSTED_CONFIDENCE
                    && e.source == target.id
                    && e.target == cb.id
                    && cb.id != source.id
                    && cb.id != target.id
                    && cb.container_id.as_deref() == Some(target.id.as_str())
                    && cb.file_path == target.file_path
                    && is_unnamed_flow_node(cb)
                    && edge_line_within(e, target)
            })
            .collect();
        callbacks.sort_by(|a, b| {
            cmp_f64(b.1.confidence, a.1.confidence)
                .then(compare_edge_callsite(&a.1, &b.1))
                .then(a.0.id.cmp(&b.0.id))
        });
        for (cb, containment) in callbacks.into_iter().take(CALL_PAIR_CALLBACKS_LIMIT) {
            let mut terminals: Vec<(Node, EdgeInfo)> = access
                .outgoing(&cb.id, CALL_EDGE_KINDS)
                .into_iter()
                .filter(|(t, e)| {
                    is_call_kind(&e.kind)
                        && e.confidence >= MIN_TRUSTED_CONFIDENCE
                        && e.source == cb.id
                        && e.target == t.id
                        && t.id != source.id
                        && t.id != target.id
                        && t.id != cb.id
                        && !is_unnamed_flow_node(t)
                        && is_source_anchor_kind(t)
                        && !is_low_value_graph_path(&t.file_path)
                        && t.file_path != target.file_path
                        && is_public(t)
                        && ctx.endpoint_name_relevance(t) >= MIN_CALL_PAIR_CALLBACK_TERMINAL_NAME_RELEVANCE
                        && edge_line_within(e, &cb)
                })
                .collect();
            terminals.sort_by(|a, b| {
                cmp_f64(ctx.endpoint_name_relevance(&b.0), ctx.endpoint_name_relevance(&a.0))
                    .then(cmp_f64(b.1.confidence, a.1.confidence))
                    .then(compare_edge_callsite(&a.1, &b.1))
                    .then(a.0.id.cmp(&b.0.id))
            });
            for (t, e) in terminals.into_iter().take(CALL_PAIR_CALLBACK_EDGES_LIMIT) {
                let mut terms = BTreeSet::new();
                for id in [&source.id, &target.id, &cb.id, &t.id] {
                    terms.extend(ctx.node_terms.get(id).cloned().unwrap_or_default());
                }
                completions.push((vec![prefix.clone(), containment.clone(), e], ctx.endpoint_name_relevance(&t), pair_order, terms));
            }
        }
    }
    completions.sort_by(|a, b| {
        cmp_f64(b.1, a.1).then(a.2.cmp(&b.2)).then(flow_key(&a.0).cmp(&flow_key(&b.0)))
    });
    let completion = completions.into_iter().next();

    // Named-owner repair for anonymous callback provenance.
    let owner_cache: RefCell<HashMap<String, Option<(Node, EdgeInfo)>>> = RefCell::new(HashMap::new());
    let named_owner_bridge = |steps: &[EdgeInfo]| -> Option<(Node, EdgeInfo)> {
        if steps.len() != 1 {
            return None;
        }
        let orphan = &steps[0];
        if !is_call_kind(&orphan.kind) && orphan.kind != "references" {
            return None;
        }
        let key = edge_key(orphan);
        if let Some(c) = owner_cache.borrow().get(&key) {
            return c.clone();
        }
        let result = (|| {
            let cb = access.node(&orphan.source)?;
            if !is_unnamed_flow_node(&cb) {
                return None;
            }
            let mut owners: Vec<(Node, EdgeInfo)> = access
                .incoming(&cb.id, &["contains"])
                .into_iter()
                .filter(|(n, e)| {
                    e.confidence >= MIN_TRUSTED_CONFIDENCE
                        && e.source == n.id
                        && e.target == cb.id
                        && n.kind != "file"
                        && !is_unnamed_flow_node(n)
                        && n.id != orphan.target
                        && n.file_path == cb.file_path
                })
                .collect();
            owners.sort_by(|a, b| {
                let own = |x: &(Node, EdgeInfo)| cb.container_id.as_deref() == Some(x.0.id.as_str());
                let encl = |x: &(Node, EdgeInfo)| x.0.start_line <= cb.start_line && x.0.end_line >= cb.end_line;
                own(b)
                    .cmp(&own(a))
                    .then(encl(b).cmp(&encl(a)))
                    .then(cmp_f64(b.1.confidence, a.1.confidence))
                    .then((a.0.end_line - a.0.start_line).cmp(&(b.0.end_line - b.0.start_line)))
                    .then(a.0.id.cmp(&b.0.id))
            });
            owners.into_iter().next()
        })();
        owner_cache.borrow_mut().insert(key, result.clone());
        result
    };
    let prefer_named_owner = |entry: &FlowCand| -> FlowCand {
        if entry.steps.len() != 1 {
            return entry.clone();
        }
        let orphan = &entry.steps[0];
        let Some(src) = access.node(&orphan.source) else { return entry.clone() };
        if !is_unnamed_flow_node(&src) {
            return entry.clone();
        }
        let ok = edge_key(orphan);
        let mut containers: Vec<&FlowCand> = unique
            .iter()
            .filter(|c| {
                if c.steps.len() <= entry.steps.len() || !is_contiguous_subpath(&entry.steps, &c.steps) {
                    return false;
                }
                let Some(off) = c.steps.iter().position(|e| edge_key(e) == ok) else { return false };
                if off == 0 || off != c.steps.len() - 1 {
                    return false;
                }
                let bridge = &c.steps[off - 1];
                bridge.kind == "contains"
                    && bridge.target == orphan.source
                    && access.node(&bridge.source).is_some_and(|o| !is_unnamed_flow_node(&o))
            })
            .collect();
        containers.sort_by(|a, b| {
            b.steps
                .len()
                .cmp(&a.steps.len())
                .then(unique_order.get(&flow_key(&a.steps)).cmp(&unique_order.get(&flow_key(&b.steps))))
        });
        if let Some(c) = containers.first() {
            return (*c).clone();
        }
        if let Some((_, bridge)) = named_owner_bridge(&entry.steps) {
            let mut e = entry.clone();
            e.steps = vec![bridge, orphan.clone()];
            return e;
        }
        let mut alternates: Vec<(EdgeInfo, (Node, EdgeInfo), usize)> = access
            .incoming(&orphan.target, &[orphan.kind.as_str()])
            .into_iter()
            .filter_map(|(n, e)| {
                if e.confidence < MIN_TRUSTED_CONFIDENCE
                    || e.target != orphan.target
                    || e.kind != orphan.kind
                    || edge_key(&e) == ok
                    || !is_unnamed_flow_node(&n)
                {
                    return None;
                }
                let bridge = named_owner_bridge(std::slice::from_ref(&e))?;
                let rank = ctx.rank(&e).unwrap_or(INF);
                let same_rank = rank != INF && rank == entry.best_query_callsite_rank;
                let owner_relevant =
                    ctx.selected_ids.contains(&bridge.0.id) || ctx.relevance(&bridge.0.id) >= STRONG_FLOW_RELEVANCE;
                (same_rank || owner_relevant).then_some((e, bridge, rank))
            })
            .collect();
        alternates.sort_by(|a, b| {
            (b.2 != INF)
                .cmp(&(a.2 != INF))
                .then(a.2.cmp(&b.2))
                .then(cmp_f64(ctx.weight(&b.0), ctx.weight(&a.0)))
                .then(cmp_f64(ctx.relevance(&b.1 .0.id), ctx.relevance(&a.1 .0.id)))
                .then(is_low_value_graph_path(&a.1 .0.file_path).cmp(&is_low_value_graph_path(&b.1 .0.file_path)))
                .then(b.0.line.unwrap_or(-1).cmp(&a.0.line.unwrap_or(-1)))
                .then(edge_key(&a.0).cmp(&edge_key(&b.0)))
        });
        match alternates.into_iter().next() {
            Some((e, (_, bridge), _)) => {
                let mut c = entry.clone();
                c.steps = vec![bridge, e];
                c
            }
            None => entry.clone(),
        }
    };
    let recovered: Vec<FlowCand> = unique
        .iter()
        .filter(|f| f.steps.len() == 1 && named_owner_bridge(&f.steps).is_none())
        .map(&prefer_named_owner)
        .filter(|p| p.steps.len() > 1)
        .collect();
    let recovered_keys: HashSet<String> = recovered.iter().map(|f| flow_key(&f.steps)).collect();
    let compare_provenance = |a: &FlowCand, b: &FlowCand| -> std::cmp::Ordering {
        let (ea, eb) = (&a.steps[0], &b.steps[0]);
        let same = a.best_query_callsite_rank == b.best_query_callsite_rank && ea.target == eb.target;
        let named = if same {
            named_owner_bridge(&b.steps).is_some().cmp(&named_owner_bridge(&a.steps).is_some())
        } else {
            std::cmp::Ordering::Equal
        };
        let (ca, cb) = (a.terminal_file_callsite_count >= 3, b.terminal_file_callsite_count >= 3);
        let (xa, xb) = (ca && a.crosses_file, cb && b.crosses_file);
        b.execution_priority
            .cmp(&a.execution_priority)
            .then(recovered_keys.contains(&flow_key(&b.steps)).cmp(&recovered_keys.contains(&flow_key(&a.steps))))
            .then(named)
            .then(xb.cmp(&xa))
            .then(cb.cmp(&ca))
            .then(if ca && cb {
                b.terminal_file_callsite_count.cmp(&a.terminal_file_callsite_count)
            } else {
                a.best_query_callsite_rank.cmp(&b.best_query_callsite_rank)
            })
            .then(if xa && xb {
                eb.line.unwrap_or(-1).cmp(&ea.line.unwrap_or(-1))
            } else {
                std::cmp::Ordering::Equal
            })
            .then(b.selected_endpoints.cmp(&a.selected_endpoints))
            .then(b.relevant_endpoints.cmp(&a.relevant_endpoints))
            .then(cmp_f64(b.terminal_name_relevance, a.terminal_name_relevance))
            .then(cmp_f64(b.terminal_relevance, a.terminal_relevance))
            .then(cmp_f64(b.endpoint_relevance, a.endpoint_relevance))
            .then(cmp_f64(b.path_relevance, a.path_relevance))
            .then(cmp_f64(b.best_query_callsite_weight, a.best_query_callsite_weight))
            .then(b.terminal_file_callsite_count.cmp(&a.terminal_file_callsite_count))
            .then(b.crosses_file.cmp(&a.crosses_file))
            .then(a.best_query_callsite_rank.cmp(&b.best_query_callsite_rank))
            .then(eb.line.unwrap_or(-1).cmp(&ea.line.unwrap_or(-1)))
            .then(ea.column.unwrap_or(i64::MAX).cmp(&eb.column.unwrap_or(i64::MAX)))
            .then(unique_order.get(&flow_key(&a.steps)).unwrap_or(&INF).cmp(unique_order.get(&flow_key(&b.steps)).unwrap_or(&INF)))
    };
    let mut raw_provenance: Vec<FlowCand> = Vec::new();
    for rank in &provenance_ranks {
        let all: Vec<&FlowCand> = unique.iter().filter(|f| f.steps.len() == 1 && f.best_query_callsite_rank == *rank).collect();
        let exec: Vec<&FlowCand> = all.iter().filter(|f| is_call_kind(&f.steps[0].kind)).copied().collect();
        let mut at: Vec<&FlowCand> = if exec.is_empty() { all } else { exec };
        at.sort_by(|a, b| compare_provenance(a, b));
        let Some(primary) = at.first().copied() else { continue };
        let primary_target = primary.steps[0].target.clone();
        let secondary = at.iter().skip(1).find(|f| {
            f.steps[0].target != primary_target
                && (f.relevant_endpoints >= 2 || f.terminal_relevance >= STRONG_FLOW_RELEVANCE || f.selected_endpoints >= 2)
        });
        raw_provenance.push(primary.clone());
        if let Some(s) = secondary {
            raw_provenance.push((*s).clone());
        }
    }
    let mut provenance: Vec<FlowCand> = Vec::new();
    for f in raw_provenance.iter().chain(recovered.iter()) {
        let p = prefer_named_owner(f);
        let k = flow_key(&p.steps);
        match provenance.iter_mut().find(|x| flow_key(&x.steps) == k) {
            Some(x) => *x = p,
            None => provenance.push(p),
        }
    }
    let reserve = 1.max(max_steps.div_ceil(2));
    provenance.sort_by(|a, b| compare_provenance(a, b));
    provenance.truncate(reserve);
    let provenance_keys: HashSet<String> = provenance.iter().map(|f| flow_key(&f.steps)).collect();
    let recovered_reps: Vec<Vec<EdgeInfo>> = provenance
        .iter()
        .filter(|f| recovered_keys.contains(&flow_key(&f.steps)))
        .map(|f| f.steps.clone())
        .collect();
    let explained: HashSet<(usize, String)> = provenance
        .iter()
        .filter_map(|f| {
            if f.steps.len() < 2 {
                return None;
            }
            let terminal = &f.steps[f.steps.len() - 1];
            let bridge = &f.steps[f.steps.len() - 2];
            let owner = access.node(&bridge.source)?;
            (bridge.kind == "contains"
                && bridge.target == terminal.source
                && owner.kind != "file"
                && !is_unnamed_flow_node(&owner)
                && owner.id != terminal.target)
                .then(|| (f.best_query_callsite_rank, terminal.target.clone()))
        })
        .collect();
    let explained_suffix = |f: &FlowCand| -> bool {
        if f.steps.len() != 1 {
            return false;
        }
        let e = &f.steps[0];
        access.node(&e.source).is_some_and(|s| is_unnamed_flow_node(&s))
            && explained.contains(&(f.best_query_callsite_rank, e.target.clone()))
    };
    let mut ranked: Vec<FlowCand> = unique
        .iter()
        .enumerate()
        .filter(|(i, f)| {
            !provenance_keys.contains(&flow_key(&f.steps))
                && !explained_suffix(f)
                && !recovered_reps.iter().any(|r| f.steps.len() > r.len() && is_terminal_subpath(r, &f.steps))
                && !unique.iter().enumerate().any(|(j, o)| {
                    *i != j && f.steps.len() < o.steps.len() && is_contiguous_subpath(&f.steps, &o.steps)
                })
        })
        .map(|(_, f)| f.clone())
        .collect();
    let mut selected: Vec<Vec<EdgeInfo>> = Vec::new();
    let mut selected_steps = 0usize;
    let admit = |steps: &[EdgeInfo], selected: &mut Vec<Vec<EdgeInfo>>, selected_steps: &mut usize| -> bool {
        let keys: Vec<String> = steps.iter().map(edge_key).collect();
        if selected.iter().any(|p| keys.iter().all(|k| p.iter().any(|s| edge_key(s) == *k))) {
            return false;
        }
        let remaining = max_steps.saturating_sub(*selected_steps);
        if remaining == 0 || steps.len() > remaining {
            return false;
        }
        selected.push(steps.to_vec());
        *selected_steps += steps.len();
        true
    };
    let mut covered: BTreeSet<String> = BTreeSet::new();
    if let Some((steps, _, _, terms)) = &completion {
        if admit(steps, &mut selected, &mut selected_steps) {
            covered.extend(terms.iter().cloned());
        }
    }
    for r in &provenance {
        if admit(&r.steps, &mut selected, &mut selected_steps) {
            covered.extend(r.terms.iter().cloned());
        }
    }
    if !ranked.is_empty() {
        let first = ranked.remove(0);
        if admit(&first.steps, &mut selected, &mut selected_steps) {
            covered.extend(first.terms.iter().cloned());
        }
    }
    let marginal = |terms: &BTreeSet<String>| -> f64 {
        terms
            .iter()
            .filter(|t| !covered.contains(*t))
            .map(|t| ctx.term_weights.iter().find(|(x, _)| x == t).map(|(_, w)| *w).unwrap_or(0.0))
            .sum()
    };
    ranked.sort_by(|a, b| b.starts_selected.cmp(&a.starts_selected).then(cmp_f64(marginal(&b.terms), marginal(&a.terms))));
    let mut deferred: Vec<Vec<EdgeInfo>> = Vec::new();
    for f in &ranked {
        if selected_steps >= max_steps {
            break;
        }
        let keys: HashSet<String> = f.steps.iter().map(edge_key).collect();
        if !selected.is_empty() && selected.iter().any(|p| p.iter().any(|s| keys.contains(&edge_key(s)))) {
            deferred.push(f.steps.clone());
            continue;
        }
        admit(&f.steps, &mut selected, &mut selected_steps);
        if selected_steps >= max_steps {
            break;
        }
    }
    for steps in deferred {
        if selected_steps >= max_steps {
            break;
        }
        admit(&steps, &mut selected, &mut selected_steps);
    }
    selected.into_iter().map(|steps| ScopeFlow { steps }).collect()
}

// ---------------------------------------------------------------------------
// Source planning
// ---------------------------------------------------------------------------

/// A numbered source excerpt.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceRange {
    pub start_line: i64,
    pub end_line: i64,
    pub node_ids: Vec<String>,
    pub content: String,
    pub truncated: bool,
    /// `whole-file`, `complete-symbol`, `query-hit`, `callsite`, `signature` or `text-only`.
    pub reason: &'static str,
}

fn numbered(lines: &[&str], start: i64) -> String {
    let last = start + lines.len().saturating_sub(1) as i64;
    let width = last.to_string().len();
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{:>w$}: {}", start + i as i64, l, w = width))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn read_lines(root: &Path, path: &str) -> Option<String> {
    std::fs::read_to_string(root.join(path)).ok()
}

/// One node's source (up to `max_lines` lines, `signature` when cut).
pub fn node_source(content: &str, node: &Node, max_lines: usize) -> Option<SourceRange> {
    let lines: Vec<&str> = content.lines().collect();
    if node.start_line < 1 || lines.is_empty() {
        return None;
    }
    let start = (node.start_line as usize).min(lines.len());
    let end = (node.end_line.max(node.start_line) as usize).min(lines.len());
    let body = &lines[start - 1..end];
    let truncated = max_lines > 0 && body.len() > max_lines;
    let kept = if truncated { &body[..max_lines] } else { body };
    Some(SourceRange {
        start_line: start as i64,
        end_line: start as i64 + kept.len().saturating_sub(1) as i64,
        node_ids: vec![node.id.clone()],
        content: numbered(kept, start as i64),
        truncated,
        reason: if truncated { "signature" } else { "complete-symbol" },
    })
}

#[derive(Debug, Clone)]
struct Planned {
    start: i64,
    end: i64,
    node_ids: Vec<String>,
    reason: &'static str,
    priority: usize,
    /// whole | symbol | signature | window
    kind: &'static str,
    intrinsically_truncated: bool,
}

/// Plan small whole files, complete symbols, or signatures plus tight query/callsite windows,
/// within `max_lines` lines for the file.
pub fn plan_file_source(
    content: &str,
    nodes: &[Node],
    text_hits: &[ChunkHit],
    query_terms: &[String],
    max_lines: usize,
    callsite_lines: &[i64],
) -> Vec<SourceRange> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let n = lines.len() as i64;
    let budget = max_lines.max(1);
    let mut ranges: Vec<Planned> = Vec::new();
    if lines.len() <= 200 && lines.len() <= max_lines {
        ranges.push(Planned {
            start: 1,
            end: n,
            node_ids: nodes.iter().map(|x| x.id.clone()).collect(),
            reason: "whole-file",
            priority: 0,
            kind: "whole",
            intrinsically_truncated: false,
        });
    } else {
        let query_lines: Vec<i64> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| {
                let lower = l.to_lowercase();
                query_terms.iter().any(|t| lower.contains(t.as_str()))
            })
            .map(|(i, _)| i as i64 + 1)
            .collect();
        for (ni, node) in nodes.iter().enumerate() {
            let start = node.start_line.clamp(1, n);
            let end = node.end_line.clamp(start, n);
            let len = (end - start + 1) as usize;
            let base = ni * 10;
            if len <= 160 && len <= budget {
                ranges.push(Planned {
                    start,
                    end,
                    node_ids: vec![node.id.clone()],
                    reason: "complete-symbol",
                    priority: base,
                    kind: "symbol",
                    intrinsically_truncated: false,
                });
                continue;
            }
            ranges.push(Planned {
                start,
                end: end.min(start + 5),
                node_ids: vec![node.id.clone()],
                reason: "signature",
                priority: base,
                kind: "signature",
                intrinsically_truncated: end > start + 5,
            });
            let mut seen: HashSet<i64> = HashSet::new();
            let mut windows: Vec<(i64, &'static str)> = Vec::new();
            for &l in callsite_lines.iter().filter(|l| **l >= start && **l <= end) {
                if seen.insert(l) {
                    windows.push((l, "callsite"));
                }
            }
            for &l in query_lines.iter().filter(|l| **l >= start && **l <= end) {
                if seen.insert(l) {
                    windows.push((l, "query-hit"));
                }
            }
            for (wi, (l, reason)) in windows.into_iter().take(2).enumerate() {
                ranges.push(Planned {
                    start: (l - 12).max(start),
                    end: (l + 12).min(end),
                    node_ids: vec![node.id.clone()],
                    reason,
                    priority: base + wi + 1,
                    kind: "window",
                    intrinsically_truncated: false,
                });
            }
        }
        if nodes.is_empty() {
            let mut seen: HashSet<i64> = HashSet::new();
            let mut windows: Vec<(i64, &'static str)> = Vec::new();
            for &l in callsite_lines {
                if seen.insert(l) {
                    windows.push((l, "callsite"));
                }
            }
            for &l in &query_lines {
                if seen.insert(l) {
                    windows.push((l, "query-hit"));
                }
            }
            for h in text_hits {
                let l = (h.start_line + h.end_line) / 2;
                if seen.insert(l) {
                    windows.push((l, "text-only"));
                }
            }
            for (i, (l, reason)) in windows.into_iter().take(2).enumerate() {
                ranges.push(Planned {
                    start: (l - 12).max(1),
                    end: (l + 12).min(n),
                    node_ids: Vec::new(),
                    reason,
                    priority: i,
                    kind: "window",
                    intrinsically_truncated: false,
                });
            }
        }
    }
    let merged = normalize_ranges(ranges, 10);
    let mut remaining = budget as i64;
    let mut out = Vec::new();
    for r in merged {
        if remaining <= 0 {
            break;
        }
        let end = r.end.min(r.start + remaining - 1);
        let slice = &lines[(r.start - 1) as usize..end as usize];
        out.push(SourceRange {
            start_line: r.start,
            end_line: end,
            node_ids: r.node_ids.clone(),
            content: numbered(slice, r.start),
            truncated: r.intrinsically_truncated || end < r.end,
            reason: r.reason,
        });
        remaining -= slice.len() as i64;
    }
    out
}

fn union_ids(a: &mut Vec<String>, b: &[String]) {
    for id in b {
        if !a.contains(id) {
            a.push(id.clone());
        }
    }
}

fn normalize_ranges(ranges: Vec<Planned>, gap: i64) -> Vec<Planned> {
    let valid: Vec<Planned> = ranges.into_iter().filter(|r| r.end >= r.start).collect();
    // A complete parent already carries its child's source.
    let mut symbols: Vec<Planned> = Vec::new();
    let mut syms: Vec<Planned> = valid.iter().filter(|r| r.kind == "symbol").cloned().collect();
    syms.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)).then(a.priority.cmp(&b.priority)));
    for r in syms {
        if let Some(c) = symbols.iter_mut().find(|c| c.start <= r.start && c.end >= r.end) {
            union_ids(&mut c.node_ids, &r.node_ids);
            c.priority = c.priority.min(r.priority);
            continue;
        }
        symbols.push(r);
    }
    let mut windows: Vec<Planned> = Vec::new();
    let mut wins: Vec<Planned> = valid.iter().filter(|r| r.kind == "window").cloned().collect();
    wins.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)).then(a.priority.cmp(&b.priority)));
    for r in wins {
        if let Some(prior) = windows.last_mut() {
            if r.start <= prior.end + gap + 1 {
                prior.end = prior.end.max(r.end);
                union_ids(&mut prior.node_ids, &r.node_ids);
                if r.priority < prior.priority {
                    prior.priority = r.priority;
                    prior.reason = r.reason;
                }
                continue;
            }
        }
        windows.push(r);
    }
    let mut base: Vec<Planned> = valid.iter().filter(|r| r.kind == "whole" || r.kind == "signature").cloned().collect();
    base.extend(symbols);
    let mut kept: Vec<Planned> = Vec::new();
    for w in windows {
        if let Some(c) = base.iter_mut().find(|r| r.start <= w.start && r.end >= w.end) {
            union_ids(&mut c.node_ids, &w.node_ids);
            c.priority = c.priority.min(w.priority);
            continue;
        }
        if let Some(o) = base.iter_mut().find(|r| r.start <= w.end && r.end >= w.start) {
            o.start = o.start.min(w.start);
            o.end = o.end.max(w.end);
            union_ids(&mut o.node_ids, &w.node_ids);
            o.priority = o.priority.min(w.priority);
            continue;
        }
        kept.push(w);
    }
    base.extend(kept);
    base.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.start.cmp(&b.start)).then(a.end.cmp(&b.end)));
    base
}

// ---------------------------------------------------------------------------
// Compact facts
// ---------------------------------------------------------------------------

/// Caller / callee counts of a node (call-like edges).
pub fn call_counts(conn: &Connection, id: &str) -> (i64, i64) {
    let kinds = "'calls', 'calls_trait_method', 'possible_call', 'instantiates'";
    // Call edges are per call site; counts are of distinct callers / callees.
    let other = |col: &str| if col == "target" { "source" } else { "target" };
    let q = |col: &str| -> i64 {
        conn.query_row(
            &format!("SELECT COUNT(DISTINCT {}) FROM edges WHERE {} = ?1 AND kind IN ({})", other(col), col, kinds),
            params![id],
            |r| r.get(0),
        )
        .unwrap_or(0)
    };
    (q("target"), q("source"))
}

/// Serialized MinHash fingerprint of a node (`mh:64:<hex>:<tokens>`), when indexed.
pub fn node_fingerprint(conn: &Connection, id: &str) -> Option<String> {
    conn.query_row(
        "SELECT minhash, token_count FROM node_minhash WHERE node_id = ?1",
        params![id],
        |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, i64>(1)?)),
    )
    .optional()
    .ok()
    .flatten()
    .map(|(blob, tokens)| format!("mh:{}:{}:{}", blob.len() / 4, hex::encode(blob), tokens))
}

/// Has the file changed on disk since it was indexed?
pub fn file_is_stale(conn: &Connection, root: &Path, path: &str) -> bool {
    let indexed: Option<String> = conn
        .query_row("SELECT content_hash FROM files WHERE path = ?1", params![path], |r| r.get(0))
        .optional()
        .ok()
        .flatten();
    match (indexed, std::fs::read(root.join(path))) {
        (Some(h), Ok(bytes)) => crate::graph::fingerprint::compute_file_hash(&bytes) != h,
        _ => true,
    }
}

/// Indexed parse health: (files, ok, partial, failed).
pub fn parse_health(conn: &Connection) -> Result<(usize, usize, usize, usize)> {
    let mut stmt = conn.prepare("SELECT parse_status, COUNT(*) FROM files GROUP BY parse_status")?;
    let rows: Vec<(String, i64)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.flatten().collect();
    let get = |k: &str| rows.iter().find(|(s, _)| s == k).map(|(_, c)| *c as usize).unwrap_or(0);
    let total = rows.iter().map(|(_, c)| *c as usize).sum();
    Ok((total, get("ok"), get("partial"), get("failed")))
}
