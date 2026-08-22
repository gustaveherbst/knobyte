//! Grounding references: parsing, resolution against the code graph, and baselines.
//!
//! Documentation grounds claims to code with *readable references* of the form
//! `kind:path:qualified_name`, e.g. `function:src/auth.rs:validate_token` or
//! `method:src/auth.rs:Auth::validate`. Older scaffolds may still contain the graph's internal
//! hashed ids (`function:3f2a...` - 32 hex chars); those are accepted as well.
//!
//! Baselines live in `_knobyte_grounded_source` (preserved across `graph rebuild`):
//!
//! | column         | meaning                                                         |
//! |----------------|-----------------------------------------------------------------|
//! | `subject_kind` | `'doc'` (legacy rows written by older versions use `'scaffold'`) |
//! | `subject_id`   | scaffold-relative path of the markdown document                  |
//! | `node_id`      | the grounding reference exactly as written in the document       |
//! | `source`       | source text of the grounded symbol when the baseline was taken   |
//! | `body_hash`    | whitespace-normalised body hash at baseline time                 |
//! | `fingerprint`  | graph node id the reference resolved to at baseline time         |

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::graph::engine::{map_node_row, NODE_COLUMNS};
use crate::graph::fingerprint::{compute_body_hash, MinHash};
use crate::graph::models::Node;
use crate::wiki::parser::parse_markdown_entity;

/// Subject kind used for per-document grounding baselines.
pub const DOC_SUBJECT_KIND: &str = "doc";

/// A parsed readable grounding reference (`kind:path:qualified_name`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroundingRef {
    pub kind: String,
    pub file_path: String,
    pub qualified_name: String,
}

impl GroundingRef {
    /// Bare symbol name (last segment of the qualified name).
    pub fn symbol_name(&self) -> &str {
        last_segment(&self.qualified_name)
    }
}

/// Classification of a raw grounding string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedRef {
    /// `kind:path:qualified_name`
    Readable(GroundingRef),
    /// Legacy hashed graph id, `kind:<32 hex chars>`.
    LegacyId(String),
    /// Anything else (tried verbatim as a node id).
    Other(String),
}

/// Result of resolving a grounding reference against the code graph.
#[derive(Debug, Clone)]
pub enum RefResolution {
    Resolved(Box<Node>),
    Ambiguous(Vec<Node>),
    Missing,
}

impl RefResolution {
    pub fn node(&self) -> Option<&Node> {
        match self {
            RefResolution::Resolved(n) => Some(n),
            _ => None,
        }
    }
}

/// Stored grounding baseline for one (document, reference) pair.
#[derive(Debug, Clone)]
pub struct Baseline {
    pub body_hash: String,
    pub source: String,
    /// Graph node id the reference resolved to when the baseline was taken.
    pub node_id: String,
}

/// All grounding references of one scaffold document.
#[derive(Debug, Clone)]
pub struct DocGroundings {
    /// Scaffold-relative path of the markdown document.
    pub doc: String,
    pub refs: Vec<String>,
}

pub fn last_segment(qualified: &str) -> &str {
    let after_colons = qualified.rsplit("::").next().unwrap_or(qualified);
    after_colons.rsplit('.').next().unwrap_or(after_colons)
}

