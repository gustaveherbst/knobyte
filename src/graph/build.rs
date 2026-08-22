//! Graph construction: read + extract each corpus file (or recover its extraction from the
//! content-addressed cache), then write nodes, resolved edges and unresolved references into a
//! (candidate) database in one transaction.

use rusqlite::{params, Connection, OptionalExtension, Result, Statement};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::graph::corpus::{CoverageReport, IndexableFile};
use crate::graph::extractor::{
    extract_file, is_code_language, language_for_path, ExtractionResult, EXTRACTOR_VERSION, LINK_PREFIX,
    LINK_RUST_MOD,
};
use crate::graph::links::{rust_mod_file, ExpressMounts};
use crate::graph::maintenance::{cancelled_error, CancelHook};
use crate::graph::chunks::{chunk_source, existing_chunks, insert_chunks, ChunkRow};
use crate::graph::fingerprint::{
    compute_body_hash, compute_file_hash, compute_node_id, MinHash, FINGERPRINT_KINDS,
};
use crate::graph::models::{ExtractedCall, ExtractedImport, ExtractedRef, ExtractedTraitImpl};
use crate::graph::resolve::{Binding, CallResolution, NodeMeta, SymbolIndex};
use crate::progress::IndexProgressBar;

/// Test hook: abort the process after extracting this many files (simulates a crash mid-build).
pub const ABORT_AFTER_FILES_ENV: &str = "KNOBYTE_GRAPH_TEST_ABORT_AFTER_FILES";

pub(crate) fn file_mtime_ms(path: &Path) -> i64 {
    fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64
        })
        .unwrap_or(0)
}

/// Cached, content-addressed extraction of one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedFile {
    pub language: String,
    pub parse_status: String,
    pub line_count: usize,
    pub last_col: usize,
    pub extraction: Option<ExtractionResult>,
    /// Body hash per extracted symbol (bodies themselves are not cached).
    pub body_hashes: Vec<String>,
    /// MinHash sketch (hex) and normalized token count per symbol, for fingerprinted kinds.
    #[serde(default)]
    pub minhashes: Vec<Option<(String, usize)>>,
}

/// A corpus file ready to be written into the graph.
#[derive(Debug, Clone)]
pub(crate) struct PreparedFile {
    pub rel_path: String,
    pub content_hash: String,
    pub size: i64,
    pub modified_at: i64,
    pub data: CachedFile,
    pub from_cache: bool,
    /// Source-chunk index rows, when the content was read by this build.
    pub chunks: Option<Vec<ChunkRow>>,
}

/// What the live graph knew about a file: used to skip reading unchanged files.
#[derive(Debug, Clone)]
pub(crate) struct KnownFile {
    pub content_hash: String,
    pub modified_at: i64,
    pub size: i64,
}

pub(crate) struct CacheEntry {
    pub content_hash: String,
    pub extractor_version: String,
    pub payload: String,
}

pub(crate) fn load_known_files(conn: &Connection) -> HashMap<String, KnownFile> {
    let mut out = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT path, content_hash, modified_at, size FROM files") {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                KnownFile {
                    content_hash: r.get(1)?,
                    modified_at: r.get(2)?,
                    size: r.get(3)?,
                },
            ))
        }) {
            for (p, k) in rows.flatten() {
                out.insert(p, k);
            }
        }
    }
    out
}

pub(crate) fn load_cache(conn: &Connection) -> HashMap<String, CacheEntry> {
    let mut out = HashMap::new();
    if let Ok(mut stmt) = conn
        .prepare("SELECT path, content_hash, extractor_version, payload FROM file_extraction_cache")
    {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                CacheEntry {
                    content_hash: r.get(1)?,
                    extractor_version: r.get(2)?,
                    payload: r.get(3)?,
                },
            ))
        }) {
            for (p, e) in rows.flatten() {
                out.insert(p, e);
            }
        }
    }
    out
}

fn extract_fresh(rel_path: &str, bytes: Vec<u8>) -> CachedFile {
    let content = match String::from_utf8(bytes) {
        Ok(c) => c,
        Err(_) => {
            return CachedFile {
                language: language_for_path(rel_path).to_string(),
                parse_status: "failed".to_string(),
                line_count: 0,
                last_col: 0,
                extraction: None,
                body_hashes: Vec::new(),
                minhashes: Vec::new(),
            }
        }
    };
    let line_count = content.lines().count().max(1);
    let last_col = content.lines().last().map(|l| l.len()).unwrap_or(0);
    match extract_file(rel_path, &content) {
        Some(mut e) => {
            let body_hashes = e.symbols.iter().map(|s| compute_body_hash(&s.body)).collect();
            let minhashes = e
                .symbols
                .iter()
                .map(|s| {
                    FINGERPRINT_KINDS.contains(&s.kind.as_str()).then(|| {
                        let mh = MinHash::of_body(&s.body);
                        (mh.to_hex(), mh.token_count)
                    })
                })
                .collect();
            for s in &mut e.symbols {
                s.body = String::new();
            }
            CachedFile {
                language: e.language.clone(),
                parse_status: e.parse_status.clone(),
                line_count,
                last_col,
                extraction: Some(e),
                body_hashes,
                minhashes,
            }
        }
        None => CachedFile {
            language: language_for_path(rel_path).to_string(),
            parse_status: "ok".to_string(),
            line_count,
            last_col,
            extraction: None,
            body_hashes: Vec::new(),
            minhashes: Vec::new(),
        },
    }
}

