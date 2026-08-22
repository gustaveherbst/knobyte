//! Agent-driven wiki synthesis.
//!
//! Knobyte makes no model calls. It deterministically clusters the code graph, renders the
//! context and prompts for one stage, and validates what an agent's model returns before
//! turning it into operation plans. Nothing is written without `--apply`, and every write goes
//! through the operation engine (preconditions, write-scope checks, audit log).
//!
//! Stages: `architecture_component`, `pattern`, `convention` (per cluster), then `global`
//! (consolidate near-duplicates) and `relationships` (typed edges between entities).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::graph::engine::{map_node_row, NODE_COLUMNS};
use crate::graph::grounding::{readable_ref_for, resolve_grounding_ref, RefResolution};
use crate::graph::models::Node;
use crate::wiki::diagnostics::diag;
use crate::wiki::models::{WikiDiagnostic, WikiEntity};
use crate::wiki::parser::slugify;
use crate::wiki::scope::WikiScope;
use crate::wiki::validate::parse_corpus;

pub const CONFIDENCE_PROMOTED: f64 = 0.7;
pub const CONFIDENCE_IN_FLIGHT: f64 = 0.4;
pub const SYNTHESIS_STAGES: [&str; 3] = ["architecture_component", "pattern", "convention"];
pub const ALL_STAGES: [&str; 5] = [
    "architecture_component",
    "pattern",
    "convention",
    "global",
    "relationships",
];
pub const RELATIONSHIP_TYPES: [&str; 6] = [
    "implements",
    "depends_on",
    "refines",
    "supersedes",
    "constrained_by",
    "related_to",
];

pub fn stage_types(stage: &str) -> &'static [&'static str] {
    match stage {
        "architecture_component" => &["architecture", "component"],
        "pattern" => &["pattern"],
        "convention" => &["convention"],
        _ => &[],
    }
}

fn short_hash(s: &str, n: usize) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex::encode(h.finalize())[..n].to_string()
}

// ---------------------------------------------------------------------------
// Code graph access
// ---------------------------------------------------------------------------

pub struct SynthesisGraph {
    conn: Connection,
}