fn is_legacy_hash(s: &str) -> bool {
    s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Parse a grounding string into its readable form or a legacy id.
pub fn parse_grounding_ref(raw: &str) -> ParsedRef {
    let raw = raw.trim();
    let (kind, rest) = match raw.split_once(':') {
        Some((k, r)) if !k.is_empty() && !r.is_empty() => (k, r),
        _ => return ParsedRef::Other(raw.to_string()),
    };
    if is_legacy_hash(rest) {
        return ParsedRef::LegacyId(raw.to_string());
    }
    match rest.split_once(':') {
        Some((path, qualified)) if !path.is_empty() && !qualified.is_empty() => {
            ParsedRef::Readable(GroundingRef {
                kind: kind.to_string(),
                file_path: path.trim_start_matches("./").to_string(),
                qualified_name: qualified.to_string(),
            })
        }
        _ if kind == "file" => ParsedRef::Readable(GroundingRef {
            kind: "file".to_string(),
            file_path: rest.trim_start_matches("./").to_string(),
            qualified_name: rest.trim_start_matches("./").to_string(),
        }),
        _ => ParsedRef::Other(raw.to_string()),
    }
}

/// Build a readable reference `kind:path:qualified_name`.
pub fn readable_ref(kind: &str, file_path: &str, qualified_name: &str) -> String {
    format!("{}:{}:{}", kind, file_path, qualified_name)
}

/// Readable reference for a graph node.
pub fn readable_ref_for(node: &Node) -> String {
    readable_ref(&node.kind, &node.file_path, &node.qualified_name)
}

/// Readable reference for `node` that keeps the kind word of `old_ref` when it is equivalent
/// (so `function:old.rs:f` relocates to `function:new.rs:f` even if the node is a `method`).
pub fn relocated_ref(old_ref: &str, node: &Node) -> String {
    let kind = match parse_grounding_ref(old_ref) {
        ParsedRef::Readable(r) if kinds_equivalent(&r.kind, &node.kind) => r.kind,
        _ => node.kind.clone(),
    };
    readable_ref(&kind, &node.file_path, &node.qualified_name)
}

fn kind_class(kind: &str) -> &str {
    match kind {
        "function" | "method" | "fn" | "def" => "callable",
        "struct" | "class" => "type",
        "type" | "type_alias" => "type_alias",
        other => other,
    }
}

/// Whether two node kinds should be treated as the same grounding kind.
/// `function` matches `method` (Rust/TS/Python methods), `struct` matches `class`.
pub fn kinds_equivalent(a: &str, b: &str) -> bool {
    a == b || kind_class(a) == kind_class(b)
}

fn normalize_qualified(q: &str) -> String {
    q.replace("::", ".")
}

/// Resolve a grounding reference (readable or legacy hashed id) to a graph node.
pub fn resolve_grounding_ref(conn: &Connection, raw: &str) -> rusqlite::Result<RefResolution> {
    let raw = raw.trim();
    let by_id: Option<Node> = conn
        .query_row(
            &format!("SELECT {} FROM nodes WHERE id = ?1", NODE_COLUMNS),
            params![raw],
            map_node_row,
        )
        .optional()?;
    if let Some(node) = by_id {
        return Ok(RefResolution::Resolved(Box::new(node)));
    }

    let r = match parse_grounding_ref(raw) {
        ParsedRef::Readable(r) => r,
        _ => return Ok(RefResolution::Missing),
    };

    let name = r.symbol_name().to_string();
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM nodes WHERE file_path = ?1 AND (qualified_name = ?2 OR name = ?3)",
        NODE_COLUMNS
    ))?;
    let rows = stmt.query_map(params![r.file_path, r.qualified_name, name], map_node_row)?;
    let wanted_q = normalize_qualified(&r.qualified_name);
    let mut candidates: Vec<Node> = Vec::new();
    for row in rows {
        let n = row?;
        if !kinds_equivalent(&r.kind, &n.kind) {
            continue;
        }
        // Either the full qualified name matches, or the ref gave a bare name.
        let q_matches = normalize_qualified(&n.qualified_name) == wanted_q;
        let bare_ref = !r.qualified_name.contains("::") && !r.qualified_name.contains('.');
        if q_matches || (bare_ref && n.name == r.qualified_name) {
            candidates.push(n);
        }
    }

    Ok(pick_candidate(candidates, &r))
}

fn pick_candidate(mut candidates: Vec<Node>, r: &GroundingRef) -> RefResolution {
    if candidates.len() > 1 {
        let wanted_q = normalize_qualified(&r.qualified_name);
        let exact_q: Vec<Node> = candidates
            .iter()
            .filter(|n| normalize_qualified(&n.qualified_name) == wanted_q)
            .cloned()
            .collect();
        if !exact_q.is_empty() {
            candidates = exact_q;
        }
    }
    if candidates.len() > 1 {
        let exact_kind: Vec<Node> = candidates
            .iter()
            .filter(|n| n.kind == r.kind)
            .cloned()
            .collect();
        if !exact_kind.is_empty() {
            candidates = exact_kind;
        }
    }
    if candidates.len() > 1 {
        // Prefer free functions over methods for a bare `function:path:name` reference.
        let top: Vec<Node> = candidates
            .iter()
            .filter(|n| n.container_id.is_none())
            .cloned()
            .collect();
        if top.len() == 1 {
            candidates = top;
        }
    }
    match candidates.len() {
        0 => RefResolution::Missing,
        1 => RefResolution::Resolved(Box::new(candidates.remove(0))),
        _ => RefResolution::Ambiguous(candidates),
    }
}

/// Dependency, build and VCS directories skipped (at any depth) when the scaffold root is the
/// project root. `node_modules` and hidden directories are always skipped.
pub const SCAFFOLD_SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "dist",
    "build",
    "out",
    "coverage",
    "__pycache__",
    "venv",
];

const PROJECT_MARKERS: &[&str] = &[
    ".git",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "Gemfile",
    "composer.json",
];

/// Whether `scaffold_root` is a project root (root-layout scaffold).
fn is_project_root(scaffold_root: &Path) -> bool {
    PROJECT_MARKERS.iter().any(|m| scaffold_root.join(m).exists())
}