/// Read and extract a file, reusing the cached extraction when its content is unchanged.
/// `known` (the live graph's record) lets an unchanged mtime+size skip reading the file.
/// Returns `None` when the file cannot be read.
pub(crate) fn prepare_file(
    file: &IndexableFile,
    known: Option<&KnownFile>,
    cache: Option<&CacheEntry>,
) -> Option<PreparedFile> {
    let modified_at = file_mtime_ms(&file.path);
    let cache_valid = |hash: &str| {
        cache
            .filter(|c| c.content_hash == hash && c.extractor_version == EXTRACTOR_VERSION)
            .and_then(|c| serde_json::from_str::<CachedFile>(&c.payload).ok())
    };
    if let Some(k) = known {
        if k.modified_at == modified_at && k.size == file.size as i64 {
            if let Some(data) = cache_valid(&k.content_hash) {
                return Some(PreparedFile {
                    rel_path: file.rel_path.clone(),
                    content_hash: k.content_hash.clone(),
                    size: k.size,
                    modified_at,
                    data,
                    from_cache: true,
                    chunks: None,
                });
            }
        }
    }
    let bytes = fs::read(&file.path).ok()?;
    let content_hash = compute_file_hash(&bytes);
    let size = bytes.len() as i64;
    let chunks = std::str::from_utf8(&bytes)
        .ok()
        .map(|text| chunk_source(&file.rel_path, text));
    if let Some(data) = cache_valid(&content_hash) {
        return Some(PreparedFile {
            rel_path: file.rel_path.clone(),
            content_hash,
            size,
            modified_at,
            data,
            from_cache: true,
            chunks,
        });
    }
    Some(PreparedFile {
        rel_path: file.rel_path.clone(),
        content_hash,
        size,
        modified_at,
        data: extract_fresh(&file.rel_path, bytes),
        from_cache: false,
        chunks,
    })
}

/// Prepare every corpus file (in parallel), preserving corpus order.
/// With `cancel`, workers stop taking files once it returns true (the caller then reports the
/// cancellation; a partial result is never published).
pub(crate) fn prepare_files(
    files: &[IndexableFile],
    known: &HashMap<String, KnownFile>,
    cache: &HashMap<String, CacheEntry>,
    progress: Option<&IndexProgressBar>,
    cancel: Option<&CancelHook>,
) -> Vec<PreparedFile> {
    let abort_after: Option<usize> = std::env::var(ABORT_AFTER_FILES_ENV)
        .ok()
        .and_then(|v| v.parse().ok());
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = std::sync::atomic::AtomicUsize::new(0);
    let mut slots: Vec<Option<PreparedFile>> = vec![None; files.len()];
    let results = std::sync::Mutex::new(Vec::with_capacity(files.len()));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if cancel.is_some_and(|c| c()) {
                    break;
                }
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if i >= files.len() {
                    break;
                }
                let f = &files[i];
                let prepared = prepare_file(f, known.get(&f.rel_path), cache.get(&f.rel_path));
                let n = done.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if abort_after.is_some_and(|limit| n >= limit) {
                    // Simulated crash: no unwinding, no cleanup, exactly like a killed process.
                    std::process::abort();
                }
                if let Some(p) = progress {
                    let syms = prepared
                        .as_ref()
                        .and_then(|p| p.data.extraction.as_ref())
                        .map(|e| e.symbols.len())
                        .unwrap_or(0);
                    p.inc_file(&f.rel_path, f.size, syms);
                }
                results.lock().unwrap().push((i, prepared));
            });
        }
    });
    for (i, p) in results.into_inner().unwrap() {
        slots[i] = p;
    }
    slots.into_iter().flatten().collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BuildSummary {
    /// Files in the graph (same as `graph status` `counts.files`).
    pub files_indexed: usize,
    /// Nodes in the graph, including per-file nodes (same as `counts.nodes`).
    pub nodes_indexed: usize,
    /// Declarations (nodes other than `file`/`module`).
    #[serde(default)]
    pub symbols_indexed: usize,
    /// Edges in the graph (same as `counts.edges`).
    pub edges_indexed: usize,
    pub duration_ms: u128,
    /// Files whose extraction was recovered from the content-hash cache.
    #[serde(default)]
    pub files_reused: usize,
    /// Files parsed by this build.
    #[serde(default)]
    pub files_extracted: usize,
    /// Edge counts per kind.
    #[serde(default)]
    pub edges_by_kind: std::collections::BTreeMap<String, usize>,
    /// TypeScript type-checker mode of this build (`source`, `typescript <version> (...)`, or
    /// `source (fallback: <reason>)`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typescript_compiler: Option<String>,
}

/// Provenance recorded for an edge produced by a resolution method.
fn provenance_for(method: &str) -> &'static str {
    match method {
        "global_unique" | "global_unique_method" | "trait_dispatch" | "rust_reexport"
        | "import_default" => "heuristic",
        "trait_impl" | "structural" | "declared-export" | "inheritance" | "rust-mod-decl" => "tree-sitter",
        m if m.ends_with("-route-handler") => "framework",
        crate::graph::ts_compiler::PROVENANCE => crate::graph::ts_compiler::PROVENANCE,
        crate::graph::resolve::ts_infer::PROVENANCE => crate::graph::resolve::ts_infer::PROVENANCE,
        _ => "lexical",
    }
}

/// Resolution method recorded on a `route -> handler` edge.
fn route_handler_method(framework: &str) -> &'static str {
    match framework {
        "express" => "express-route-handler",
        "fastapi" => "fastapi-route-handler",
        "flask" => "flask-route-handler",
        "nestjs" => "nestjs-route-handler",
        _ => "nextjs-route-handler",
    }
}