impl SynthesisGraph {
    pub fn open(graph_db: &Path) -> Option<Self> {
        if !graph_db.exists() {
            return None;
        }
        Connection::open_with_flags(graph_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()
            .map(|conn| Self { conn })
    }

    fn files(&self) -> Vec<String> {
        let Ok(mut stmt) = self.conn.prepare("SELECT path FROM files ORDER BY path") else {
            return Vec::new();
        };
        stmt.query_map([], |r| r.get::<_, String>(0))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    fn nodes_in_file(&self, path: &str) -> Vec<Node> {
        let sql = format!(
            "SELECT {} FROM nodes WHERE file_path = ?1 ORDER BY start_line, id",
            NODE_COLUMNS
        );
        let Ok(mut stmt) = self.conn.prepare(&sql) else {
            return Vec::new();
        };
        stmt.query_map(params![path], map_node_row)
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    fn node(&self, id: &str) -> Option<Node> {
        let sql = format!("SELECT {} FROM nodes WHERE id = ?1", NODE_COLUMNS);
        self.conn.query_row(&sql, params![id], map_node_row).ok()
    }

    fn neighbours(&self, id: &str, outgoing: bool) -> Vec<(String, String)> {
        let sql = if outgoing {
            "SELECT target, kind FROM edges WHERE source = ?1 ORDER BY target"
        } else {
            "SELECT source, kind FROM edges WHERE target = ?1 ORDER BY source"
        };
        let Ok(mut stmt) = self.conn.prepare(sql) else {
            return Vec::new();
        };
        stmt.query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    }

    /// Resolve a readable reference (or node id) to a unique node.
    pub fn resolve(&self, reference: &str) -> Option<Node> {
        match resolve_grounding_ref(&self.conn, reference) {
            Ok(RefResolution::Resolved(n)) => Some(*n),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Clusters
// ---------------------------------------------------------------------------

const IGNORED_SEGMENTS: [&str; 24] = [
    "node_modules",
    "dist",
    "build",
    "out",
    "coverage",
    ".git",
    ".svn",
    ".hg",
    ".knobyte",
    ".next",
    ".nuxt",
    ".turbo",
    ".cache",
    ".venv",
    "venv",
    "__pycache__",
    "target",
    "vendor",
    "bin",
    "obj",
    "tmp",
    "temp",
    ".idea",
    ".vscode",
];
const LOW_VALUE: [&str; 35] = [
    "docs",
    "doc",
    "documentation",
    "scripts",
    "script",
    "examples",
    "example",
    "fixtures",
    "fixture",
    "samples",
    "sample",
    "assets",
    "static",
    "public",
    "images",
    "img",
    "css",
    "styles",
    "fonts",
    ".github",
    ".gitlab",
    ".circleci",
    "e2e",
    "__mocks__",
    "migrations",
    "node_modules",
    "dist",
    "build",
    "coverage",
    "test",
    "tests",
    "__tests__",
    "spec",
    "specs",
    "benches",
];
const CONTAINER_DIRS: [&str; 4] = ["src", "lib", "app", "source"];
const WORKSPACE_WRAPPERS: [&str; 5] = ["packages", "apps", "services", "modules", "crates"];
const SYMBOL_KINDS: [&str; 20] = [
    "function",
    "method",
    "class",
    "struct",
    "interface",
    "trait",
    "protocol",
    "enum",
    "type_alias",
    "namespace",
    "module",
    "component",
    "route",
    "constant",
    "variable",
    "property",
    "field",
    "impl",
    "type",
    "endpoint",
];

/// Cluster name and path prefix for a repository file (None = not clustered).
pub fn resolve_cluster_key(path: &str) -> Option<(String, String)> {
    let normalized = path.replace('\\', "/");
    let normalized = normalized.trim_start_matches("./");
    let segs: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        return None;
    }
    if segs
        .iter()
        .any(|s| IGNORED_SEGMENTS.contains(&s.to_lowercase().as_str()))
    {
        return None;
    }
    let low = |s: &str| LOW_VALUE.contains(&s.to_lowercase().as_str());
    let first = segs[0].to_lowercase();
    if WORKSPACE_WRAPPERS.contains(&first.as_str()) && segs.len() >= 3 {
        let pkg = segs[1];
        let next = segs[2].to_lowercase();
        if CONTAINER_DIRS.contains(&next.as_str()) && segs.len() > 4 {
            let module = segs[3];
            if low(module) {
                return None;
            }
            return Some((module.to_string(), segs[..3].join("/")));
        }
        if low(pkg) {
            return None;
        }
        return Some((pkg.to_string(), segs[..2].join("/")));
    }
    if CONTAINER_DIRS.contains(&first.as_str()) && segs.len() >= 3 {
        let module = segs[1];
        if low(module) {
            return None;
        }
        return Some((module.to_string(), segs[0].to_string()));
    }
    if low(segs[0]) || CONTAINER_DIRS.contains(&first.as_str()) {
        return None;
    }
    Some((segs[0].to_string(), segs[0].to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Cluster {
    pub name: String,
    pub prefix: String,
    pub files: Vec<String>,
    /// Node ids of the cluster's symbols.
    pub node_ids: Vec<String>,
    pub description: String,
}

pub fn find_clusters(graph: &SynthesisGraph, min_files: usize) -> Vec<Cluster> {
    let mut acc: BTreeMap<(String, String), (Vec<String>, Vec<String>)> = BTreeMap::new();
    for path in graph.files() {
        let Some((name, prefix)) = resolve_cluster_key(&path) else {
            continue;
        };
        let entry = acc.entry((name, prefix)).or_default();
        entry.0.push(path.clone());
        for n in graph.nodes_in_file(&path) {
            if SYMBOL_KINDS.contains(&n.kind.as_str()) {
                entry.1.push(n.id);
            }
        }
    }
    let mut out = Vec::new();
    for ((name, prefix), (mut files, mut nodes)) in acc {
        if files.len() < min_files.max(1) || nodes.is_empty() {
            continue;
        }
        files.sort();
        nodes.sort();
        nodes.dedup();
        out.push(Cluster {
            description: format!(
                "Module \"{}\" ({}/): {} files, {} symbols",
                name,
                prefix,
                files.len(),
                nodes.len()
            ),
            name,
            prefix,
            files,
            node_ids: nodes,
        });
    }
    out.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.description.cmp(&b.description))
    });
    out
}

// ---------------------------------------------------------------------------
// Cluster context
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextNode {
    /// Readable grounding reference (`kind:path:qualified_name`): what units ground to.
    pub node_id: String,
    pub kind: String,
    pub name: String,
    pub file_path: String,
    pub importance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docstring: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callers: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub callees: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CodeBlock {
    pub node_id: String,
    pub file_path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub importance: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClusterContext {
    pub cluster: Cluster,
    pub nodes: Vec<ContextNode>,
    pub code_blocks: Vec<CodeBlock>,
    pub truncated: bool,
    pub dropped_blocks: usize,
}

impl ClusterContext {
    pub fn node_ids(&self) -> HashSet<String> {
        self.nodes.iter().map(|n| n.node_id.clone()).collect()
    }
}

fn is_primary(n: &Node) -> bool {
    n.is_exported
        || n.visibility.as_deref() == Some("pub")
        || n.visibility.as_deref() == Some("public")
        || (n.container_id.is_none()
            && !matches!(n.kind.as_str(), "variable" | "field" | "property"))
}

fn tokens(s: &str) -> usize {
    s.len().div_ceil(4)
}

pub fn build_cluster_context(
    graph: &SynthesisGraph,
    project_root: &Path,
    cluster: &Cluster,
    cfg: &crate::config::WikiSynthesisConfig,
) -> ClusterContext {
    let mut nodes: Vec<Node> = cluster
        .node_ids
        .iter()
        .filter_map(|id| graph.node(id))
        .collect();
    nodes.sort_by(|a, b| {
        (!is_primary(a))
            .cmp(&!is_primary(b))
            .then_with(|| a.file_path.cmp(&b.file_path))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });
    let total = nodes.len();
    nodes.truncate(cfg.max_nodes.max(1));
    let mut truncated = total > nodes.len();
    let mut ctx_nodes = Vec::new();
    let mut blocks = Vec::new();
    let mut file_cache: HashMap<String, Vec<String>> = HashMap::new();
    for n in &nodes {
        let primary = is_primary(n);
        let name_of = |id: &str| {
            graph
                .node(id)
                .map(|x| x.qualified_name)
                .unwrap_or_else(|| id.to_string())
        };
        let (callers, callees) = if primary && matches!(n.kind.as_str(), "function" | "method") {
            (
                graph
                    .neighbours(&n.id, false)
                    .into_iter()
                    .filter(|(_, k)| k == "calls")
                    .take(5)
                    .map(|(id, _)| name_of(&id))
                    .collect(),
                graph
                    .neighbours(&n.id, true)
                    .into_iter()
                    .filter(|(_, k)| k == "calls")
                    .take(5)
                    .map(|(id, _)| name_of(&id))
                    .collect(),
            )
        } else {
            (Vec::new(), Vec::new())
        };
        let reference = readable_ref_for(n);
        ctx_nodes.push(ContextNode {
            node_id: reference.clone(),
            kind: n.kind.clone(),
            name: n.qualified_name.clone(),
            file_path: n.file_path.clone(),
            importance: if primary { "primary" } else { "supporting" }.into(),
            signature: n.signature.clone(),
            docstring: n.docstring.clone(),
            callers,
            callees,
        });
        let lines = file_cache.entry(n.file_path.clone()).or_insert_with(|| {
            read_source_contained(project_root, &n.file_path)
                .map(|t| t.lines().map(|l| l.to_string()).collect())
                .unwrap_or_default()
        });
        if lines.is_empty() || n.start_line < 1 {
            continue;
        }
        let pad = if primary {
            cfg.primary_context_lines
        } else {
            0
        };
        let cap = if primary {
            cfg.max_file_lines
        } else {
            cfg.supporting_max_lines
        };
        let from = (n.start_line as usize).saturating_sub(pad).max(1);
        let to = ((n.end_line.max(n.start_line) as usize) + pad)
            .min(lines.len())
            .min(from + cap.max(1) - 1);
        if from > to {
            continue;
        }
        let content = lines[from - 1..to].join("\n");
        if content.trim().is_empty() {
            continue;
        }
        blocks.push(CodeBlock {
            node_id: reference,
            file_path: n.file_path.clone(),
            start_line: from,
            end_line: to,
            importance: if primary { "primary" } else { "supporting" }.into(),
            content,
        });
    }
    // Fit the token budget: drop supporting evidence first, then the tail of primary.
    let budget = cfg.max_tokens.max(1);
    let fixed: usize = ctx_nodes
        .iter()
        .map(|n| tokens(&serde_json::to_string(n).unwrap_or_default()))
        .sum();
    let mut used = fixed;
    let mut kept = Vec::new();
    let mut dropped = 0;
    let (primary, supporting): (Vec<CodeBlock>, Vec<CodeBlock>) =
        blocks.into_iter().partition(|b| b.importance == "primary");
    for b in primary.into_iter().chain(supporting) {
        let cost = tokens(&b.content) + 20;
        if used + cost > budget {
            dropped += 1;
            continue;
        }
        used += cost;
        kept.push(b);
    }
    if dropped > 0 {
        truncated = true;
    }
    ClusterContext {
        cluster: cluster.clone(),
        nodes: ctx_nodes,
        code_blocks: kept,
        truncated,
        dropped_blocks: dropped,
    }
}

// ---------------------------------------------------------------------------
// Prompts
// ---------------------------------------------------------------------------

const SHARED_RULES: &str = r#"You are an expert software architect and code analyst helping build a high-quality knowledge wiki for this repository.

Your job is to extract ONLY clear, reusable, high-signal knowledge from the ClusterContext you are given.
The ClusterContext is code-first: primary symbols, supporting symbols, primary and supporting code evidence.

HARD RULES (non-negotiable):
1. ONLY use information present in the ClusterContext. Never invent or assume missing code, intent, or architecture.
2. Prefer PRIMARY evidence. Supporting evidence may refine a unit; it should not be the sole basis for one.
3. Every unit MUST be grounded in one or more nodeIds copied exactly from the context (they are readable references like `function:src/x.rs:name`). If you cannot ground it, do not emit it.
4. Units must be ATOMIC (one clear concept each) and SEMANTIC (reusable, not a restated signature).
5. Prefer a few excellent units over many mediocre ones. An empty result is a valid and often correct answer.
6. Titles are concise (at most about 80 characters). Summaries are one to three sentences (10-500 characters). Bodies (at least 20 characters) may use Markdown.
7. Always assign a confidence between 0 and 1, and be conservative:
   - 0.85-1.0 = strong, repeated, or very clear evidence
   - 0.65-0.84 = plausible and grounded, but thinner evidence
   - below 0.65 = usually do not emit at all
8. Knobyte applies its own confidence gates after you: at least 0.7 is proposed as "promoted", at least 0.4 as "in_flight", anything lower is rejected. Nothing is applied without human review.
9. Output ONLY valid JSON matching the requested schema. No prose around it, no Markdown fences."#;

fn output_schema(types: &[&str]) -> String {
    format!(
        r#"Output schema:
{{
  "units": [
    {{
      "type": {},
      "title": string,
      "summary": string,
      "body": string,
      "confidence": number,
      "grounding": {{ "nodeIds": string[], "evidence": optional array of {{ "nodeId": string, "quote"?: string, "reason"?: string }} }},
      "reasoning": optional string
    }}
  ]
}}"#,
        types
            .iter()
            .map(|t| format!("\"{}\"", t))
            .collect::<Vec<_>>()
            .join(" | ")
    )
}

fn stage_system(stage: &str) -> String {
    let focus = match stage {
        "architecture_component" => "You are performing stage 1 of synthesis: architecture and component extraction.\n\nFocus exclusively on the architectural boundaries and responsibilities of this cluster, the major components or modules and what each owns, and what this cluster is responsible for versus what it depends on. Do NOT extract patterns or coding conventions in this stage.\n\nBad units: restating a single function; vague claims with no concrete code support; speculative system-wide architecture not evidenced in this cluster.",
        "pattern" => "You are performing stage 2 of synthesis: pattern extraction.\n\nFocus exclusively on recurring or clearly intentional implementation approaches in this cluster. A good pattern unit names a reusable approach, explains how it works in this codebase, points at concrete evidence, and is more specific than a textbook label. Do NOT extract architecture boundaries or coding style conventions in this stage.\n\nBad units: one-off code with no reusable approach; generic pattern names with weak evidence; a restatement of one function body.",
        _ => "You are performing stage 3 of synthesis: convention extraction.\n\nFocus exclusively on coding conventions and local rules visible in this cluster: naming, error handling, return shapes, validation style, layering rules, logging and testing habits. A good convention unit states a clear, actionable rule grounded in actual code. Do NOT re-extract architecture or patterns in this stage.\n\nBad units: one accidental style choice; overly broad rules not evidenced in the code; a library default restated as a team convention.",
    };
    format!(
        "{}\n\n{}\n\n{}",
        SHARED_RULES,
        focus,
        output_schema(stage_types(stage))
    )
}

fn fence_for(content: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in content.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

pub fn render_cluster_context(ctx: &ClusterContext) -> String {
    let mut l: Vec<String> = vec![
        format!("# Cluster: {}", ctx.cluster.name),
        String::new(),
        ctx.cluster.description.clone(),
    ];
    if ctx.truncated {
        l.push(String::new());
        l.push(format!(
            "NOTE: this cluster is larger than the budget for one context, so it was trimmed ({} code block(s) not shown). Treat anything absent as UNKNOWN, never as a fact about the code.",
            ctx.dropped_blocks
        ));
    }
    l.push(String::new());
    l.push(format!("## Files ({})", ctx.cluster.files.len()));
    for f in &ctx.cluster.files {
        l.push(format!("- {}", f));
    }
    l.push(String::new());
    l.push("## Nodes".into());
    l.push(String::new());
    l.push("Ground every unit in one or more of these exact nodeIds. Prefer the PRIMARY symbols; SUPPORTING symbols are context only.".into());
    for imp in ["primary", "supporting"] {
        let group: Vec<&ContextNode> = ctx.nodes.iter().filter(|n| n.importance == imp).collect();
        l.push(String::new());
        l.push(format!(
            "### {} symbols ({})",
            if imp == "primary" {
                "Primary"
            } else {
                "Supporting"
            },
            group.len()
        ));
        if group.is_empty() {
            l.push("- (none)".into());
        }
        for n in group {
            l.push(format!("- nodeId: {}", n.node_id));
            l.push(format!("  kind: {}", n.kind));
            l.push(format!("  name: {}", n.name));
            if let Some(s) = &n.signature {
                l.push(format!("  signature: {}", s));
            }
            if let Some(d) = &n.docstring {
                l.push(format!("  doc: {}", d.replace('\n', " ")));
            }
            if !n.callers.is_empty() {
                l.push(format!("  callers: {}", n.callers.join(", ")));
            }
            if !n.callees.is_empty() {
                l.push(format!("  callees: {}", n.callees.join(", ")));
            }
        }
    }
    l.push(String::new());
    l.push("## Source".into());
    for imp in ["primary", "supporting"] {
        let group: Vec<&CodeBlock> = ctx
            .code_blocks
            .iter()
            .filter(|b| b.importance == imp)
            .collect();
        l.push(String::new());
        l.push(format!(
            "### {} evidence ({})",
            if imp == "primary" {
                "Primary"
            } else {
                "Supporting"
            },
            group.len()
        ));
        for b in group {
            let fence = fence_for(&b.content);
            l.push(String::new());
            l.push(format!(
                "#### {} (lines {}-{}) nodeId={}",
                b.file_path, b.start_line, b.end_line, b.node_id
            ));
            l.push(fence.clone());
            l.push(b.content.clone());
            l.push(fence);
        }
    }
    l.join("\n")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderedPrompt {
    pub stage: String,
    pub system: String,
    pub user: String,
    pub expected_types: Vec<String>,
}

pub fn render_stage_prompt(ctx: &ClusterContext, stage: &str) -> RenderedPrompt {
    let instruction = match stage {
        "architecture_component" => "Extract high-quality architecture and component units. Base every claim on concrete implementation evidence rather than on naming. If the cluster is too small or too unclear to support one, return an empty units array.",
        "pattern" => "Extract high-quality pattern units. Only emit a pattern where you can point at clear, repeated, or clearly intentional implementation evidence. If no strong patterns are visible, return an empty units array.",
        _ => "Extract high-quality convention units. Be strict: only emit conventions clearly demonstrated by the implementations above. If none are visible, return an empty units array.",
    };
    RenderedPrompt {
        stage: stage.to_string(),
        system: stage_system(stage),
        user: format!(
            "Here is the ClusterContext for this synthesis stage:\n\n{}\n\n---\n\n{}\n\nReturn only the JSON object: {{ \"stage\": \"{}\", \"cluster\": \"{}\", \"units\": [...] }}.",
            render_cluster_context(ctx),
            instruction,
            stage,
            ctx.cluster.name
        ),
        expected_types: stage_types(stage).iter().map(|s| s.to_string()).collect(),
    }
}

/// Largest source file whose text is quoted into synthesis context.
pub const MAX_SYNTHESIS_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

/// Read a graph-recorded source file for prompt context: only a relative path that resolves
/// (through symlinks) inside the project root, and only up to [`MAX_SYNTHESIS_SOURCE_BYTES`].
pub fn read_source_contained(project_root: &Path, file_path: &str) -> Option<String> {
    use std::io::Read;
    let rel = Path::new(file_path);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
    {
        return None;
    }
    let root = std::fs::canonicalize(project_root).ok()?;
    let real = std::fs::canonicalize(root.join(rel)).ok()?;
    if !real.starts_with(&root) {
        return None;
    }
    let meta = std::fs::metadata(&real).ok()?;
    if !meta.is_file() || meta.len() > MAX_SYNTHESIS_SOURCE_BYTES {
        return None;
    }
    let mut buf = Vec::new();
    std::fs::File::open(&real)
        .ok()?
        .take(MAX_SYNTHESIS_SOURCE_BYTES + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > MAX_SYNTHESIS_SOURCE_BYTES {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Number of files in the code graph, or None when the graph is not built.
pub fn graph_file_count(graph_db: &Path) -> Option<usize> {
    let g = SynthesisGraph::open(graph_db)?;
    let n = g.files().len();
    (n > 0).then_some(n)
}

/// Why there is nothing to synthesize, accurately: no graph, or a graph without clusters.
pub fn no_clusters_message(graph_db: &Path) -> String {
    match graph_file_count(graph_db) {
        None => "Knobyte found no clusters because the code graph is not built. Build it with `knobyte graph rebuild`, then run synthesis again.".to_string(),
        Some(n) => format!(
            "Knobyte found no clusters: the code graph ({} file(s)) has no group of related files large enough to synthesize (see `wiki.synthesis.minFiles`). There is nothing to synthesize yet.",
            n
        ),
    }
}

pub fn render_playbook(
    repo_root: &Path,
    scaffold_root: &Path,
    clusters: &[String],
    only: Option<&str>,
) -> String {
    render_playbook_with(
        repo_root,
        scaffold_root,
        clusters,
        only,
        &scaffold_root.join("graph.db"),
    )
}

/// [`render_playbook`] with the code graph location (used to explain an empty cluster list).
pub fn render_playbook_with(
    repo_root: &Path,
    scaffold_root: &Path,
    clusters: &[String],
    only: Option<&str>,
    graph_db: &Path,
) -> String {
    let scope = match only {
        Some(c) => format!("SCOPE: build ONLY the cluster \"{}\". Skip every other cluster.", c),
        None if clusters.is_empty() => format!(
            "{} Stop and report that.",
            no_clusters_message(graph_db)
        ),
        None => format!(
            "Knobyte deterministically found {} cluster(s):\n{}",
            clusters.len(),
            clusters.iter().map(|c| format!("  - {}", c)).collect::<Vec<_>>().join("\n")
        ),
    };
    format!(
        r#"You are building a knowledge wiki for an already-indexed repository, using Knobyte's CLI.

REPO:      {repo}
SCAFFOLD:  {scaffold}

The code graph is already built. Your job is to propose knowledge entities and the typed
relationships between them, through the commands below. Do not edit wiki files directly.

IMPORTANT BOUNDARIES
- Knobyte makes no model calls and holds no API keys. YOU run the model. Knobyte selects scope,
  renders prompts, validates what you return, and turns valid candidates into operation plans.
- Never invent knowledge the returned context does not support. Copy grounding nodeIds exactly
  as given; Knobyte re-resolves every one of them against the live code graph and drops any
  unit whose references it cannot resolve.
- Prefer a few excellent, well-grounded units over many weak ones. An empty stage is a valid
  outcome. Prefer sparse, sharp relationships; do not overuse related_to.
- Nothing you produce is written until it is reviewed. `knobyte wiki synthesis propose` plans
  by default and writes only with --apply.

{scope}

Every command below accepts --json.

============================================================
A. PER-CLUSTER SYNTHESIS
============================================================
For each cluster, one at a time:
  1. Run: knobyte wiki synthesis prepare --cluster <name> --stage architecture_component --json
     It returns the cluster context and the exact system and user prompts to send.
  2. Send those two strings to your own model. Save its JSON answer
     {{ "stage": "architecture_component", "cluster": "<name>", "units": [ ... ] }} to a file.
  3. Run: knobyte wiki synthesis propose <file> --json
     Knobyte validates every unit (confidence >= 0.7 promoted, >= 0.4 in_flight, lower rejected),
     re-resolves its grounding and prints the operation plan. Add --apply once it looks right.
  4. Repeat 1-3 for --stage pattern, then --stage convention.
  5. A stage that yields zero valid units is fine; continue with the next stage or cluster.

============================================================
B. GLOBAL CROSS-CUTTING PASS
============================================================
Only after stage A has been applied.
  1. Run: knobyte wiki synthesis prepare --stage global --json
     It returns deterministically grouped near-duplicate entities and the prompt.
  2. Judge each group: merge, promote_one, keep_separate, or drop_weak. When unsure, keep_separate.
  3. Save {{ "stage": "global", "actions": [ ... ] }} and run knobyte wiki synthesis propose <file> --json.

============================================================
C. RELATIONSHIP FORMATION
============================================================
Only after B has been applied.
  1. Run: knobyte wiki synthesis prepare --stage relationships --json
     It returns candidate pairs with structural evidence and an allowedTypes menu.
  2. For each candidate choose the MOST SPECIFIC allowed type and the correct direction, or skip.
     Confidence must be at least 0.80 (0.90 for related_to).
  3. Save {{ "stage": "relationships", "judgments": [ ... ] }} and run knobyte wiki synthesis propose <file> --json.

============================================================
FINAL REPORT
============================================================
Report concisely: clusters processed (and skipped, with reasons); units proposed and accepted by
stage and type; units Knobyte refused and why; global-pass actions; relationships created by
type and candidates skipped; anything that failed.

Begin with A. Proceed without asking for confirmation between clusters or stages; stop only on an
error you cannot resolve."#,
        repo = repo_root.display(),
        scaffold = scaffold_root.display(),
        scope = scope
    )
}

// ---------------------------------------------------------------------------
// Unit validation and proposal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedUnit {
    #[serde(rename = "type")]
    pub unit_type: String,
    pub title: String,
    pub summary: String,
    pub body: String,
    pub confidence: f64,
    pub node_ids: Vec<String>,
    pub status: String,
    pub stage: String,
    pub cluster: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rejected {
    pub item: Value,
    pub reasons: Vec<String>,
}

pub fn status_for_confidence(c: f64) -> Option<&'static str> {
    if c >= CONFIDENCE_PROMOTED {
        Some("promoted")
    } else if c >= CONFIDENCE_IN_FLIGHT {
        Some("in_flight")
    } else {
        None
    }
}

/// Strip a Markdown code fence around a JSON answer.
pub fn strip_code_fences(raw: &str) -> &str {
    let t = raw.trim();
    if t.starts_with("```") {
        if let (Some(nl), Some(end)) = (t.find('\n'), t.rfind("```")) {
            if end > nl {
                return t[nl + 1..end].trim();
            }
        }
    }
    t
}

/// The array under `key` in an agent response (or the response itself when it is an array).
pub fn extract_array(response: &Value, key: &str) -> Option<Vec<Value>> {
    match response {
        Value::Array(a) => Some(a.clone()),
        Value::Object(o) => o.get(key).and_then(|v| v.as_array()).cloned(),
        Value::String(s) => serde_json::from_str::<Value>(strip_code_fences(s))
            .ok()
            .and_then(|v| extract_array(&v, key)),
        _ => None,
    }
}

pub fn validate_units(
    raw: &[Value],
    stage: &str,
    ctx: &ClusterContext,
) -> (Vec<AcceptedUnit>, Vec<Rejected>) {
    let allowed = stage_types(stage);
    let valid_nodes = ctx.node_ids();
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    for item in raw {
        let mut reasons = Vec::new();
        let s = |k: &str| {
            item.get(k)
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
        };
        let unit_type = s("type").unwrap_or_default();
        let title = s("title").unwrap_or_default();
        let summary = s("summary").unwrap_or_default();
        let body = s("body").unwrap_or_default();
        let confidence = item.get("confidence").and_then(|v| v.as_f64());
        let node_ids: Vec<String> = item
            .get("grounding")
            .and_then(|g| g.get("nodeIds"))
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if !allowed.contains(&unit_type.as_str()) {
            reasons.push(format!(
                "type \"{}\" is not allowed for stage \"{}\" (allowed: {})",
                unit_type,
                stage,
                allowed.join(", ")
            ));
        }
        let tl = title.chars().count();
        if !(3..=120).contains(&tl) {
            reasons.push(format!("title must be 3-120 characters; got {}", tl));
        }
        let sl = summary.chars().count();
        if !(10..=500).contains(&sl) {
            reasons.push(format!("summary must be 10-500 characters; got {}", sl));
        }
        if body.chars().count() < 20 {
            reasons.push("body must be at least 20 characters".into());
        }
        if node_ids.is_empty() {
            reasons.push("a candidate must be grounded in at least one code-graph node id".into());
        }
        let missing: Vec<&String> = node_ids
            .iter()
            .filter(|n| !valid_nodes.contains(*n))
            .collect();
        if !missing.is_empty() {
            reasons.push(format!(
                "grounding names node ids that were not in the cluster context: {}",
                missing
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let status = match confidence {
            Some(c) if (0.0..=1.0).contains(&c) => match status_for_confidence(c) {
                Some(st) => Some(st),
                None => {
                    reasons.push(format!(
                        "confidence {} is below the {} floor, so the unit is not proposed",
                        c, CONFIDENCE_IN_FLIGHT
                    ));
                    None
                }
            },
            _ => {
                reasons.push("confidence must be a number between 0 and 1".into());
                None
            }
        };
        if !reasons.is_empty() {
            rejected.push(Rejected {
                item: item.clone(),
                reasons,
            });
            continue;
        }
        accepted.push(AcceptedUnit {
            unit_type,
            title,
            summary,
            body,
            confidence: confidence.unwrap_or(0.0),
            node_ids,
            status: status.unwrap_or("in_flight").to_string(),
            stage: stage.to_string(),
            cluster: ctx.cluster.name.clone(),
        });
    }
    (accepted, rejected)
}

/// Where a synthesized unit is filed: `(file, heading depth)`; None depth = new file-level entity.
pub fn placement(unit: &AcceptedUnit) -> Option<(String, Option<usize>)> {
    match unit.unit_type.as_str() {
        "architecture" | "component" => Some(("context/architecture.md".into(), Some(2))),
        "convention" => Some(("context/conventions.md".into(), Some(2))),
        "pattern" => Some((
            format!(
                "patterns/{}-{}.md",
                slugify(&unit.cluster).replace('_', "-"),
                slugify(&unit.title).replace('_', "-")
            ),
            None,
        )),
        _ => None,
    }
}

fn op_id(kind: &str, parts: &[&str]) -> String {
    format!("syn_{}_{}", kind, short_hash(&parts.join("|"), 32))
}

pub fn propose_units(
    units: &[AcceptedUnit],
    scope: &WikiScope,
    timestamp: &str,
) -> (Vec<Value>, Vec<Rejected>) {
    let mut ops = Vec::new();
    let mut rejected = Vec::new();
    let mut claimed: HashMap<String, String> = HashMap::new();
    for u in units {
        let Some((file, depth)) = placement(u) else {
            rejected.push(Rejected {
                item: serde_json::to_value(u).unwrap_or(Value::Null),
                reasons: vec![format!(
                    "no filing rule for entity type \"{}\"",
                    u.unit_type
                )],
            });
            continue;
        };
        if scope.is_read_only(&file) {
            rejected.push(Rejected {
                item: serde_json::to_value(u).unwrap_or(Value::Null),
                reasons: vec![format!("{} is read-only to the wiki", file)],
            });
            continue;
        }
        if depth.is_none() {
            if let Some(other) = claimed.get(&file) {
                rejected.push(Rejected {
                    item: serde_json::to_value(u).unwrap_or(Value::Null),
                    reasons: vec![format!(
                        "\"{}\" would be filed at {}, which \"{}\" already claims",
                        u.title, file, other
                    )],
                });
                continue;
            }
            if scope.scaffold_root.join(&file).exists() {
                rejected.push(Rejected {
                    item: serde_json::to_value(u).unwrap_or(Value::Null),
                    reasons: vec![format!("{} already exists", file)],
                });
                continue;
            }
            claimed.insert(file.clone(), u.title.clone());
        }
        let mut payload = json!({
            "file": file,
            "type": u.unit_type,
            "title": u.title,
            "summary": u.summary,
            "body": u.body,
            "status": u.status,
            "groundsTo": u.node_ids,
            "insertAt": { "at": "end-of-file" },
            "metadata": { "synthesis": { "confidence": u.confidence, "stage": u.stage, "cluster": u.cluster } },
        });
        if let Some(d) = depth {
            payload["headingDepth"] = json!(d);
        }
        let id = format!("syn_{}", short_hash(&payload.to_string(), 40));
        ops.push(json!({
            "opId": id,
            "type": "create-entry",
            "actor": { "kind": "agent", "id": "synthesis" },
            "timestamp": timestamp,
            "payload": payload,
        }));
    }
    (ops, rejected)
}

// ---------------------------------------------------------------------------
// Global pass
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WikiUnit {
    pub id: String,
    #[serde(rename = "type")]
    pub unit_type: String,
    pub title: String,
    pub summary: String,
    pub status: String,
    pub file: String,
    pub grounding: Vec<String>,
    pub revision: i64,
    pub content_hash: String,
    #[serde(skip)]
    pub body: String,
}

impl From<&WikiEntity> for WikiUnit {
    fn from(e: &WikiEntity) -> Self {
        Self {
            id: e.id.clone(),
            unit_type: e.entity_type.clone(),
            title: e.title.clone(),
            summary: e.summary.clone().unwrap_or_default(),
            status: e.status.clone(),
            file: e.file.clone(),
            grounding: e.grounds_to.clone(),
            revision: e.revision,
            content_hash: e.content_hash.clone(),
            body: e.body.clone(),
        }
    }
}

/// Active entities, first claimant per id.
pub fn active_units(scope: &WikiScope) -> Vec<WikiUnit> {
    let (files, _) = parse_corpus(scope);
    let mut seen = HashSet::new();
    files
        .iter()
        .flat_map(|f| f.entities.iter().map(|e| &e.entity))
        .filter(|e| e.is_active() && e.metadata_kind != "implicit" && seen.insert(e.id.clone()))
        .map(WikiUnit::from)
        .collect()
}

const STOPWORDS: [&str; 60] = [
    "the", "a", "an", "and", "or", "but", "of", "to", "in", "on", "for", "with", "is", "are", "be",
    "this", "that", "these", "those", "it", "its", "as", "by", "from", "at", "into", "via",
    "using", "used", "use", "uses", "how", "what", "when", "which", "each", "per", "we", "our",
    "they", "their", "can", "may", "must", "should", "will", "has", "have", "not", "all", "any",
    "one", "two", "if", "so", "do", "does", "than", "then", "also",
];

fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 3 && !STOPWORDS.contains(t))
        .map(|t| t.to_string())
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let shared = a.intersection(b).count() as f64;
    shared / ((a.len() + b.len()) as f64 - shared)
}

struct Features {
    title: HashSet<String>,
    summary: HashSet<String>,
    text: HashSet<String>,
    phrases: HashSet<String>,
    nodes: HashSet<String>,
    file: HashSet<String>,
}

fn featurize(u: &WikiUnit) -> Features {
    let title = tokenize(&u.title);
    let summary = tokenize(&u.summary);
    let both = tokenize(&format!("{} {}", u.title, u.summary));
    Features {
        title: title.iter().cloned().collect(),
        summary: summary.iter().cloned().collect(),
        text: summary.into_iter().chain(tokenize(&u.body)).collect(),
        phrases: both
            .windows(2)
            .map(|w| format!("{} {}", w[0], w[1]))
            .collect(),
        nodes: u.grounding.iter().cloned().collect(),
        file: HashSet::from([u.file.clone()]),
    }
}

fn pair_score(a: &Features, b: &Features) -> f64 {
    0.4 * jaccard(&a.title, &b.title)
        + 0.2 * jaccard(&a.summary, &b.summary)
        + 0.15 * jaccard(&a.nodes, &b.nodes)
        + 0.15 * jaccard(&a.text, &b.text)
        + 0.1 * jaccard(&a.file, &b.file)
}

fn is_edge(a: &Features, b: &Features, min: f64) -> bool {
    let t = jaccard(&a.title, &b.title);
    pair_score(a, b) >= min
        || t >= 0.6
        || jaccard(&a.nodes, &b.nodes) >= 0.5
        || (a.nodes.intersection(&b.nodes).count() >= 1 && t >= 0.25)
        || a.phrases.intersection(&b.phrases).count() >= 2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateGroup {
    pub group_id: String,
    #[serde(rename = "type")]
    pub group_type: String,
    pub units: Vec<WikiUnit>,
    pub reason: String,
}

pub fn find_candidate_groups(units: &[WikiUnit], max_groups: usize) -> Vec<CandidateGroup> {
    const MIN_SCORE: f64 = 0.26;
    const MAX_GROUP: usize = 10;
    let mut by_type: BTreeMap<&str, Vec<&WikiUnit>> = BTreeMap::new();
    for u in units {
        by_type.entry(u.unit_type.as_str()).or_default().push(u);
    }
    let mut scored: Vec<(f64, CandidateGroup)> = Vec::new();
    for (ty, list) in by_type {
        if list.len() < 2 {
            continue;
        }
        let feats: Vec<Features> = list.iter().map(|u| featurize(u)).collect();
        let mut parent: Vec<usize> = (0..list.len()).collect();
        fn find(p: &mut [usize], i: usize) -> usize {
            let mut r = i;
            while p[r] != r {
                r = p[r];
            }
            let mut c = i;
            while p[c] != r {
                let n = p[c];
                p[c] = r;
                c = n;
            }
            r
        }
        for i in 0..list.len() {
            for j in i + 1..list.len() {
                if is_edge(&feats[i], &feats[j], MIN_SCORE) {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    if a != b {
                        parent[a.max(b)] = a.min(b);
                    }
                }
            }
        }
        let mut comps: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for i in 0..list.len() {
            let r = find(&mut parent, i);
            comps.entry(r).or_default().push(i);
        }
        for idx in comps.values().filter(|c| c.len() >= 2) {
            let mut top: f64 = 0.0;
            for a in 0..idx.len() {
                for b in a + 1..idx.len() {
                    top = top.max(pair_score(&feats[idx[a]], &feats[idx[b]]));
                }
            }
            let mut members: Vec<&WikiUnit> = idx.iter().map(|i| list[*i]).collect();
            members.sort_by_key(|u| (if u.status == "promoted" { 0 } else { 1 }, u.id.clone()));
            members.truncate(MAX_GROUP);
            members.sort_by(|a, b| a.id.cmp(&b.id));
            let ids: Vec<&str> = members.iter().map(|u| u.id.as_str()).collect();
            scored.push((
                top,
                CandidateGroup {
                    group_id: format!("gp_{}_{}", ty, short_hash(&ids.join("|"), 12)),
                    group_type: ty.to_string(),
                    reason: format!(
                        "{} \"{}\" entities grouped (recall-first). Top pairwise similarity: {:.2}.",
                        members.len(),
                        ty,
                        top
                    ),
                    units: members.into_iter().cloned().collect(),
                },
            ));
        }
    }
    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.group_id.cmp(&b.1.group_id))
    });
    scored
        .into_iter()
        .take(max_groups.max(1))
        .map(|(_, g)| g)
        .collect()
}

pub fn render_global_prompt(groups: &[CandidateGroup]) -> RenderedPrompt {
    let system = r#"You are consolidating a knowledge wiki. Each group below holds entities of one type that a deterministic, recall-first pass flagged as possible near-duplicates.

For each group choose exactly one action:
- "merge": the entities describe one concept; give a canonicalUnit {type (the group type), title, summary, body, groundingNodeIds (a subset of the group's grounding)}. The originals are deprecated and the new entity supersedes them.
- "promote_one": one entity already says it best; give winnerId. The others are deprecated and superseded by it.
- "drop_weak": some entities are weak; give loserIds (never all of them). They are deprecated.
- "keep_separate": they are distinct. When unsure, keep_separate.

merge, promote_one and drop_weak require substantive reasoning (at least 10 characters).
Output ONLY JSON: { "stage": "global", "actions": [ { "groupId": string, "action": string, "canonicalUnit"?: {...}, "winnerId"?: string, "loserIds"?: string[], "reasoning"?: string } ] }"#;
    let mut user = String::from("Candidate groups:\n");
    for g in groups {
        user.push_str(&format!(
            "\n## {} ({})\n{}\n",
            g.group_id, g.group_type, g.reason
        ));
        for u in &g.units {
            user.push_str(&format!(
                "- id: {}\n  title: {}\n  status: {}\n  summary: {}\n  grounding: {}\n",
                u.id,
                u.title,
                u.status,
                u.summary,
                u.grounding.join(", ")
            ));
        }
    }
    if groups.is_empty() {
        user.push_str("\n(no groups: return { \"stage\": \"global\", \"actions\": [] })\n");
    }
    RenderedPrompt {
        stage: "global".into(),
        system: system.into(),
        user,
        expected_types: Vec::new(),
    }
}

fn deprecate_op(group: &str, u: &WikiUnit, ts: &str) -> Value {
    json!({
        "opId": op_id("dep", &[group, &u.id]),
        "type": "set-property",
        "entityId": u.id,
        "baseRevision": u.revision,
        "actor": { "kind": "agent", "id": "synthesis-global-pass" },
        "timestamp": ts,
        "payload": { "property": "status", "value": "deprecated" },
    })
}

pub fn plan_global_pass(
    raw: &[Value],
    groups: &[CandidateGroup],
    ts: &str,
) -> (Vec<Value>, Vec<Rejected>) {
    let by_id: HashMap<&str, &CandidateGroup> =
        groups.iter().map(|g| (g.group_id.as_str(), g)).collect();
    let mut ops = Vec::new();
    let mut creations = Vec::new();
    let mut rejected = Vec::new();
    let mut seen = HashSet::new();
    for a in raw {
        let s = |k: &str| a.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
        let gid = s("groupId").unwrap_or_default();
        let action = s("action").unwrap_or_default();
        let reasoning = s("reasoning").unwrap_or_default();
        let losers: Vec<String> = a
            .get("loserIds")
            .and_then(|v| v.as_array())
            .map(|x| {
                x.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let winner = s("winnerId");
        let mut reasons = Vec::new();
        let Some(g) = by_id.get(gid.as_str()) else {
            rejected.push(Rejected {
                item: a.clone(),
                reasons: vec![format!("unknown groupId: {}", gid)],
            });
            continue;
        };
        if !seen.insert(gid.clone()) {
            rejected.push(Rejected {
                item: a.clone(),
                reasons: vec![format!("duplicate action for groupId: {}", gid)],
            });
            continue;
        }
        let members: HashSet<&str> = g.units.iter().map(|u| u.id.as_str()).collect();
        if !["merge", "promote_one", "keep_separate", "drop_weak"].contains(&action.as_str()) {
            reasons.push(format!("unknown action \"{}\"", action));
        }
        if let Some(w) = &winner {
            if !members.contains(w.as_str()) {
                reasons.push(format!("winnerId {} is not a member of the group", w));
            }
            if losers.contains(w) {
                reasons.push("winnerId must not also appear in loserIds".into());
            }
        }
        let strays: Vec<&String> = losers
            .iter()
            .filter(|l| !members.contains(l.as_str()))
            .collect();
        if !strays.is_empty() {
            reasons.push(format!(
                "loserIds not in the group: {}",
                strays
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if action != "keep_separate" && reasoning.trim().chars().count() < 10 {
            reasons.push(format!("{} requires substantive reasoning", action));
        }
        let canonical = a.get("canonicalUnit").cloned();
        match action.as_str() {
            "merge" => match &canonical {
                None => reasons.push("merge requires a canonicalUnit".into()),
                Some(c) => {
                    if c.get("type").and_then(|v| v.as_str()) != Some(g.group_type.as_str()) {
                        reasons.push(format!(
                            "canonicalUnit.type must equal the group type \"{}\"",
                            g.group_type
                        ));
                    }
                    let nodes: Vec<String> = c
                        .get("groundingNodeIds")
                        .and_then(|v| v.as_array())
                        .map(|x| {
                            x.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let union: HashSet<&str> = g
                        .units
                        .iter()
                        .flat_map(|u| u.grounding.iter().map(|s| s.as_str()))
                        .collect();
                    if nodes.is_empty() {
                        reasons.push(
                            "a merged canonicalUnit must keep at least one grounding node".into(),
                        );
                    }
                    let invented: Vec<&String> = nodes
                        .iter()
                        .filter(|n| !union.contains(n.as_str()))
                        .collect();
                    if !invented.is_empty() {
                        reasons.push(format!(
                            "canonicalUnit grounding introduces node ids not in the group: {}",
                            invented
                                .iter()
                                .map(|s| s.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    for k in ["title", "summary", "body"] {
                        if c.get(k)
                            .and_then(|v| v.as_str())
                            .map(|s| s.trim().is_empty())
                            .unwrap_or(true)
                        {
                            reasons.push(format!("canonicalUnit.{} is required", k));
                        }
                    }
                }
            },
            "promote_one" if winner.is_none() => {
                reasons.push("promote_one requires a winnerId".into())
            }
            "drop_weak" => {
                if losers.is_empty() {
                    reasons.push("drop_weak requires at least one loserId".into());
                }
                if losers.len() >= g.units.len() {
                    reasons.push("drop_weak must leave at least one unit in the group".into());
                }
            }
            _ => {}
        }
        if !reasons.is_empty() {
            rejected.push(Rejected {
                item: a.clone(),
                reasons,
            });
            continue;
        }
        match action.as_str() {
            "drop_weak" => {
                for l in &losers {
                    if let Some(u) = g.units.iter().find(|u| &u.id == l) {
                        ops.push(deprecate_op(&gid, u, ts));
                    }
                }
            }
            "promote_one" => {
                let w = g
                    .units
                    .iter()
                    .find(|u| Some(&u.id) == winner.as_ref())
                    .expect("validated");
                for l in g.units.iter().filter(|u| u.id != w.id) {
                    ops.push(json!({
                        "opId": op_id("sup", &[&gid, &w.id, &l.id]),
                        "type": "add-relation",
                        "entityId": w.id,
                        "actor": { "kind": "agent", "id": "synthesis-global-pass" },
                        "timestamp": ts,
                        "payload": { "relation": { "type": "supersedes", "target": l.id, "note": reasoning } },
                    }));
                    ops.push(deprecate_op(&gid, l, ts));
                }
            }
            "merge" => {
                let c = canonical.expect("validated");
                let anchor = g
                    .units
                    .iter()
                    .find(|u| Some(&u.id) == winner.as_ref())
                    .unwrap_or(&g.units[0]);
                let others: Vec<&WikiUnit> = g.units.iter().filter(|u| u.id != anchor.id).collect();
                for o in &others {
                    ops.push(deprecate_op(&gid, o, ts));
                }
                creations.push(json!({
                    "opId": op_id("mrg", &[&gid]),
                    "type": "supersede-entry",
                    "entityId": anchor.id,
                    "actor": { "kind": "agent", "id": "synthesis-global-pass" },
                    "timestamp": ts,
                    "payload": {
                        "note": reasoning,
                        "replacement": {
                            "file": anchor.file,
                            "insertAt": { "at": "end-of-file" },
                            "headingDepth": 2,
                            "type": g.group_type,
                            "title": c["title"],
                            "summary": c["summary"],
                            "body": c["body"],
                            "status": "in_flight",
                            "relations": others.iter().map(|o| json!({ "type": "supersedes", "target": o.id })).collect::<Vec<_>>(),
                            "groundsTo": c["groundingNodeIds"],
                            "metadata": { "synthesis": { "stage": "global", "groupId": gid, "mergedFrom": g.units.iter().map(|u| u.id.clone()).collect::<Vec<_>>() } },
                        }
                    },
                }));
            }
            _ => {}
        }
    }
    ops.extend(creations);
    (ops, rejected)
}

// ---------------------------------------------------------------------------
// Relationships
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelationshipCandidate {
    pub candidate_id: String,
    pub source: WikiUnit,
    pub target: WikiUnit,
    pub evidence: String,
    pub score: f64,
    pub allowed_types: Vec<String>,
}

fn edge_weight(kind: &str) -> f64 {
    match kind {
        "implements" => 1.0,
        "extends" => 0.9,
        "overrides" => 0.7,
        "calls" | "instantiates" => 0.6,
        "decorates" => 0.5,
        "references" | "type_of" | "returns" => 0.4,
        "imports" => 0.3,
        _ => 0.2,
    }
}

pub fn allowed_types_for(s: &str, t: &str, abstraction: bool, dependency: bool) -> Vec<String> {
    let pair = |a: &str, b: &str| (s == a && t == b) || (s == b && t == a);
    let mut allowed: HashSet<&str> = HashSet::new();
    if pair("component", "pattern") || pair("architecture", "pattern") {
        allowed.extend(["implements", "refines"]);
    }
    if pair("component", "component") {
        allowed.insert("depends_on");
    }
    if pair("component", "architecture") {
        allowed.extend(["implements", "refines", "depends_on"]);
    }
    if ["component", "pattern", "architecture", "decision"]
        .iter()
        .any(|x| pair("convention", x))
    {
        allowed.insert("constrained_by");
    }
    if s == "decision" || t == "decision" {
        allowed.extend(["depends_on", "supersedes"]);
    }
    if abstraction {
        allowed.extend(["implements", "refines"]);
    }
    if dependency {
        allowed.insert("depends_on");
    }
    if s == t {
        allowed.extend(["refines", "supersedes"]);
    }
    allowed.insert("related_to");
    RELATIONSHIP_TYPES
        .iter()
        .filter(|x| allowed.contains(**x))
        .map(|x| x.to_string())
        .collect()
}

pub fn find_relationship_candidates(
    units: &[WikiUnit],
    graph: Option<&SynthesisGraph>,
    max_candidates: usize,
    max_per_unit: usize,
) -> Vec<RelationshipCandidate> {
    // Map grounding references to node ids, and node ids to units.
    let mut unit_nodes: HashMap<&str, Vec<String>> = HashMap::new();
    let mut by_node: HashMap<String, Vec<&str>> = HashMap::new();
    for u in units {
        let mut ids = Vec::new();
        for r in &u.grounding {
            let id = graph
                .and_then(|g| g.resolve(r))
                .map(|n| n.id)
                .unwrap_or_else(|| r.clone());
            ids.push(id.clone());
            let v = by_node.entry(id).or_default();
            if !v.contains(&u.id.as_str()) {
                v.push(u.id.as_str());
            }
        }
        unit_nodes.insert(u.id.as_str(), ids);
    }
    #[derive(Default)]
    struct Acc {
        shared: HashSet<String>,
        edges: Vec<(String, String, f64)>, // (from unit, kind, weight)
    }
    let mut pairs: BTreeMap<(String, String), Acc> = BTreeMap::new();
    let key = |a: &str, b: &str| {
        if a < b {
            (a.to_string(), b.to_string())
        } else {
            (b.to_string(), a.to_string())
        }
    };
    for (node, ids) in &by_node {
        for i in 0..ids.len() {
            for j in i + 1..ids.len() {
                pairs
                    .entry(key(ids[i], ids[j]))
                    .or_default()
                    .shared
                    .insert(node.clone());
            }
        }
    }
    if let Some(g) = graph {
        for u in units {
            for from in unit_nodes.get(u.id.as_str()).into_iter().flatten() {
                for (to, kind) in g.neighbours(from, true) {
                    if kind == "contains" || kind == "exports" {
                        continue;
                    }
                    for target in by_node.get(&to).into_iter().flatten() {
                        if *target == u.id {
                            continue;
                        }
                        pairs.entry(key(&u.id, target)).or_default().edges.push((
                            u.id.clone(),
                            kind.clone(),
                            edge_weight(&kind),
                        ));
                    }
                }
            }
        }
    }
    let by_id: HashMap<&str, &WikiUnit> = units.iter().map(|u| (u.id.as_str(), u)).collect();
    let mut scored: Vec<RelationshipCandidate> = Vec::new();
    for ((a, b), acc) in pairs {
        let shared = if acc.shared.is_empty() {
            0.0
        } else {
            (0.5 + 0.1 * (acc.shared.len() as f64 - 1.0)).min(1.0)
        };
        let edge: f64 = acc.edges.iter().map(|e| e.2).sum::<f64>().min(1.0);
        let score = (0.75 * edge + 0.4 * shared).min(1.0);
        if score < 0.3 {
            continue;
        }
        let a_to_b: f64 = acc.edges.iter().filter(|e| e.0 == a).map(|e| e.2).sum();
        let b_to_a: f64 = acc.edges.iter().filter(|e| e.0 == b).map(|e| e.2).sum();
        let (s, t) = if b_to_a > a_to_b {
            (b.clone(), a.clone())
        } else {
            (a.clone(), b.clone())
        };
        let (Some(su), Some(tu)) = (by_id.get(s.as_str()), by_id.get(t.as_str())) else {
            continue;
        };
        let abstraction = acc
            .edges
            .iter()
            .any(|e| ["implements", "extends", "overrides"].contains(&e.1.as_str()));
        let dependency = acc.edges.iter().any(|e| {
            [
                "calls",
                "references",
                "instantiates",
                "type_of",
                "returns",
                "decorates",
                "imports",
            ]
            .contains(&e.1.as_str())
        });
        let mut parts = Vec::new();
        if !acc.edges.is_empty() {
            parts.push(format!(
                "{} code-graph edge(s) link {} \"{}\" to {} \"{}\".",
                acc.edges.len(),
                su.unit_type,
                su.title,
                tu.unit_type,
                tu.title
            ));
        }
        if !acc.shared.is_empty() {
            let mut sh: Vec<&String> = acc.shared.iter().collect();
            sh.sort();
            parts.push(format!(
                "Shared grounding: {}.",
                sh.iter()
                    .take(5)
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        parts.push(format!("Structural score: {:.2}.", score));
        scored.push(RelationshipCandidate {
            candidate_id: format!("rel_{}", short_hash(&format!("{}|{}", a, b), 12)),
            source: (*su).clone(),
            target: (*tu).clone(),
            evidence: parts.join(" "),
            score,
            allowed_types: allowed_types_for(&su.unit_type, &tu.unit_type, abstraction, dependency),
        });
    }
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.candidate_id.cmp(&b.candidate_id))
    });
    let mut per: HashMap<String, usize> = HashMap::new();
    let mut kept = Vec::new();
    for c in scored {
        if kept.len() >= max_candidates {
            break;
        }
        let (s, t) = (c.source.id.clone(), c.target.id.clone());
        if per.get(&s).copied().unwrap_or(0) >= max_per_unit
            || per.get(&t).copied().unwrap_or(0) >= max_per_unit
        {
            continue;
        }
        *per.entry(s).or_default() += 1;
        *per.entry(t).or_default() += 1;
        kept.push(c);
    }
    kept
}

pub fn render_relationships_prompt(candidates: &[RelationshipCandidate]) -> RenderedPrompt {
    let system = r#"You are forming typed relationships in a knowledge wiki. Each candidate pair below has structural evidence from the code graph and an allowedTypes menu.

For each candidate either create ONE relationship or skip it:
- Choose the MOST SPECIFIC valid type from that candidate's allowedTypes, and the correct direction (sourceId -> targetId, both members of the candidate).
- implements: source realizes target. depends_on: source needs target. refines: source specializes target. supersedes: source replaces target. constrained_by: source must follow target. related_to: only when nothing more specific holds.
- Confidence must be at least 0.80 (0.90 for related_to). Prefer skipping to asserting a weak edge.

Output ONLY JSON: { "stage": "relationships", "judgments": [ { "candidateId": string, "action": "create" | "skip", "type"?: string, "sourceId"?: string, "targetId"?: string, "confidence"?: number, "evidence"?: string, "reasoning"?: string } ] }"#;
    let mut user = String::from("Candidates:\n");
    for c in candidates {
        user.push_str(&format!(
            "\n## {}\nsource: {} ({}) \"{}\" - {}\ntarget: {} ({}) \"{}\" - {}\nevidence: {}\nallowedTypes: {}\n",
            c.candidate_id,
            c.source.id,
            c.source.unit_type,
            c.source.title,
            c.source.summary,
            c.target.id,
            c.target.unit_type,
            c.target.title,
            c.target.summary,
            c.evidence,
            c.allowed_types.join(", ")
        ));
    }
    if candidates.is_empty() {
        user.push_str(
            "\n(no candidates: return { \"stage\": \"relationships\", \"judgments\": [] })\n",
        );
    }
    RenderedPrompt {
        stage: "relationships".into(),
        system: system.into(),
        user,
        expected_types: Vec::new(),
    }
}

pub fn plan_relationships(
    raw: &[Value],
    candidates: &[RelationshipCandidate],
    existing: &HashSet<(String, String, String)>,
    ts: &str,
) -> (Vec<Value>, Vec<Rejected>, usize) {
    let by_id: HashMap<&str, &RelationshipCandidate> = candidates
        .iter()
        .map(|c| (c.candidate_id.as_str(), c))
        .collect();
    let mut ops = Vec::new();
    let mut rejected = Vec::new();
    let mut skipped = 0;
    let mut seen = HashSet::new();
    for j in raw {
        let s = |k: &str| j.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
        let cid = s("candidateId").unwrap_or_default();
        let Some(c) = by_id.get(cid.as_str()) else {
            rejected.push(Rejected {
                item: j.clone(),
                reasons: vec![format!("unknown candidateId: {}", cid)],
            });
            continue;
        };
        if !seen.insert(cid.clone()) {
            rejected.push(Rejected {
                item: j.clone(),
                reasons: vec![format!("duplicate judgment for candidateId: {}", cid)],
            });
            continue;
        }
        match s("action").as_deref() {
            Some("skip") => {
                skipped += 1;
                continue;
            }
            Some("create") => {}
            other => {
                rejected.push(Rejected {
                    item: j.clone(),
                    reasons: vec![format!("action must be create or skip, got {:?}", other)],
                });
                continue;
            }
        }
        let mut reasons = Vec::new();
        let ty = s("type").unwrap_or_default();
        if !c.allowed_types.contains(&ty) {
            reasons.push(format!(
                "type \"{}\" is not in allowedTypes [{}]",
                ty,
                c.allowed_types.join(", ")
            ));
        }
        let members = [c.source.id.as_str(), c.target.id.as_str()];
        let (src, tgt) = (
            s("sourceId").unwrap_or_default(),
            s("targetId").unwrap_or_default(),
        );
        if !members.contains(&src.as_str()) || !members.contains(&tgt.as_str()) || src == tgt {
            reasons.push("sourceId and targetId must be the two members of the candidate".into());
        }
        let threshold = if ty == "related_to" { 0.9 } else { 0.8 };
        match j.get("confidence").and_then(|v| v.as_f64()) {
            None => reasons.push("create requires a confidence".into()),
            Some(conf) if conf < threshold => reasons.push(format!(
                "confidence {} is below the {} threshold {}",
                conf, ty, threshold
            )),
            _ => {}
        }
        if existing.contains(&(src.clone(), tgt.clone(), ty.clone())) {
            reasons.push(format!("{} already declares {} -> {}", src, ty, tgt));
        }
        if !reasons.is_empty() {
            rejected.push(Rejected {
                item: j.clone(),
                reasons,
            });
            continue;
        }
        let note = s("evidence").or_else(|| s("reasoning"));
        let mut rel = json!({ "type": ty, "target": tgt });
        if let Some(n) = note {
            rel["note"] = json!(n);
        }
        ops.push(json!({
            "opId": op_id("rel", &[&cid, &src, &tgt, &ty]),
            "type": "add-relation",
            "entityId": src,
            "actor": { "kind": "agent", "id": "synthesis-relationships" },
            "timestamp": ts,
            "payload": { "relation": rel },
        }));
    }
    (ops, rejected, skipped)
}

// ---------------------------------------------------------------------------
// Service entry points
// ---------------------------------------------------------------------------

pub struct SynthesisEnv<'a> {
    pub scope: &'a WikiScope,
    pub project_root: &'a Path,
    pub graph_db: &'a Path,
}

pub fn clusters(env: &SynthesisEnv) -> Vec<Cluster> {
    match SynthesisGraph::open(env.graph_db) {
        Some(g) => find_clusters(&g, env.scope.config.synthesis.min_files),
        None => Vec::new(),
    }
}

/// `prepare`: context and prompts for one stage.
#[allow(clippy::result_large_err)]
pub fn prepare(
    env: &SynthesisEnv,
    stage: &str,
    cluster: Option<&str>,
) -> Result<Value, WikiDiagnostic> {
    let cfg = &env.scope.config.synthesis;
    let graph = SynthesisGraph::open(env.graph_db);
    match stage {
        s if SYNTHESIS_STAGES.contains(&s) => {
            let Some(g) = graph else {
                return Err(diag(
                    "GROUNDINGS_UNCHECKED",
                    "The code graph is not built; run `knobyte graph rebuild` before synthesis",
                    "graph.db",
                ));
            };
            let all = find_clusters(&g, cfg.min_files);
            let Some(name) = cluster else {
                return Err(diag(
                    "INVALID_REQUEST",
                    format!(
                        "--cluster is required for stage {}. Clusters: {}",
                        s,
                        all.iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    "",
                ));
            };
            let Some(c) = all.iter().find(|c| c.name == name) else {
                return Err(diag(
                    "ENTITY_NOT_FOUND",
                    format!("No cluster named \"{}\"", name),
                    "",
                ));
            };
            let ctx = build_cluster_context(&g, env.project_root, c, cfg);
            let prompt = render_stage_prompt(&ctx, s);
            Ok(json!({ "stage": s, "cluster": c.name, "context": ctx, "prompt": prompt }))
        }
        "global" => {
            let groups = find_candidate_groups(&active_units(env.scope), cfg.max_groups);
            Ok(
                json!({ "stage": "global", "groups": groups, "prompt": render_global_prompt(&groups) }),
            )
        }
        "relationships" => {
            let candidates = find_relationship_candidates(
                &active_units(env.scope),
                graph.as_ref(),
                cfg.max_candidates,
                cfg.max_per_unit,
            );
            Ok(
                json!({ "stage": "relationships", "candidates": candidates, "prompt": render_relationships_prompt(&candidates) }),
            )
        }
        other => Err(diag(
            "INVALID_REQUEST",
            format!(
                "\"{}\" is not a synthesis stage ({})",
                other,
                ALL_STAGES.join(", ")
            ),
            "",
        )),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProposeResult {
    pub stage: String,
    pub accepted: usize,
    pub rejected: Vec<Rejected>,
    pub skipped: usize,
    pub operations: Vec<Value>,
}

/// `propose`: validate an agent response into operation envelopes (not applied here).
#[allow(clippy::result_large_err)]
pub fn propose(
    env: &SynthesisEnv,
    response: &Value,
    stage_hint: Option<&str>,
) -> Result<ProposeResult, WikiDiagnostic> {
    let response = match response {
        Value::String(s) => serde_json::from_str::<Value>(strip_code_fences(s)).map_err(|e| {
            diag(
                "INVALID_AGENT_RESPONSE",
                format!("The response is not JSON: {}", e),
                "",
            )
        })?,
        v => v.clone(),
    };
    let stage = response
        .get("stage")
        .and_then(|v| v.as_str())
        .or(stage_hint)
        .ok_or_else(|| {
            diag(
                "INVALID_AGENT_RESPONSE",
                "The response names no `stage` (pass --stage)",
                "",
            )
        })?
        .to_string();
    let cfg = &env.scope.config.synthesis;
    let ts = chrono::Utc::now().to_rfc3339();
    let graph = SynthesisGraph::open(env.graph_db);
    if SYNTHESIS_STAGES.contains(&stage.as_str()) {
        let units = extract_array(&response, "units")
            .ok_or_else(|| diag("INVALID_AGENT_RESPONSE", "Expected a `units` array", ""))?;
        let cluster = response
            .get("cluster")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                diag(
                    "INVALID_AGENT_RESPONSE",
                    "The response names no `cluster`",
                    "",
                )
            })?;
        let Some(g) = graph else {
            return Err(diag(
                "GROUNDING_UNVERIFIED",
                "No code graph is available, and a grounding may not be written unverified",
                "graph.db",
            ));
        };
        let all = find_clusters(&g, cfg.min_files);
        let c = all.iter().find(|c| c.name == cluster).ok_or_else(|| {
            diag(
                "ENTITY_NOT_FOUND",
                format!("No cluster named \"{}\"", cluster),
                "",
            )
        })?;
        let ctx = build_cluster_context(&g, env.project_root, c, cfg);
        let (accepted, mut rejected) = validate_units(&units, &stage, &ctx);
        let (ops, more) = propose_units(&accepted, env.scope, &ts);
        rejected.extend(more);
        return Ok(ProposeResult {
            stage,
            accepted: ops.len(),
            rejected,
            skipped: 0,
            operations: ops,
        });
    }
    match stage.as_str() {
        "global" => {
            let actions = extract_array(&response, "actions")
                .ok_or_else(|| diag("INVALID_AGENT_RESPONSE", "Expected an `actions` array", ""))?;
            let groups = find_candidate_groups(&active_units(env.scope), cfg.max_groups);
            let (ops, rejected) = plan_global_pass(&actions, &groups, &ts);
            let accepted = actions.len() - rejected.len();
            Ok(ProposeResult {
                stage,
                accepted,
                rejected,
                skipped: 0,
                operations: ops,
            })
        }
        "relationships" => {
            let judgments = extract_array(&response, "judgments").ok_or_else(|| {
                diag("INVALID_AGENT_RESPONSE", "Expected a `judgments` array", "")
            })?;
            let units = active_units(env.scope);
            let candidates = find_relationship_candidates(
                &units,
                graph.as_ref(),
                cfg.max_candidates,
                cfg.max_per_unit,
            );
            let (files, _) = parse_corpus(env.scope);
            let existing: HashSet<(String, String, String)> = files
                .iter()
                .flat_map(|f| f.entities.iter())
                .flat_map(|e| {
                    e.entity.relations.iter().map(move |r| {
                        (e.entity.id.clone(), r.target_id.clone(), r.rel_type.clone())
                    })
                })
                .collect();
            let (ops, rejected, skipped) =
                plan_relationships(&judgments, &candidates, &existing, &ts);
            Ok(ProposeResult {
                stage,
                accepted: ops.len(),
                rejected,
                skipped,
                operations: ops,
            })
        }
        other => Err(diag(
            "INVALID_AGENT_RESPONSE",
            format!("\"{}\" is not a synthesis stage", other),
            "",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_keys() {
        assert_eq!(
            resolve_cluster_key("src/wiki/index.rs"),
            Some(("wiki".into(), "src".into()))
        );
        assert_eq!(resolve_cluster_key("src/main.rs"), None);
        assert_eq!(resolve_cluster_key("tests/a.rs"), None);
        assert_eq!(
            resolve_cluster_key("packages/api/src/auth/x.ts"),
            Some(("auth".into(), "packages/api/src".into()))
        );
    }

    #[test]
    fn confidence_gate() {
        assert_eq!(status_for_confidence(0.7), Some("promoted"));
        assert_eq!(status_for_confidence(0.5), Some("in_flight"));
        assert_eq!(status_for_confidence(0.39), None);
    }
}