/// Scaffold markdown documents, as (scaffold-relative path, absolute path).
///
/// Skips `local/`, `node_modules` and hidden entries at any depth (`.git`, `.venv`, ...); when
/// the scaffold root is the project root, also the build/dependency trees in
/// [`SCAFFOLD_SKIP_DIRS`]. Symlinked files and directories are followed (loops are detected
/// and skipped); a file reached through two links is listed once.
pub fn scaffold_markdown_files(scaffold_root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    if !scaffold_root.exists() {
        return out;
    }
    let root_layout = is_project_root(scaffold_root);
    let mut seen_real: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let walker = WalkDir::new(scaffold_root)
        .follow_links(true)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            if name.starts_with('.') {
                return false;
            }
            if e.file_type().is_dir() {
                if name == "node_modules" || (root_layout && SCAFFOLD_SKIP_DIRS.contains(&name.as_ref())) {
                    return false;
                }
                if e.depth() == 1 && name == "local" {
                    return false;
                }
            }
            true
        });
    for entry in walker.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !entry.file_type().is_file() || path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !seen_real.insert(real) {
            continue;
        }
        let rel = match path.strip_prefix(scaffold_root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => path.to_string_lossy().to_string(),
        };
        out.push((rel, path.to_path_buf()));
    }
    out
}

/// Collect grounding references from every scaffold document.
pub fn scan_scaffold_groundings(scaffold_root: &Path) -> Vec<DocGroundings> {
    let mut out = Vec::new();
    for (rel, path) in scaffold_markdown_files(scaffold_root) {
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Some(entity) = parse_markdown_entity(&rel, &content) {
            if !entity.grounds_to.is_empty() {
                out.push(DocGroundings {
                    doc: rel,
                    refs: entity.grounds_to,
                });
            }
        }
    }
    out
}

/// Read the source lines spanned by a node.
pub fn read_node_source(project_root: &Path, node: &Node) -> Option<String> {
    let content = fs::read_to_string(project_root.join(&node.file_path)).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start = node.start_line.max(1) as usize - 1;
    let end = (node.end_line.max(node.start_line) as usize).min(lines.len());
    if start >= end {
        return None;
    }
    Some(lines[start..end].join("\n"))
}

pub fn get_baseline(conn: &Connection, doc: &str, reference: &str) -> Option<Baseline> {
    conn.query_row(
        "SELECT body_hash, source, fingerprint FROM _knobyte_grounded_source
         WHERE subject_kind = ?1 AND subject_id = ?2 AND node_id = ?3",
        params![DOC_SUBJECT_KIND, doc, reference],
        |r| {
            Ok(Baseline {
                body_hash: r.get(0)?,
                source: r.get(1)?,
                node_id: r.get(2)?,
            })
        },
    )
    .ok()
}

/// Baseline written by older Knobyte versions (`graph ground` keyed by hashed node id).
pub fn legacy_baseline(conn: &Connection, node_id: &str) -> Option<Baseline> {
    conn.query_row(
        "SELECT body_hash, source, node_id FROM _knobyte_grounded_source
         WHERE subject_kind = 'scaffold' AND node_id = ?1 LIMIT 1",
        params![node_id],
        |r| {
            Ok(Baseline {
                body_hash: r.get(0)?,
                source: r.get(1)?,
                node_id: r.get(2)?,
            })
        },
    )
    .ok()
}

/// Record (or overwrite) the baseline of `reference` in `doc` with the node's current body.
pub fn record_baseline(
    conn: &Connection,
    project_root: &Path,
    doc: &str,
    reference: &str,
    node: &Node,
) -> rusqlite::Result<()> {
    let source = read_node_source(project_root, node).unwrap_or_default();
    let hash = node
        .body_hash
        .clone()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| compute_body_hash(&source));
    conn.execute(
        "INSERT OR REPLACE INTO _knobyte_grounded_source
            (subject_kind, subject_id, node_id, source, body_hash, fingerprint)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![DOC_SUBJECT_KIND, doc, reference, source, hash, node.id],
    )?;
    // Callers and callees at baseline time: the reconciler's neighbour evidence.
    let neighbors = crate::graph::reconcile::neighbors_of(conn, &node.id);
    conn.execute(
        "INSERT OR REPLACE INTO _knobyte_grounded_neighbors (subject_kind, subject_id, node_id, neighbors)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            DOC_SUBJECT_KIND,
            doc,
            reference,
            serde_json::to_string(&neighbors).unwrap_or_else(|_| "[]".into())
        ],
    )?;
    Ok(())
}