const INSERT_NODE_SQL: &str = r#"
    INSERT OR REPLACE INTO nodes (
        id, kind, name, qualified_name, identity_key, file_path, language,
        start_line, end_line, start_column, end_column, docstring, signature, visibility,
        is_exported, is_async, is_static, is_abstract, return_type, body_hash, updated_at
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)
"#;

struct NodeRow<'a> {
    id: &'a str,
    kind: &'a str,
    name: &'a str,
    qualified_name: &'a str,
    file_path: &'a str,
    language: &'a str,
    start_line: i64,
    end_line: i64,
    start_col: i64,
    end_col: i64,
    docstring: Option<&'a str>,
    signature: Option<&'a str>,
    visibility: Option<&'a str>,
    is_exported: bool,
    is_async: bool,
    is_static: bool,
    is_abstract: bool,
    return_type: Option<&'a str>,
    body_hash: &'a str,
    updated_at: i64,
}

fn insert_node(stmt: &mut Statement, row: &NodeRow) -> Result<usize> {
    let identity_key = format!("{}:{}:{}", row.file_path, row.kind, row.qualified_name);
    stmt.execute(params![
        row.id,
        row.kind,
        row.name,
        row.qualified_name,
        identity_key,
        row.file_path,
        row.language,
        row.start_line,
        row.end_line,
        row.start_col,
        row.end_col,
        row.docstring,
        row.signature,
        row.visibility,
        row.is_exported as i64,
        row.is_async as i64,
        row.is_static as i64,
        row.is_abstract as i64,
        row.return_type,
        row.body_hash,
        row.updated_at,
    ])
}

/// Inserts edges, de-duplicating (source, target, kind) and counting real inserts only.
struct EdgeWriter<'a> {
    stmt: Statement<'a>,
    seen: HashSet<(String, String, String)>,
    inserted: usize,
    by_kind: std::collections::BTreeMap<String, usize>,
}