/// Move a baseline from `old_ref` to `new_ref` (after a relocation), keeping the baseline body
/// hash so that a symbol which moved *and* changed is still reported as changed.
pub fn move_baseline(
    conn: &Connection,
    doc: &str,
    old_ref: &str,
    new_ref: &str,
    new_node_id: &str,
) -> rusqlite::Result<usize> {
    let _ = conn.execute(
        "UPDATE OR REPLACE _knobyte_grounded_neighbors SET node_id = ?1
         WHERE subject_kind = ?2 AND subject_id = ?3 AND node_id = ?4",
        params![new_ref, DOC_SUBJECT_KIND, doc, old_ref],
    );
    conn.execute(
        "UPDATE OR REPLACE _knobyte_grounded_source SET node_id = ?1, fingerprint = ?2
         WHERE subject_kind = ?3 AND subject_id = ?4 AND node_id = ?5",
        params![new_ref, new_node_id, DOC_SUBJECT_KIND, doc, old_ref],
    )
}

// ---------------------------------------------------------------------------
// Committed baselines
// ---------------------------------------------------------------------------
//
// `graph.db` is gitignored and disposable, so a baseline that lives only there is lost by a
// fresh clone or a `graph rebuild`, after which drift is silently accepted. The baseline is
// therefore committed in the scaffold itself:
//
// ```yaml
// grounds_to:
//   - function:src/a.rs:f                 # plain reference: no committed baseline yet
//   - ref: function:src/b.rs:g
//     body_hash: <sha256 of the whitespace-normalised body>
//     fingerprint: mh1:<token count>:<MinHash sketch, hex>
// ```
//
// and on inline anchors as an optional hash suffix:
// `<!-- kb-ground: function:src/a.rs:f #<body_hash> -->`.
//
// Committed values win; the `_knobyte_grounded_source` cache is only a fallback for groundings
// authored before the fields existed.

/// Prefix (format version) of a serialized committed fingerprint.
pub const FINGERPRINT_PREFIX: &str = "mh1";

/// Serialize a MinHash sketch as `mh1:<token_count>:<hex>`.
pub fn serialize_fingerprint(mh: &MinHash) -> String {
    format!("{}:{}:{}", FINGERPRINT_PREFIX, mh.token_count, mh.to_hex())
}

/// Parse a committed fingerprint written by [`serialize_fingerprint`].
pub fn parse_fingerprint(text: &str) -> Option<MinHash> {
    let mut parts = text.trim().split(':');
    let (prefix, tokens, hex) = (parts.next()?, parts.next()?, parts.next()?);
    if prefix != FINGERPRINT_PREFIX || parts.next().is_some() {
        return None;
    }
    MinHash::from_hex(hex, tokens.parse().ok()?)
}

/// Baseline values committed with one grounding in the markdown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommittedBaseline {
    pub body_hash: Option<String>,
    pub fingerprint: Option<String>,
}

/// Where a grounding reference was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefOrigin {
    Frontmatter,
    /// Inline `<!-- kb-ground: ... -->` anchor (1-based line).
    Anchor(usize),
}

/// One grounding reference of a document, with its committed baseline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRef {
    pub reference: String,
    pub origin: RefOrigin,
    pub committed: CommittedBaseline,
}

/// Comment prefixes of inline grounding anchors.
pub const ANCHOR_PREFIXES: &[&str] = &["<!-- kb-ground:", "<!-- grounds:", "<!-- kb-anchor:"];

/// Split the inside of an anchor comment into its reference and optional `#<body_hash>` suffix.
pub fn split_anchor(inner: &str) -> (String, Option<String>) {
    let mut tokens = inner.split_whitespace();
    let reference = tokens.next().unwrap_or("").to_string();
    let hash = tokens
        .find_map(|t| t.strip_prefix('#'))
        .filter(|h| !h.is_empty())
        .map(str::to_string);
    (reference, hash)
}

fn yaml_str(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

/// The reference of one `grounds_to` entry (plain string, or a map with `ref` / `node_id` /
/// `node`) and its committed baseline.
pub fn grounds_to_entry(entry: &serde_json::Value) -> Option<(String, CommittedBaseline)> {
    if let Some(s) = entry.as_str() {
        let s = s.trim();
        return (!s.is_empty()).then(|| (s.to_string(), CommittedBaseline::default()));
    }
    let reference = ["ref", "node_id", "node"]
        .iter()
        .find_map(|k| entry.get(*k).and_then(|v| v.as_str()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())?;
    Some((
        reference,
        CommittedBaseline {
            body_hash: entry.get("body_hash").and_then(yaml_str),
            fingerprint: entry.get("fingerprint").and_then(yaml_str),
        },
    ))
}

/// Grounding references of one document: frontmatter `grounds_to` entries first, then inline
/// anchors. Each origin is deduplicated on its own, so a reference written both in the
/// frontmatter and as an anchor is checked (and reported) once per origin.
pub fn extract_doc_refs(content: &str) -> Vec<DocRef> {
    use crate::drift::markdown::{parse_frontmatter, split_frontmatter};
    let mut out: Vec<DocRef> = Vec::new();
    if let Some(entries) = parse_frontmatter(content)
        .as_ref()
        .and_then(|fm| fm.get("grounds_to"))
        .and_then(|g| g.as_array())
    {
        for e in entries {
            if let Some((reference, committed)) = grounds_to_entry(e) {
                if !out.iter().any(|d| d.reference == reference) {
                    out.push(DocRef {
                        reference,
                        origin: RefOrigin::Frontmatter,
                        committed,
                    });
                }
            }
        }
    }
    let (_, body_start) = split_frontmatter(content);
    for (idx, line) in content.split('\n').enumerate().skip(body_start - 1) {
        let trimmed = line.trim();
        for prefix in ANCHOR_PREFIXES {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                if let Some(inner) = rest.strip_suffix("-->") {
                    let (reference, hash) = split_anchor(inner);
                    let dup = out
                        .iter()
                        .any(|d| d.reference == reference && matches!(d.origin, RefOrigin::Anchor(_)));
                    if !reference.is_empty() && !dup {
                        out.push(DocRef {
                            reference,
                            origin: RefOrigin::Anchor(idx + 1),
                            committed: CommittedBaseline {
                                body_hash: hash,
                                fingerprint: None,
                            },
                        });
                    }
                }
                break;
            }
        }
    }
    out
}

/// Committed frontmatter baselines of every scaffold document, by reference.
#[derive(Debug, Clone, Default)]
pub struct CommittedIndex {
    by_ref: std::collections::HashMap<String, Vec<(String, CommittedBaseline)>>,
}

impl CommittedIndex {
    /// Index the frontmatter entries of `(scaffold-relative doc, refs)` pairs.
    pub fn build<'a>(docs: impl IntoIterator<Item = (&'a str, &'a [DocRef])>) -> Self {
        let mut idx = Self::default();
        for (doc, refs) in docs {
            for r in refs.iter().filter(|r| r.origin == RefOrigin::Frontmatter) {
                idx.by_ref
                    .entry(r.reference.clone())
                    .or_default()
                    .push((doc.to_string(), r.committed.clone()));
            }
        }
        idx
    }

    /// Read every scaffold document under `scaffold_root`.
    pub fn from_scaffold(scaffold_root: &Path) -> Self {
        let docs: Vec<(String, Vec<DocRef>)> = scaffold_markdown_files(scaffold_root)
            .into_iter()
            .filter_map(|(rel, path)| fs::read_to_string(path).ok().map(|c| (rel, extract_doc_refs(&c))))
            .collect();
        Self::build(docs.iter().map(|(d, r)| (d.as_str(), r.as_slice())))
    }

    /// The frontmatter entry for `reference` in `doc` itself.
    pub fn here(&self, doc: &str, reference: &str) -> Option<&CommittedBaseline> {
        self.by_ref
            .get(reference)
            .and_then(|v| v.iter().find(|(d, _)| d == doc).map(|(_, c)| c))
    }

    /// Frontmatter entries for `reference` in other documents.
    pub fn elsewhere(&self, doc: &str, reference: &str) -> Vec<&CommittedBaseline> {
        self.by_ref
            .get(reference)
            .map(|v| v.iter().filter(|(d, _)| d != doc).map(|(_, c)| c).collect())
            .unwrap_or_default()
    }
}

/// A grounding's baseline, resolved from committed values first and the graph.db cache second.
#[derive(Debug, Clone, Default)]
pub struct EffectiveBaseline {
    pub body_hash: Option<String>,
    pub minhash: Option<MinHash>,
    /// Cached baseline source text (`None` when only committed values exist).
    pub source: Option<String>,
    /// Graph node id the reference resolved to when the cached baseline was taken.
    pub node_id: Option<String>,
    /// Callers and callees recorded with the cached baseline.
    pub neighbors: Vec<String>,
    /// The body hash came from the committed markdown (not the cache).
    pub committed: bool,
    /// An anchor with no baseline of its own whose committed fingerprints in other scaffold
    /// files disagree: nothing is chosen between them.
    pub conflict: bool,
}

impl EffectiveBaseline {
    pub fn is_empty(&self) -> bool {
        self.body_hash.is_none() && self.minhash.is_none()
    }
}