impl<'a> EdgeWriter<'a> {
    fn new(tx: &'a Connection) -> Result<Self> {
        Ok(Self {
            stmt: tx.prepare(
                "INSERT INTO edges (source, target, kind, metadata, line, col, confidence, resolution_method, provenance) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?,
            seen: HashSet::new(),
            inserted: 0,
            by_kind: Default::default(),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn insert(
        &mut self,
        source: &str,
        target: &str,
        kind: &str,
        metadata: Option<&str>,
        line: i64,
        col: i64,
        confidence: f64,
        method: &str,
    ) {
        if source == target && kind != "calls" {
            return;
        }
        // Call edges are per call site: two calls from one caller to one target at different
        // positions are two edges (each carries its own line/column). Other kinds are facts
        // about a pair and are kept once.
        let site = if crate::graph::engine::CALL_EDGE_KINDS.contains(&kind) {
            format!("{}:{}", line, col)
        } else {
            String::new()
        };
        let key = (source.to_string(), target.to_string(), format!("{}@{}", kind, site));
        if !self.seen.insert(key) {
            return;
        }
        if let Ok(n) = self.stmt.execute(params![
            source,
            target,
            kind,
            metadata,
            line,
            col,
            confidence,
            method,
            provenance_for(method)
        ]) {
            self.inserted += n;
            *self.by_kind.entry(kind.to_string()).or_default() += n;
        }
    }
}

/// Node kinds a non-call reference of each kind may bind to.
fn ref_target_kinds(kind: &str) -> &'static [&'static str] {
    match kind {
        "extends" => &["class", "interface", "struct", "trait"],
        "implements" => &["interface", "class", "trait"],
        "instantiates" => &["class", "struct", "enum"],
        "returns" | "type_of" | "aliases" => &["class", "struct", "enum", "interface", "trait", "type_alias"],
        "decorates" => &["function", "class", "method"],
        "references" => &["function", "method"],
        _ => &[],
    }
}

/// Metadata recorded with a build.
pub(crate) struct BuildMeta<'a> {
    pub root: &'a Path,
    pub policy_hash: &'a str,
    pub coverage: &'a CoverageReport,
    pub mode: &'a str,
    /// Live graph whose source-chunk rows unchanged files can reuse (refresh).
    pub live: Option<&'a Connection>,
    /// Polled between build phases; a cancelled build fails with
    /// `GRAPH_MAINTENANCE_CANCELLED` and its transaction is rolled back.
    pub cancel: Option<&'a CancelHook>,
}

impl BuildMeta<'_> {
    fn check_cancel(&self, phase: &str) -> Result<()> {
        if self.cancel.is_some_and(|c| c()) {
            return Err(cancelled_error(phase));
        }
        Ok(())
    }
}

/// Write a complete graph for `prepared` into `conn` (a fresh candidate), inside one
/// transaction.
pub(crate) fn write_graph(
    conn: &mut Connection,
    prepared: Vec<PreparedFile>,
    meta: &BuildMeta,
    progress: Option<&IndexProgressBar>,
) -> Result<BuildSummary> {
    let start_time = std::time::Instant::now();
    let mut summary = BuildSummary::default();
    let tx = conn.transaction()?;

    let mut all_calls: Vec<(String, ExtractedCall)> = Vec::new();
    let mut all_imports: Vec<(String, String, ExtractedImport)> = Vec::new();
    let mut all_trait_impls: Vec<(String, ExtractedTraitImpl)> = Vec::new();
    let mut all_refs: Vec<(String, ExtractedRef)> = Vec::new();
    let mut rust_mods: Vec<(String, ExtractedRef)> = Vec::new();
    let mut metas: Vec<NodeMeta> = Vec::new();
    let mut all_files: HashSet<String> = HashSet::new();
    let mut file_langs: HashMap<String, String> = HashMap::new();
    // TS/JS files with content hashes, for the optional type-checker mode.
    let mut ts_inputs: Vec<(String, String)> = Vec::new();
    let indexed_at = chrono::Utc::now().timestamp_millis();

    // Cross-file Express router mounts, composed before any route node is written.
    let corpus_paths: HashSet<String> = prepared.iter().map(|p| p.rel_path.clone()).collect();
    let express_mounts = ExpressMounts::compose(
        prepared
            .iter()
            .filter_map(|p| p.data.extraction.as_ref().map(|e| (p.rel_path.as_str(), e))),
        &corpus_paths,
    );

    {
        let mut node_stmt = tx.prepare(INSERT_NODE_SQL)?;
        let mut file_stmt = tx.prepare(
            r#"
            INSERT OR REPLACE INTO files (
                path, content_hash, language, size, modified_at, indexed_at, node_count,
                parse_status, extractor_version
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
        )?;
        let mut minhash_stmt = tx.prepare(
            "INSERT OR REPLACE INTO node_minhash (node_id, minhash, token_count) VALUES (?1, ?2, ?3)",
        )?;
        let mut lsh_stmt =
            tx.prepare("INSERT INTO node_lsh (band, bucket, node_id) VALUES (?1, ?2, ?3)")?;
        let mut cache_stmt = tx.prepare(
            "INSERT OR REPLACE INTO file_extraction_cache (path, content_hash, extractor_version, payload) \
             VALUES (?1, ?2, ?3, ?4)",
        )?;

        for pf in prepared {
            let rel_path = pf.rel_path.clone();
            all_files.insert(rel_path.clone());
            if pf.from_cache {
                summary.files_reused += 1;
            } else {
                summary.files_extracted += 1;
            }
            let chunks = pf
                .chunks
                .clone()
                .or_else(|| meta.live.and_then(|live| existing_chunks(live, &rel_path, &pf.content_hash)))
                .or_else(|| {
                    let bytes = fs::read(meta.root.join(&rel_path)).ok()?;
                    (compute_file_hash(&bytes) == pf.content_hash)
                        .then(|| std::str::from_utf8(&bytes).ok().map(|t| chunk_source(&rel_path, t)))
                        .flatten()
                });
            if let Some(rows) = &chunks {
                insert_chunks(&tx, &rel_path, &pf.content_hash, rows)?;
            }
            let payload = serde_json::to_string(&pf.data).unwrap_or_default();
            cache_stmt.execute(params![rel_path, pf.content_hash, EXTRACTOR_VERSION, payload])?;

            let mut data = pf.data;
            let node_count = data
                .extraction
                .as_ref()
                .map(|e| e.symbols.len())
                .unwrap_or(0);
            file_stmt.execute(params![
                rel_path,
                pf.content_hash,
                data.language,
                pf.size,
                pf.modified_at,
                indexed_at,
                node_count as i64,
                data.parse_status,
                EXTRACTOR_VERSION,
            ])?;
            let mut extraction = match data.extraction.take() {
                Some(e) => e,
                None => continue,
            };
            if !express_mounts.is_empty() {
                express_mounts.apply(&rel_path, &mut extraction, &mut data.body_hashes, &mut data.minhashes);
            }
            summary.files_indexed += 1;
            let language = extraction.language.clone();
            file_langs.insert(rel_path.clone(), language.clone());
            if matches!(language.as_str(), "typescript" | "tsx" | "javascript") {
                ts_inputs.push((rel_path.clone(), pf.content_hash.clone()));
            }

            // Per-file node: source of import edges and of module-level calls.
            if is_code_language(&language) {
                let file_id = compute_node_id(&rel_path, "file", &rel_path);
                let file_name = rel_path.rsplit('/').next().unwrap_or(&rel_path).to_string();
                insert_node(
                    &mut node_stmt,
                    &NodeRow {
                        id: &file_id,
                        kind: "file",
                        name: &file_name,
                        qualified_name: &rel_path,
                        file_path: &rel_path,
                        language: &language,
                        start_line: 1,
                        end_line: data.line_count as i64,
                        start_col: 0,
                        end_col: data.last_col as i64,
                        docstring: None,
                        signature: None,
                        visibility: None,
                        is_exported: true,
                        is_async: false,
                        is_static: false,
                        is_abstract: false,
                        return_type: None,
                        body_hash: &pf.content_hash,
                        updated_at: indexed_at,
                    },
                )?;
                metas.push(NodeMeta {
                    id: file_id,
                    kind: "file".to_string(),
                    name: file_name.clone(),
                    qualified_name: file_name,
                    file_path: rel_path.clone(),
                    container: None,
                    start_line: 1,
                    end_line: data.line_count,
                    start_col: 0,
                    end_col: data.last_col,
                    is_exported: true,
                });
            }

            for (k, sym) in extraction.symbols.into_iter().enumerate() {
                let node_id = compute_node_id(&rel_path, &sym.kind, &sym.qualified_name);
                let body_hash = data.body_hashes.get(k).cloned().unwrap_or_default();
                if let Some(Some((hex, tokens))) = data.minhashes.get(k) {
                    if let Some(mh) = MinHash::from_hex(hex, *tokens) {
                        minhash_stmt.execute(params![node_id, mh.to_blob(), *tokens as i64])?;
                        for (band, bucket) in mh.band_hashes().into_iter().enumerate() {
                            lsh_stmt.execute(params![band as i64, bucket, node_id])?;
                        }
                    }
                }
                insert_node(
                    &mut node_stmt,
                    &NodeRow {
                        id: &node_id,
                        kind: &sym.kind,
                        name: &sym.name,
                        qualified_name: &sym.qualified_name,
                        file_path: &rel_path,
                        language: &language,
                        start_line: sym.start_line as i64,
                        end_line: sym.end_line as i64,
                        start_col: sym.start_col as i64,
                        end_col: sym.end_col as i64,
                        docstring: sym.docstring.as_deref(),
                        signature: sym.signature.as_deref(),
                        visibility: sym.visibility.as_deref(),
                        is_exported: sym.is_exported,
                        is_async: sym.is_async,
                        is_static: sym.is_static,
                        is_abstract: sym.is_abstract,
                        return_type: sym.return_type.as_deref(),
                        body_hash: &body_hash,
                        updated_at: indexed_at,
                    },
                )?;
                metas.push(NodeMeta {
                    id: node_id,
                    kind: sym.kind,
                    name: sym.name,
                    qualified_name: sym.qualified_name,
                    file_path: rel_path.clone(),
                    container: sym.container,
                    start_line: sym.start_line,
                    end_line: sym.end_line,
                    start_col: sym.start_col,
                    end_col: sym.end_col,
                    is_exported: sym.is_exported,
                });
                summary.nodes_indexed += 1;
            }

            for call in extraction.calls {
                all_calls.push((rel_path.clone(), call));
            }
            for imp in extraction.imports {
                all_imports.push((rel_path.clone(), language.clone(), imp));
            }
            for ti in extraction.trait_impls {
                all_trait_impls.push((rel_path.clone(), ti));
            }
            for r in extraction.refs {
                if r.kind == LINK_RUST_MOD {
                    rust_mods.push((rel_path.clone(), r));
                } else if !r.kind.starts_with(LINK_PREFIX) {
                    all_refs.push((rel_path.clone(), r));
                }
            }
        }
    }

    meta.check_cancel("writing nodes")?;
    if let Some(p) = progress {
        p.set_phase("Resolving symbols, containment and inheritance...");
    }
    let ts_configs = crate::graph::tsconfig::TsConfigs::load(meta.root, all_files.iter());
    let mut index = SymbolIndex::new(metas, all_files);
    index.set_ts_configs(ts_configs);
    for (file_path, language, imp) in &all_imports {
        if matches!(language.as_str(), "typescript" | "tsx" | "javascript") {
            if imp.is_reexport {
                index.add_reexport(file_path, imp);
            } else {
                index.add_js_import(file_path, imp);
            }
        }
    }
    // JS/TS `export default` facts feed import resolution; they are not edges themselves.
    all_refs.retain(|(file_path, r)| {
        if r.kind == "default_export" {
            index.set_default_export(file_path, &r.target_name);
            return false;
        }
        if r.kind == crate::graph::resolve::ts_infer::FACT_KIND {
            index.add_ts_fact(file_path, r);
            return false;
        }
        true
    });

    // Optional TypeScript type-checker mode (`graph.typescript.compiler: "tsc"` / `--ts-compiler`):
    // checker facts override source resolution for the calls, signatures and type aliases they
    // answer. Source-only (the default), or unavailable / failed checker: no facts, no change.
    let ts_outcome = crate::graph::ts_compiler::collect(meta.root, &ts_inputs);
    let mut ts_applied = ts_outcome.facts.as_ref().map(|f| f.apply(&index.metas));
    if let Some(applied) = &ts_applied {
        let mut stmt = tx.prepare(
            "UPDATE nodes SET signature = ?1, return_type = COALESCE(?2, return_type) WHERE id = ?3",
        )?;
        for (i, signature, return_type) in &applied.signatures {
            stmt.execute(params![signature, return_type, index.metas[*i].id])?;
        }
    }
    summary.typescript_compiler = Some(ts_outcome.status.clone());

    // 0. Containers.
    {
        let mut stmt = tx.prepare("UPDATE nodes SET container_id = ?1 WHERE id = ?2")?;
        for i in 0..index.metas.len() {
            if let Some(c) = index.container_of(i) {
                stmt.execute(params![index.metas[c].id, index.metas[i].id])?;
            }
        }
    }

    let mut edges = EdgeWriter::new(&tx)?;

    // 0b. Structure: `contains` (lexical parent -> declaration) and `exports`
    // (file -> exported top-level declaration).
    for i in 0..index.metas.len() {
        let Some(parent) = index.lexical_parent(i) else {
            continue;
        };
        let (src, dst) = (index.metas[parent].id.clone(), index.metas[i].id.clone());
        let line = index.metas[i].start_line as i64;
        edges.insert(&src, &dst, "contains", None, line, 0, 1.0, "structural");
        let m = &index.metas[i];
        let lang = file_langs.get(&m.file_path).map(String::as_str).unwrap_or("");
        if index.metas[parent].kind == "file"
            && m.is_exported
            && matches!(lang, "typescript" | "tsx" | "javascript" | "rust" | "csharp" | "swift")
        {
            edges.insert(&src, &dst, "exports", None, line, 0, 1.0, "declared-export");
        }
    }

    // 0c. `mod name;` -> the module's file.
    for (file_path, r) in &rust_mods {
        let Some(ns) = index.symbol_in_file(file_path, &r.from, "namespace") else { continue };
        let Some(target) = rust_mod_file(file_path, &r.target_name, r.qualifier.as_deref(), &corpus_paths)
            .and_then(|f| index.file_node(&f))
        else {
            continue;
        };
        let (src, dst) = (index.metas[ns].id.clone(), index.metas[target].id.clone());
        edges.insert(&src, &dst, "contains", None, r.line as i64, r.col as i64, 1.0, "rust-mod-decl");
    }

    // 1. Trait implementations (implements and impl_of edges).
    if !all_trait_impls.is_empty() {
        if let Some(p) = progress {
            p.set_phase("Resolving trait implementations & method dispatch...");
        }
    }
    for (file_path, ti) in &all_trait_impls {
        let (type_idx, trait_idx) = index.link_trait_impl(file_path, &ti.trait_name, &ti.type_name);
        if let (Some(ty), Some(tr)) = (type_idx, trait_idx) {
            let (src, dst) = (index.metas[ty].id.clone(), index.metas[tr].id.clone());
            edges.insert(&src, &dst, "implements", None, ti.line as i64, 0, 1.0, "trait_impl");
        }
    }
    let impl_pairs: Vec<(usize, usize)> = index.impl_of.iter().map(|(a, b)| (*a, *b)).collect();
    for (impl_m, trait_m) in impl_pairs {
        let line = index.metas[impl_m].start_line as i64;
        let (src, dst) = (
            index.metas[impl_m].id.clone(),
            index.metas[trait_m].id.clone(),
        );
        edges.insert(&src, &dst, "impl_of", None, line, 0, 1.0, "trait_impl");
    }

    // 2. Imports: bindings + edges from the importing file node.
    if !all_imports.is_empty() {
        if let Some(p) = progress {
            p.set_phase(&format!("Linking {} module imports...", all_imports.len()));
        }
    }
    let mut bindings_by_file: HashMap<String, Vec<Binding>> = HashMap::new();
    {
        let mut module_stmt = tx.prepare(INSERT_NODE_SQL)?;
        let mut binding_stmt = tx.prepare(
            r#"
            INSERT OR REPLACE INTO import_bindings (
                binding_key, file_path, local_name, imported_name, module_specifier,
                resolved_file_path, target_id, is_type_only, metadata
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
        )?;
        let mut module_nodes: HashSet<String> = HashSet::new();

        for (file_path, language, imp) in &all_imports {
            let res = index.resolve_import(file_path, language, imp);
            let target_id = res.target.map(|t| index.metas[t].id.clone());

            let edge_target = match &target_id {
                Some(t) => t.clone(),
                None => {
                    // External (or unresolvable) module node, tagged with the importer's language.
                    let module_id =
                        compute_node_id("", "module", &format!("{}:{}", language, imp.source_module));
                    if module_nodes.insert(module_id.clone()) {
                        insert_node(
                            &mut module_stmt,
                            &NodeRow {
                                id: &module_id,
                                kind: "module",
                                name: &imp.source_module,
                                qualified_name: &imp.source_module,
                                file_path: "",
                                language,
                                start_line: 0,
                                end_line: 0,
                                start_col: 0,
                                end_col: 0,
                                docstring: None,
                                signature: Some(&imp.source_module),
                                visibility: None,
                                is_exported: true,
                                is_async: false,
                                is_static: false,
                                is_abstract: false,
                                return_type: None,
                                body_hash: "",
                                updated_at: indexed_at,
                            },
                        )?;
                    }
                    module_id
                }
            };

            let metadata = serde_json::json!({
                "module": imp.source_module,
                "imported": imp.imported_name,
                "local": imp.local_name,
                "reexport": imp.is_reexport,
                "resolution": res.method,
            })
            .to_string();

            if let Some(src) = index.file_node(file_path).map(|i| index.metas[i].id.clone()) {
                edges.insert(
                    &src,
                    &edge_target,
                    "imports",
                    Some(&metadata),
                    imp.line as i64,
                    0,
                    if target_id.is_some() { res.confidence } else { 1.0 },
                    res.method,
                );
            }

            let binding_key = format!(
                "{}:{}:{}:{}:{}",
                file_path, imp.line, imp.source_module, imp.imported_name, imp.local_name
            );
            binding_stmt.execute(params![
                binding_key,
                file_path,
                imp.local_name,
                imp.imported_name,
                imp.source_module,
                res.resolved_file,
                target_id,
                imp.is_type_only as i64,
                metadata,
            ])?;

            // Glob imports (`use super::*`, `from x import *`) bind no name but still make the
            // module's file import evidence for name resolution.
            if !imp.local_name.is_empty() || imp.imported_name == "*" {
                bindings_by_file
                    .entry(file_path.clone())
                    .or_default()
                    .push(Binding {
                        local: imp.local_name.clone(),
                        imported: imp.imported_name.clone(),
                        is_module: imp.is_module,
                        target: res.target,
                        resolved_file: res.resolved_file.clone(),
                    });
            }
        }
    }

    index.set_ts_bindings(&bindings_by_file);

    let mut ref_stmt = tx.prepare(
        r#"
        INSERT OR IGNORE INTO unresolved_refs (
            ref_key, from_node_id, reference_name, reference_kind, line, col, candidates,
            file_path, language, receiver, qualifier, status, resolver
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'knobyte-resolver')
        "#,
    )?;
    let empty: Vec<Binding> = Vec::new();

    meta.check_cancel("linking imports")?;
    // 3. Calls.
    if !all_calls.is_empty() {
        if let Some(p) = progress {
            p.set_phase(&format!("Resolving {} call graph edges...", all_calls.len()));
        }
    }
    let total_calls = all_calls.len();
    for (idx, (file_path, call)) in all_calls.iter().enumerate() {
        if idx % 2000 == 0 && idx > 0 {
            if let Some(p) = progress {
                p.set_phase(&format!(
                    "Resolving call graph edges ({}/{})...",
                    idx, total_calls
                ));
            }
        }
        let caller = match index.enclosing_callable(file_path, call.line, call.col) {
            Some(c) => c,
            None => continue,
        };
        let bindings = bindings_by_file.get(file_path).unwrap_or(&empty);
        let source_id = index.metas[caller].id.clone();
        let checked = ts_applied.as_mut().and_then(|a| a.call_target(file_path, call));
        let resolution = match checked {
            Some(target) => CallResolution::Edge {
                target,
                kind: "calls",
                confidence: 1.0,
                method: crate::graph::ts_compiler::PROVENANCE,
            },
            None => index.resolve_call(file_path, call, Some(caller), bindings),
        };
        match resolution {
            CallResolution::Edge {
                target,
                kind,
                confidence,
                method,
            } => {
                let target_meta = &index.metas[target];
                // Calling a type constructs it (`new Foo()`, `Foo()`, `Foo(..)` tuple structs).
                let kind = if kind == "calls"
                    && matches!(target_meta.kind.as_str(), "class" | "struct" | "enum")
                {
                    "instantiates"
                } else {
                    kind
                };
                let target_id = target_meta.id.clone();
                edges.insert(
                    &source_id,
                    &target_id,
                    kind,
                    None,
                    call.line as i64,
                    call.col as i64,
                    confidence,
                    method,
                );
            }
            other => {
                let (status, candidates) = match other {
                    CallResolution::Ambiguous(c) => (
                        "ambiguous",
                        Some(
                            serde_json::to_string(
                                &c.iter()
                                    .map(|&i| index.metas[i].id.clone())
                                    .collect::<Vec<_>>(),
                            )
                            .unwrap_or_default(),
                        ),
                    ),
                    _ => ("unresolved", None),
                };
                let ref_key = format!(
                    "{}:{}:{}:{}:{}",
                    file_path, source_id, call.target_name, call.line, call.col
                );
                let language = file_langs.get(file_path).cloned().unwrap_or_default();
                let _ = ref_stmt.execute(params![
                    ref_key,
                    source_id,
                    call.target_name,
                    "call",
                    call.line as i64,
                    call.col as i64,
                    candidates,
                    file_path,
                    language,
                    call.receiver,
                    call.qualifier,
                    status,
                ]);
            }
        }
    }

    meta.check_cancel("resolving calls")?;
    // 4. Non-call references (extends, implements, instantiates, returns, type_of, decorates,
    // references), resolved conservatively: lexical scope and explicit imports only.
    if !all_refs.is_empty() {
        if let Some(p) = progress {
            p.set_phase(&format!("Resolving {} type & reference edges...", all_refs.len()));
        }
    }
    let mut extends_pairs: Vec<(usize, usize)> = Vec::new();
    let mut typed_edges: Vec<(usize, usize, String, i64, i64)> = Vec::new();
    let mut alias_of: HashMap<usize, usize> = HashMap::new();
    // Checker-resolved type aliases first, so they win over the source resolution of the same
    // pair (edges are de-duplicated per pair and kind).
    for (from, to, line) in ts_applied.as_ref().map(|a| a.aliases.as_slice()).unwrap_or(&[]) {
        alias_of.insert(*from, *to);
        let (src, dst) = (index.metas[*from].id.clone(), index.metas[*to].id.clone());
        edges.insert(&src, &dst, "aliases", None, *line as i64, 0, 1.0, crate::graph::ts_compiler::PROVENANCE);
    }
    for (file_path, r) in &all_refs {
        let bindings = bindings_by_file.get(file_path).unwrap_or(&empty);
        let source = if r.from.is_empty() {
            index.enclosing_callable(file_path, r.line, r.col)
        } else if r.from_kind == "extension" {
            // Swift `extension T: P`: the conformance belongs to `T`, wherever it is declared.
            match index.resolve_named(file_path, &r.from, None, &["class", "struct", "enum", "interface"], bindings, None) {
                CallResolution::Edge { target, .. } => Some(target),
                _ => None,
            }
        } else {
            index.symbol_in_file(file_path, &r.from, &r.from_kind)
        };
        let Some(source) = source else { continue };
        let res = if r.kind == "references" && r.from_kind == "route" {
            // Framework route -> its handler: the route syntax proves the handler name, and only
            // a uniquely named declaration of the same file binds it.
            let framework = r
                .qualifier
                .as_deref()
                .and_then(|q| q.strip_prefix("framework:"))
                .unwrap_or("nextjs");
            match index.callables_in_file(file_path, &r.target_name).as_slice() {
                [t] => CallResolution::Edge {
                    target: *t,
                    kind: "",
                    confidence: 0.8,
                    method: route_handler_method(framework),
                },
                _ => CallResolution::Unresolved,
            }
        } else {
            index.resolve_named(
                file_path,
                &r.target_name,
                r.qualifier.as_deref(),
                ref_target_kinds(&r.kind),
                bindings,
                Some(source),
            )
        };
        let source_id = index.metas[source].id.clone();
        match res {
            CallResolution::Edge {
                target,
                confidence,
                method,
                ..
            } => {
                // A Swift inheritance clause does not say which entry is the superclass: the
                // first entry of a class is emitted as `extends`, and is a conformance when it
                // resolves to a protocol.
                let kind = if r.kind == "extends"
                    && file_path.ends_with(".swift")
                    && index.metas[target].kind == "interface"
                    && index.metas[source].kind != "interface"
                {
                    "implements"
                } else {
                    r.kind.as_str()
                };
                if matches!(kind, "returns" | "type_of" | "extends" | "implements" | "instantiates") {
                    typed_edges.push((source, target, kind.to_string(), r.line as i64, r.col as i64));
                }
                if r.kind == "aliases" && index.metas[source].kind == "type_alias" {
                    alias_of.insert(source, target);
                }
                if kind == "extends" {
                    extends_pairs.push((source, target));
                }
                let target_id = index.metas[target].id.clone();
                edges.insert(
                    &source_id,
                    &target_id,
                    kind,
                    None,
                    r.line as i64,
                    r.col as i64,
                    confidence,
                    method,
                );
            }
            other => {
                // A bare identifier argument is usually a local variable, not a reference.
                if r.kind == "references" {
                    continue;
                }
                let (status, candidates) = match other {
                    CallResolution::Ambiguous(c) => (
                        "ambiguous",
                        Some(
                            serde_json::to_string(
                                &c.iter()
                                    .take(20)
                                    .map(|&i| index.metas[i].id.clone())
                                    .collect::<Vec<_>>(),
                            )
                            .unwrap_or_default(),
                        ),
                    ),
                    _ => ("unresolved", None),
                };
                let ref_key = format!(
                    "{}:{}:{}:{}:{}:{}",
                    file_path, source_id, r.kind, r.target_name, r.line, r.col
                );
                let language = file_langs.get(file_path).cloned().unwrap_or_default();
                let _ = ref_stmt.execute(params![
                    ref_key,
                    source_id,
                    r.target_name,
                    r.kind,
                    r.line as i64,
                    r.col as i64,
                    candidates,
                    file_path,
                    language,
                    Option::<String>::None,
                    r.qualifier,
                    status,
                ]);
            }
        }
    }

    // 4b. Type aliases: a type reference that lands on `type A = B` also reaches `B` (followed
    // through alias chains), as the type checker would see it.
    for (source, target, kind, line, col) in &typed_edges {
        let mut cur = *target;
        let mut hops = 0;
        while let Some(&next) = alias_of.get(&cur) {
            cur = next;
            hops += 1;
            if hops > 4 || cur == *target {
                break;
            }
        }
        if hops > 0 && cur != *target && cur != *source {
            let (src, dst) = (index.metas[*source].id.clone(), index.metas[cur].id.clone());
            edges.insert(&src, &dst, kind, None, *line, *col, 0.9, "type-alias");
        }
    }

    // 5. Overrides: a method of a subclass named like a method of its resolved base class.
    // Swift marks overrides explicitly (`override func`); the extractor records those as
    // `overrides` references instead, so overloads and initializers are not paired by name.
    for (class_idx, base_idx) in extends_pairs {
        if index.metas[class_idx].file_path.ends_with(".swift") {
            continue;
        }
        let class = index.metas[class_idx].clone();
        let base = index.metas[base_idx].clone();
        for (m, bm) in index.override_pairs(&class, &base) {
            let (src, dst) = (index.metas[m].id.clone(), index.metas[bm].id.clone());
            let line = index.metas[m].start_line as i64;
            edges.insert(&src, &dst, "overrides", None, line, 0, 1.0, "inheritance");
        }
    }

    summary.edges_indexed = edges.inserted;
    summary.edges_by_kind = std::mem::take(&mut edges.by_kind);
    drop(edges);
    drop(ref_stmt);
    meta.check_cancel("resolving references")?;

    if let Some(p) = progress {
        p.set_phase("Writing graph candidate...");
    }

    let now_str = chrono::Utc::now().to_rfc3339();
    let root_str = meta
        .root
        .canonicalize()
        .unwrap_or_else(|_| meta.root.to_path_buf())
        .to_string_lossy()
        .to_string();
    let coverage = serde_json::to_string(meta.coverage).unwrap_or_else(|_| "null".into());
    for (k, v) in [
        ("last_build_time", now_str.as_str()),
        ("last_successful_index_at", now_str.as_str()),
        ("project_root", root_str.as_str()),
        ("extractor_version", EXTRACTOR_VERSION),
        ("corpus_policy_hash", meta.policy_hash),
        ("coverage", coverage.as_str()),
        ("build_mode", meta.mode),
        ("rebuild_required", "0"),
        ("typescript_compiler", ts_outcome.mode_key.as_str()),
        ("typescript_compiler_status", ts_outcome.status.as_str()),
    ] {
        tx.execute(
            "INSERT OR REPLACE INTO project_metadata (key, value, updated_at) VALUES (?1, ?2, ?3)",
            params![k, v, indexed_at],
        )?;
    }

    // Provenance of exactly these rows (publication id, git branch / HEAD, versions, source
    // digest); published with them.
    crate::graph::snapshot::write_snapshot(&tx, meta.root, meta.policy_hash)?;

    // Totals use the same definitions as `graph status` (rows in `files`, `nodes`, `edges`).
    let totals = graph_totals(&tx);
    summary.files_indexed = totals.files_indexed;
    summary.nodes_indexed = totals.nodes_indexed;
    summary.symbols_indexed = totals.symbols_indexed;
    summary.edges_indexed = totals.edges_indexed;
    tx.commit()?;
    summary.duration_ms = start_time.elapsed().as_millis();
    Ok(summary)
}

/// Totals of a graph with the same semantics as `graph status`: `files_indexed` counts every
/// indexed file (including files without declarations), `nodes_indexed` every node (including
/// per-file nodes), `symbols_indexed` only declarations, `edges_indexed` every edge.
pub(crate) fn graph_totals(conn: &Connection) -> BuildSummary {
    let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0).max(0) as usize;
    BuildSummary {
        files_indexed: count("SELECT COUNT(*) FROM files"),
        nodes_indexed: count("SELECT COUNT(*) FROM nodes"),
        symbols_indexed: count("SELECT COUNT(*) FROM nodes WHERE kind NOT IN ('file', 'module')"),
        edges_indexed: count("SELECT COUNT(*) FROM edges"),
        ..Default::default()
    }
}

/// Value of a `project_metadata` key.
pub(crate) fn metadata_value(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM project_metadata WHERE key = ?1",
        params![key],
        |r| r.get(0),
    )
    .optional()
    .ok()
    .flatten()
}