/// Resolve the baseline of `r` in `doc` (most direct first):
/// the reference's own committed values; for an anchor, the same file's `grounds_to` entry for
/// the reference, else the entries in other scaffold files (disagreeing fingerprints are a
/// conflict, a body hash is taken only when they all agree); finally the graph.db cache.
pub fn resolve_baseline(
    conn: Option<&Connection>,
    doc: &str,
    r: &DocRef,
    index: &CommittedIndex,
) -> EffectiveBaseline {
    let own = &r.committed;
    let mut body_hash = own.body_hash.clone();
    let mut minhash = own.fingerprint.as_deref().and_then(parse_fingerprint);
    let mut conflict = false;
    if matches!(r.origin, RefOrigin::Anchor(_)) {
        if let Some(here) = index.here(doc, &r.reference) {
            body_hash = body_hash.or_else(|| here.body_hash.clone());
            minhash = minhash.or_else(|| here.fingerprint.as_deref().and_then(parse_fingerprint));
        } else {
            let elsewhere: Vec<&CommittedBaseline> = index
                .elsewhere(doc, &r.reference)
                .into_iter()
                .filter(|c| c.fingerprint.as_deref().and_then(parse_fingerprint).is_some())
                .collect();
            let fps: BTreeSet<&str> = elsewhere.iter().filter_map(|c| c.fingerprint.as_deref()).collect();
            if fps.len() > 1 {
                conflict = minhash.is_none();
            } else if let Some(first) = elsewhere.first() {
                minhash = minhash.or_else(|| first.fingerprint.as_deref().and_then(parse_fingerprint));
                let hashes: BTreeSet<Option<&str>> =
                    elsewhere.iter().map(|c| c.body_hash.as_deref()).collect();
                if hashes.len() == 1 {
                    body_hash = body_hash.or_else(|| first.body_hash.clone());
                }
            }
        }
    }
    let committed = body_hash.is_some();
    let cached = conn.and_then(|c| {
        get_baseline(c, doc, &r.reference).or_else(|| match parse_grounding_ref(&r.reference) {
            ParsedRef::LegacyId(id) | ParsedRef::Other(id) => legacy_baseline(c, &id),
            _ => None,
        })
    });
    let neighbors = conn
        .map(|c| crate::graph::reconcile::baseline_neighbors(c, DOC_SUBJECT_KIND, doc, &r.reference))
        .unwrap_or_default();
    let mut out = EffectiveBaseline {
        body_hash,
        minhash,
        source: None,
        node_id: None,
        neighbors,
        committed,
        conflict,
    };
    if let Some(b) = cached {
        if out.body_hash.is_none() && !b.body_hash.is_empty() {
            out.body_hash = Some(b.body_hash.clone());
        }
        if out.minhash.is_none() && !conflict && !b.source.trim().is_empty() {
            out.minhash = Some(MinHash::of_body(&b.source));
        }
        out.source = Some(b.source).filter(|s| !s.trim().is_empty());
        out.node_id = Some(b.node_id).filter(|s| !s.is_empty());
    }
    out
}

/// Body hash and committed fingerprint of `node` as they are now.
pub fn current_baseline_values(conn: &Connection, project_root: &Path, node: &Node) -> (String, String) {
    let source = read_node_source(project_root, node).unwrap_or_default();
    let hash = node
        .body_hash
        .clone()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| compute_body_hash(&source));
    let mh = crate::graph::reconcile::node_fingerprint(conn, &node.id)
        .unwrap_or_else(|| MinHash::of_body(&source));
    (hash, serialize_fingerprint(&mh))
}

fn yaml_scalar(s: &str) -> String {
    serde_yaml::to_string(&serde_yaml::Value::String(s.to_string()))
        .map(|t| t.trim_end().to_string())
        .unwrap_or_else(|_| format!("{:?}", s))
}

fn yaml_value_inline(v: &serde_yaml::Value, indent: usize) -> String {
    match v {
        serde_yaml::Value::String(s) => yaml_scalar(s),
        serde_yaml::Value::Mapping(_) | serde_yaml::Value::Sequence(_) => {
            let text = serde_yaml::to_string(v).unwrap_or_default();
            let pad = " ".repeat(indent);
            let body: Vec<String> = text.trim_end().lines().map(|l| format!("{}{}", pad, l)).collect();
            format!("\n{}", body.join("\n"))
        }
        other => serde_yaml::to_string(other)
            .map(|t| t.trim_end().to_string())
            .unwrap_or_default(),
    }
}

fn key_str(k: &serde_yaml::Value) -> String {
    match k {
        serde_yaml::Value::String(s) => s.clone(),
        other => serde_yaml::to_string(other)
            .map(|t| t.trim_end().to_string())
            .unwrap_or_default(),
    }
}

fn yaml_entry_ref(e: &serde_yaml::Value) -> Option<(Option<&'static str>, String)> {
    match e {
        serde_yaml::Value::String(s) => Some((None, s.trim().to_string())),
        serde_yaml::Value::Mapping(_) => ["ref", "node_id", "node"].iter().find_map(|k| {
            e.get(*k)
                .and_then(|v| v.as_str())
                .map(|s| (Some(*k), s.trim().to_string()))
        }),
        _ => None,
    }
}

/// Regenerate a `grounds_to` block with committed values for the references in `updates`.
fn render_grounds_to(entries: &[serde_yaml::Value], updates: &BTreeMap<String, (String, String)>) -> String {
    let mut out = String::from("grounds_to:\n");
    for e in entries {
        let (ref_key, reference) = match yaml_entry_ref(e) {
            Some((k, r)) => (k, Some(r)),
            None => (None, None),
        };
        let update = reference.as_ref().and_then(|r| updates.get(r));
        match e {
            serde_yaml::Value::Mapping(m) => {
                let mut lines: Vec<String> = Vec::new();
                if let (Some(k), Some(r)) = (ref_key, &reference) {
                    lines.push(format!("{}: {}", k, yaml_scalar(r)));
                }
                for (k, v) in m {
                    let key = key_str(k);
                    if Some(key.as_str()) == ref_key {
                        continue;
                    }
                    if update.is_some() && (key == "body_hash" || key == "fingerprint") {
                        continue;
                    }
                    lines.push(format!("{}: {}", key, yaml_value_inline(v, 6)));
                }
                if let Some((hash, fp)) = update {
                    lines.push(format!("body_hash: {}", yaml_scalar(hash)));
                    lines.push(format!("fingerprint: {}", yaml_scalar(fp)));
                }
                if lines.is_empty() {
                    out.push_str("  - {}\n");
                }
                for (i, l) in lines.iter().enumerate() {
                    out.push_str(if i == 0 { "  - " } else { "    " });
                    out.push_str(l);
                    out.push('\n');
                }
            }
            serde_yaml::Value::String(s) => match update {
                Some((hash, fp)) => {
                    out.push_str(&format!("  - ref: {}\n", yaml_scalar(s.trim())));
                    out.push_str(&format!("    body_hash: {}\n", yaml_scalar(hash)));
                    out.push_str(&format!("    fingerprint: {}\n", yaml_scalar(fp)));
                }
                None => out.push_str(&format!("  - {}\n", yaml_scalar(s))),
            },
            other => out.push_str(&format!("  - {}\n", yaml_value_inline(other, 4))),
        }
    }
    out
}

/// Write committed baselines (`reference -> (body_hash, fingerprint)`) into a document: the
/// matching `grounds_to` entries become `{ref, body_hash, fingerprint}` maps and the matching
/// anchors get a `#<body_hash>` suffix. Everything else is preserved byte for byte.
pub fn apply_committed_baselines(content: &str, updates: &BTreeMap<String, (String, String)>) -> String {
    if updates.is_empty() {
        return content.to_string();
    }
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let bare = |l: &str| l.trim_end_matches(['\n', '\r']).to_string();
    let mut out = String::with_capacity(content.len() + 256);
    let mut i = 0;

    // Frontmatter.
    let fm_end = if lines.first().map(|l| bare(l)).as_deref() == Some("---") {
        lines
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, l)| bare(l) == "---")
            .map(|(j, _)| j)
    } else {
        None
    };
    if let Some(end) = fm_end {
        out.push_str(lines[0]);
        i = 1;
        while i < end {
            if !bare(lines[i]).starts_with("grounds_to:") {
                out.push_str(lines[i]);
                i += 1;
                continue;
            }
            // The block: the key line plus the indented / list lines that follow it.
            let mut j = i + 1;
            while j < end {
                let l = bare(lines[j]);
                if l.trim().is_empty() || l.starts_with(' ') || l.starts_with('\t') || l.starts_with('-') {
                    j += 1;
                } else {
                    break;
                }
            }
            while j > i + 1 && bare(lines[j - 1]).trim().is_empty() {
                j -= 1;
            }
            let block: String = lines[i..j].concat();
            let parsed: Option<Vec<serde_yaml::Value>> = serde_yaml::from_str::<serde_yaml::Value>(&block)
                .ok()
                .and_then(|v| v.get("grounds_to").cloned())
                .and_then(|g| match g {
                    serde_yaml::Value::Sequence(s) => Some(s),
                    serde_yaml::Value::String(s) => Some(vec![serde_yaml::Value::String(s)]),
                    _ => None,
                });
            let touches = parsed.as_ref().is_some_and(|entries| {
                entries
                    .iter()
                    .filter_map(yaml_entry_ref)
                    .any(|(_, r)| updates.contains_key(&r))
            });
            match parsed {
                Some(entries) if touches => {
                    let mut rendered = render_grounds_to(&entries, updates);
                    if lines[i].ends_with("\r\n") {
                        rendered = rendered.replace('\n', "\r\n");
                    }
                    out.push_str(&rendered);
                }
                _ => out.push_str(&block),
            }
            i = j;
        }
    }

    // Body: anchors.
    for raw in &lines[i..] {
        let body = raw.trim_end_matches(['\n', '\r']);
        let ending = &raw[body.len()..];
        let trimmed = body.trim();
        let mut replaced = None;
        for prefix in ANCHOR_PREFIXES {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                if let Some(inner) = rest.strip_suffix("-->") {
                    let (reference, hash) = split_anchor(inner);
                    if let Some((new_hash, _)) = updates.get(&reference) {
                        if hash.as_deref() != Some(new_hash.as_str()) {
                            let indent = &body[..body.len() - body.trim_start().len()];
                            replaced = Some(format!("{}{} {} #{} -->", indent, prefix, reference, new_hash));
                        }
                    }
                }
                break;
            }
        }
        match replaced {
            Some(l) => {
                out.push_str(&l);
                out.push_str(ending);
            }
            None => out.push_str(raw),
        }
    }
    out
}

/// Capture baselines for every grounding of the scaffold documents accepted by `include`
/// (scaffold-relative path): record them in the graph.db cache and commit them into the
/// markdown. Returns the number of (document, reference) pairs baselined.
pub fn capture_baselines(
    conn: &Connection,
    project_root: &Path,
    scaffold_root: &Path,
    include: &dyn Fn(&str) -> bool,
) -> rusqlite::Result<usize> {
    let mut grounded = 0;
    for (rel, path) in scaffold_markdown_files(scaffold_root) {
        if !include(&rel) {
            continue;
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let mut updates: BTreeMap<String, (String, String)> = BTreeMap::new();
        for r in extract_doc_refs(&content) {
            if updates.contains_key(&r.reference) {
                continue;
            }
            if let RefResolution::Resolved(node) = resolve_grounding_ref(conn, &r.reference)? {
                record_baseline(conn, project_root, &rel, &r.reference, &node)?;
                updates.insert(r.reference.clone(), current_baseline_values(conn, project_root, &node));
                grounded += 1;
            }
        }
        let updated = apply_committed_baselines(&content, &updates);
        if updated != content {
            let _ = fs::write(&path, updated);
        }
    }
    Ok(grounded)
}

/// Re-baseline every grounded reference in the scaffold to the current code ("accept current
/// code"): cache and commit the baselines, and remove cached baselines of references no longer
/// present plus legacy whole-graph baselines. Returns the number of references baselined.
pub fn ground_documents(
    conn: &Connection,
    project_root: &Path,
    scaffold_root: &Path,
) -> rusqlite::Result<usize> {
    let mut keep: BTreeSet<(String, String)> = BTreeSet::new();
    for (rel, path) in scaffold_markdown_files(scaffold_root) {
        if let Ok(content) = fs::read_to_string(&path) {
            for r in extract_doc_refs(&content) {
                keep.insert((rel.clone(), r.reference));
            }
        }
    }
    let grounded = capture_baselines(conn, project_root, scaffold_root, &|_| true)?;

    conn.execute(
        "DELETE FROM _knobyte_grounded_source WHERE subject_kind = 'scaffold'",
        [],
    )?;
    let existing: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT subject_id, node_id FROM _knobyte_grounded_source WHERE subject_kind = ?1",
        )?;
        let rows = stmt.query_map(params![DOC_SUBJECT_KIND], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (doc, r) in existing {
        if !keep.contains(&(doc.clone(), r.clone())) {
            conn.execute(
                "DELETE FROM _knobyte_grounded_source WHERE subject_kind = ?1 AND subject_id = ?2 AND node_id = ?3",
                params![DOC_SUBJECT_KIND, doc, r],
            )?;
            conn.execute(
                "DELETE FROM _knobyte_grounded_neighbors WHERE subject_kind = ?1 AND subject_id = ?2 AND node_id = ?3",
                params![DOC_SUBJECT_KIND, doc, r],
            )?;
        }
    }
    Ok(grounded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_readable_and_legacy_refs() {
        assert_eq!(
            parse_grounding_ref("function:src/auth.rs:validate_token"),
            ParsedRef::Readable(GroundingRef {
                kind: "function".into(),
                file_path: "src/auth.rs".into(),
                qualified_name: "validate_token".into(),
            })
        );
        match parse_grounding_ref("method:src/a.rs:Auth::check") {
            ParsedRef::Readable(r) => {
                assert_eq!(r.qualified_name, "Auth::check");
                assert_eq!(r.symbol_name(), "check");
            }
            other => panic!("unexpected {:?}", other),
        }
        assert!(matches!(
            parse_grounding_ref("function:0123456789abcdef0123456789abcdef"),
            ParsedRef::LegacyId(_)
        ));
        assert!(kinds_equivalent("function", "method"));
        assert!(!kinds_equivalent("function", "struct"));
    }
}
