use ignore::WalkBuilder;
use rusqlite::{params, Connection, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::graph::extractor::extract_file;
use crate::graph::fingerprint::{
    compute_body_hash, compute_file_hash, compute_node_id, generate_minhash,
};
use crate::graph::models::{GraphStatus, Node, ScopedNode};
use crate::graph::schema::initialize_graph_schema;
use crate::progress::IndexProgressBar;

#[derive(Debug, Clone)]
struct NodeMeta {
    id: String,
    kind: String,
    name: String,
    qualified_name: String,
    file_path: String,
}

#[derive(Debug, Clone)]
pub struct IndexableFile {
    pub path: PathBuf,
    pub rel_path: String,
    pub size: u64,
}

pub fn scan_indexable_files(root: &Path) -> (Vec<IndexableFile>, u64) {
    let walker = WalkBuilder::new(root)
        .hidden(true)
        .parents(false)
        .git_ignore(true)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !(name == "target"
                || name == "node_modules"
                || name == ".git"
                || name == ".knobyte"
                || name == "dist"
                || name == "reference")
        })
        .build();

    let mut files = Vec::new();
    let mut total_bytes = 0u64;

    for result in walker {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        if !crate::graph::extractor::is_supported_path(path) {
            continue;
        }

        let rel_path = match path.strip_prefix(root) {
            Ok(p) => p.to_string_lossy().to_string(),
            Err(_) => path.to_string_lossy().to_string(),
        };

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        total_bytes += size;
        files.push(IndexableFile {
            path: path.to_path_buf(),
            rel_path,
            size,
        });
    }

    (files, total_bytes)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BuildSummary {
    pub files_indexed: usize,
    pub nodes_indexed: usize,
    pub edges_indexed: usize,
    pub duration_ms: u128,
}

pub struct GraphEngine {
    conn: Connection,
}

impl GraphEngine {
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let conn = Connection::open(db_path)?;
        initialize_graph_schema(&conn)?;
        Ok(Self { conn })
    }

    pub fn status(&self) -> Result<GraphStatus> {
        let node_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap_or(0);
        let edge_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
            .unwrap_or(0);
        let file_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap_or(0);
        let unresolved_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM unresolved_refs", [], |r| r.get(0))
            .unwrap_or(0);
        let schema_version: i64 = self
            .conn
            .query_row("SELECT MAX(version) FROM schema_versions", [], |r| r.get(0))
            .unwrap_or(0);

        let last_indexed: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM project_metadata WHERE key = 'last_build_time'",
                [],
                |r| r.get(0),
            )
            .ok();

        Ok(GraphStatus {
            up_to_date: true,
            schema_version,
            file_count,
            node_count,
            edge_count,
            unresolved_count,
            last_indexed,
        })
    }

    pub fn rebuild(&mut self, root: &Path) -> Result<BuildSummary> {
        self.rebuild_with_progress(root, None)
    }

    pub fn rebuild_with_progress(
        &mut self,
        root: &Path,
        progress: Option<&IndexProgressBar>,
    ) -> Result<BuildSummary> {
        let (files, _) = scan_indexable_files(root);
        self.rebuild_files(root, &files, progress)
    }

    pub fn rebuild_files(
        &mut self,
        root: &Path,
        files: &[IndexableFile],
        progress: Option<&IndexProgressBar>,
    ) -> Result<BuildSummary> {
        self.conn.execute_batch(
            r#"
            DELETE FROM edges;
            DELETE FROM nodes;
            DELETE FROM files;
            DELETE FROM unresolved_refs;
            DELETE FROM import_bindings;
            DELETE FROM node_fingerprints;
            "#,
        )?;
        self.build_files(root, files, progress)
    }

    pub fn build(&mut self, root: &Path) -> Result<BuildSummary> {
        self.build_with_progress(root, None)
    }

    pub fn build_with_progress(
        &mut self,
        root: &Path,
        progress: Option<&IndexProgressBar>,
    ) -> Result<BuildSummary> {
        let (files, _) = scan_indexable_files(root);
        self.build_files(root, &files, progress)
    }

    pub fn build_files(
        &mut self,
        _root: &Path,
        files: &[IndexableFile],
        progress: Option<&IndexProgressBar>,
    ) -> Result<BuildSummary> {
        let start_time = std::time::Instant::now();

        let mut files_indexed = 0;
        let mut nodes_indexed = 0;
        let mut edges_indexed = 0;

        let tx = self.conn.transaction()?;

        let mut all_calls = Vec::new();
        let mut all_imports = Vec::new();
        let mut all_trait_impls = Vec::new();
        let mut all_node_metas = Vec::new();

        for file in files {
            let path = &file.path;
            let rel_path = &file.rel_path;
            let file_size = file.size;

            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => {
                    if let Some(p) = progress {
                        p.inc_file(rel_path, file_size, 0);
                    }
                    continue;
                }
            };

            let extraction = match extract_file(rel_path, &content) {
                Some(e) => e,
                None => {
                    if let Some(p) = progress {
                        p.inc_file(rel_path, file_size, 0);
                    }
                    continue;
                }
            };

            let file_hash = compute_file_hash(content.as_bytes());
            let modified_at = fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64
                })
                .unwrap_or(0);
            let indexed_at = chrono::Utc::now().timestamp_millis();

            let file_node_count = extraction.symbols.len() as i64;

            tx.execute(
                r#"
                INSERT OR REPLACE INTO files (
                    path, content_hash, language, size, modified_at, indexed_at, node_count, parse_status
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'ok')
                "#,
                params![
                    rel_path,
                    file_hash,
                    extraction.language,
                    content.len() as i64,
                    modified_at,
                    indexed_at,
                    file_node_count,
                ],
            )?;

            files_indexed += 1;

            let sym_count = extraction.symbols.len();
            for sym in extraction.symbols {
                let node_id = compute_node_id(rel_path, &sym.kind, &sym.qualified_name);
                let body_hash = compute_body_hash(&sym.body);
                let identity_key = format!("{}:{}:{}", rel_path, sym.kind, sym.qualified_name);

                tx.execute(
                    r#"
                    INSERT OR REPLACE INTO nodes (
                        id, kind, name, qualified_name, identity_key, file_path, language,
                        start_line, end_line, start_column, end_column, docstring, signature,
                        is_exported, is_async, body_hash, updated_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                    "#,
                    params![
                        node_id,
                        sym.kind,
                        sym.name,
                        sym.qualified_name,
                        identity_key,
                        rel_path,
                        extraction.language,
                        sym.start_line as i64,
                        sym.end_line as i64,
                        sym.start_col as i64,
                        sym.end_col as i64,
                        sym.docstring,
                        sym.signature,
                        if sym.is_exported { 1 } else { 0 },
                        if sym.is_async { 1 } else { 0 },
                        body_hash,
                        indexed_at,
                    ],
                )?;

                // Save fingerprint
                let minhash = generate_minhash(&sym.body);
                let _ = tx.execute(
                    r#"
                    INSERT OR REPLACE INTO node_fingerprints (
                        node_id, minhash, neighbors, token_count
                    ) VALUES (?1, ?2, '[]', ?3)
                    "#,
                    params![node_id, minhash, sym.body.split_whitespace().count() as i64,],
                );

                all_node_metas.push(NodeMeta {
                    id: node_id.clone(),
                    kind: sym.kind.clone(),
                    name: sym.name.clone(),
                    qualified_name: sym.qualified_name.clone(),
                    file_path: rel_path.to_string(),
                });

                nodes_indexed += 1;
            }

            for call in extraction.calls {
                all_calls.push((rel_path.clone(), call));
            }
            for imp in extraction.imports {
                all_imports.push((rel_path.clone(), imp));
            }
            for ti in extraction.trait_impls {
                all_trait_impls.push((rel_path.clone(), ti));
            }

            if let Some(p) = progress {
                p.inc_file(rel_path, file_size, sym_count);
            }
        }

        // Fast in-memory lookup structures for O(1) relational resolution
        let mut nodes_by_file_and_name: HashMap<(String, String), String> = HashMap::new();
        let mut nodes_by_base_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut nodes_by_qual_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut first_node_by_file: HashMap<String, String> = HashMap::new();

        for (idx, n) in all_node_metas.iter().enumerate() {
            nodes_by_file_and_name.insert((n.file_path.clone(), n.name.clone()), n.id.clone());
            nodes_by_file_and_name.insert((n.file_path.clone(), n.qualified_name.clone()), n.id.clone());

            nodes_by_base_name.entry(n.name.clone()).or_default().push(idx);
            nodes_by_qual_name.entry(n.qualified_name.clone()).or_default().push(idx);

            first_node_by_file.entry(n.file_path.clone()).or_insert_with(|| n.id.clone());
        }

        // 1. Process trait implementations (implements and impl_of edges)
        if !all_trait_impls.is_empty() {
            if let Some(p) = progress {
                p.set_phase("Resolving trait implementations & method dispatch...");
            }
        }
        {
            let mut edge_stmt = tx.prepare(
                "INSERT OR IGNORE INTO edges (source, target, kind, line, col, confidence) VALUES (?1, ?2, ?3, ?4, 0, 1.0)",
            )?;

            for (file_path, ti) in &all_trait_impls {
                let trait_node_id = nodes_by_qual_name.get(&ti.trait_name)
                    .or_else(|| nodes_by_base_name.get(&ti.trait_name))
                    .and_then(|idxs| idxs.iter().find(|&&i| all_node_metas[i].kind == "trait"))
                    .map(|&i| all_node_metas[i].id.clone());

                let type_node_id = nodes_by_qual_name.get(&ti.type_name)
                    .or_else(|| nodes_by_base_name.get(&ti.type_name))
                    .and_then(|idxs| idxs.iter().find(|&&i| {
                        let k = &all_node_metas[i].kind;
                        k == "struct" || k == "enum" || k == "class"
                    }))
                    .map(|&i| all_node_metas[i].id.clone());

                if let (Some(ref type_id), Some(ref trait_id)) = (&type_node_id, &trait_node_id) {
                    let _ = edge_stmt.execute(params![type_id, trait_id, "implements", ti.line as i64]);
                    edges_indexed += 1;
                }

                if let Some(ref _trait_id) = trait_node_id {
                    let prefix = format!("{}::", ti.trait_name);
                    let trait_methods: Vec<(String, String)> = all_node_metas.iter()
                        .filter(|n| (n.kind == "function" || n.kind == "method") && (n.qualified_name.starts_with(&prefix) || n.name == ti.trait_name))
                        .map(|n| (n.id.clone(), n.name.clone()))
                        .collect();

                    let type_prefix = format!("{}::", ti.type_name);
                    for (t_m_id, m_name) in trait_methods {
                        let type_m_id = nodes_by_file_and_name.get(&(file_path.clone(), m_name.clone()))
                            .cloned()
                            .or_else(|| {
                                all_node_metas.iter().find(|n| {
                                    n.file_path == *file_path
                                        && n.name == m_name
                                        && (n.qualified_name.starts_with(&type_prefix) || n.qualified_name == m_name)
                                }).map(|n| n.id.clone())
                            });

                        if let Some(impl_m_id) = type_m_id {
                            let _ = edge_stmt.execute(params![impl_m_id, t_m_id, "impl_of", ti.line as i64]);
                            edges_indexed += 1;
                        }
                    }
                }
            }
        }

        // 2. Process calls
        if !all_calls.is_empty() {
            if let Some(p) = progress {
                p.set_phase(&format!("Resolving {} call graph edges...", all_calls.len()));
            }
        }
        {
            let mut edge_stmt = tx.prepare(
                "INSERT OR IGNORE INTO edges (source, target, kind, line, col, confidence) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            let mut ref_stmt = tx.prepare(
                "INSERT OR IGNORE INTO unresolved_refs (ref_key, from_node_id, reference_name, reference_kind, line, col, file_path, receiver, status) VALUES (?1, ?2, ?3, 'call', ?4, ?5, ?6, ?7, 'unresolved')",
            )?;

            let total_calls = all_calls.len();
            for (idx, (file_path, call)) in all_calls.into_iter().enumerate() {
                if idx % 2000 == 0 && idx > 0 {
                    if let Some(p) = progress {
                        p.set_phase(&format!("Resolving call graph edges ({}/{})...", idx, total_calls));
                    }
                }

                let caller_base = call
                    .caller_name
                    .split("::")
                    .last()
                    .unwrap_or(&call.caller_name);
                let caller_node_id = nodes_by_file_and_name
                    .get(&(file_path.clone(), caller_base.to_string()))
                    .or_else(|| nodes_by_file_and_name.get(&(file_path.clone(), call.caller_name.clone())))
                    .cloned()
                    .or_else(|| {
                        let pat = format!("::{}", caller_base);
                        all_node_metas
                            .iter()
                            .find(|n| n.file_path == file_path && n.qualified_name.ends_with(&pat))
                            .map(|n| n.id.clone())
                    });

                let candidates: Vec<(String, String, String)> = if call.target_name.contains("::") {
                    let target_base = call
                        .target_name
                        .split("::")
                        .last()
                        .unwrap_or(&call.target_name);
                    let pat_suffix = format!("::{}", call.target_name);
                    nodes_by_base_name
                        .get(target_base)
                        .map(|idxs| {
                            idxs.iter()
                                .filter_map(|&i| {
                                    let n = &all_node_metas[i];
                                    if n.name == call.target_name
                                        || n.qualified_name == call.target_name
                                        || n.qualified_name.ends_with(&pat_suffix)
                                    {
                                        Some((n.id.clone(), n.kind.clone(), n.qualified_name.clone()))
                                    } else {
                                        None
                                    }
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                } else {
                    nodes_by_base_name
                        .get(&call.target_name)
                        .map(|idxs| {
                            idxs.iter()
                                .map(|&i| {
                                    let n = &all_node_metas[i];
                                    (n.id.clone(), n.kind.clone(), n.qualified_name.clone())
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                };

                match (caller_node_id, candidates.is_empty()) {
                    (Some(source), false) => {
                        let candidate_count = candidates.len();
                        for (target_id, kind, qual) in candidates {
                            let (edge_kind, conf) =
                                if kind == "trait" || qual.contains("::") && kind == "method" {
                                    ("calls_trait_method", 0.9)
                                } else if candidate_count == 1 {
                                    ("calls", 1.0)
                                } else {
                                    ("calls", 0.75)
                                };
                            let _ = edge_stmt.execute(params![
                                source,
                                target_id,
                                edge_kind,
                                call.line as i64,
                                call.col as i64,
                                conf
                            ]);
                            edges_indexed += 1;
                        }
                    }
                    (Some(source), true) => {
                        let ref_key = format!(
                            "{}:{}:{}:{}:{}",
                            file_path, source, call.target_name, call.line, call.col
                        );
                        let _ = ref_stmt.execute(params![
                            ref_key,
                            source,
                            call.target_name,
                            call.line as i64,
                            call.col as i64,
                            file_path,
                            call.receiver
                        ]);
                    }
                    _ => {}
                }
            }
        }

        // 3. Process imports
        if !all_imports.is_empty() {
            if let Some(p) = progress {
                p.set_phase(&format!("Linking {} module imports...", all_imports.len()));
            }
        }
        {
            let mut import_edge_stmt = tx.prepare(
                "INSERT OR IGNORE INTO edges (source, target, kind, metadata, line, col, confidence) VALUES (?1, ?2, 'imports', ?3, ?4, 0, 1.0)",
            )?;
            for (file_path, imp) in all_imports {
                let mod_name = imp
                    .source_module
                    .trim_start_matches("use ")
                    .trim_end_matches(';')
                    .split("::")
                    .next()
                    .unwrap_or("")
                    .trim();
                if mod_name.is_empty() {
                    continue;
                }

                let source_id = first_node_by_file.get(&file_path).cloned();

                if let Some(src) = source_id {
                    let target_id = nodes_by_base_name
                        .get(mod_name)
                        .or_else(|| nodes_by_qual_name.get(mod_name))
                        .and_then(|idxs| idxs.first().map(|&i| all_node_metas[i].id.clone()))
                        .or_else(|| {
                            all_node_metas
                                .iter()
                                .find(|n| n.file_path.contains(mod_name))
                                .map(|n| n.id.clone())
                        });

                    let target_node_id = match target_id {
                        Some(id) => id,
                        None => {
                            let sub_name = mod_name.split('_').next_back().unwrap_or(mod_name);
                            let sub_target = nodes_by_base_name
                                .get(sub_name)
                                .and_then(|idxs| idxs.first().map(|&i| all_node_metas[i].id.clone()))
                                .or_else(|| {
                                    all_node_metas
                                        .iter()
                                        .find(|n| n.file_path.contains(sub_name))
                                        .map(|n| n.id.clone())
                                });

                            match sub_target {
                                Some(id) => id,
                                None => {
                                    let node_id = compute_node_id("", "module", mod_name);
                                    let _ = tx.execute(
                                        r#"
                                        INSERT OR IGNORE INTO nodes (
                                            id, kind, name, qualified_name, identity_key, file_path, language,
                                            start_line, end_line, start_column, end_column, docstring, signature,
                                            is_exported, is_async, body_hash, updated_at
                                        ) VALUES (?1, 'module', ?2, ?2, ?1, '', 'rust', 0, 0, 0, 0, NULL, ?2, 1, 0, '', ?3)
                                        "#,
                                        params![node_id, mod_name, chrono::Utc::now().timestamp_millis()],
                                    );
                                    node_id
                                }
                            }
                        }
                    };

                    let _ = import_edge_stmt.execute(params![
                        src,
                        target_node_id,
                        imp.source_module,
                        imp.line as i64
                    ]);
                    edges_indexed += 1;
                }
            }
        }

        if let Some(p) = progress {
            p.set_phase("Committing graph database...");
        }

        let now_str = chrono::Utc::now().to_rfc3339();
        tx.execute(
            "INSERT OR REPLACE INTO project_metadata (key, value, updated_at) VALUES ('last_build_time', ?1, ?2)",
            params![now_str, chrono::Utc::now().timestamp_millis()],
        )?;

        tx.commit()?;

        Ok(BuildSummary {
            files_indexed,
            nodes_indexed,
            edges_indexed,
            duration_ms: start_time.elapsed().as_millis(),
        })
    }

    pub fn query_where_defined(&self, name: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT id, kind, name, qualified_name, container_id, identity_key, file_path, language,
                   start_line, end_line, start_column, end_column, docstring, signature, visibility,
                   is_exported, is_async, is_static, is_abstract, return_type, body_hash, updated_at
            FROM nodes
            WHERE name = ?1 OR qualified_name = ?1 OR qualified_name LIKE ?2
            ORDER BY file_path, start_line
            "#,
        )?;

        let pattern = format!("%::{}", name);
        let rows = stmt.query_map(params![name, pattern], Self::map_node)?;

        let mut nodes = Vec::new();
        for r in rows {
            nodes.push(r?);
        }
        Ok(nodes)
    }

    pub fn query_who_calls(&self, target_name: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT * FROM (
                SELECT n.id, n.kind, n.name, n.qualified_name, n.container_id, n.identity_key, n.file_path, n.language,
                       n.start_line, n.end_line, n.start_column, n.end_column, n.docstring, n.signature, n.visibility,
                       n.is_exported, n.is_async, n.is_static, n.is_abstract, n.return_type, n.body_hash, n.updated_at
                FROM nodes n
                JOIN edges e ON n.id = e.source
                JOIN nodes t ON e.target = t.id
                WHERE (t.name = ?1 OR t.qualified_name = ?1 OR t.qualified_name LIKE ('%::' || ?1) OR t.qualified_name LIKE ('%.' || ?1))
                  AND e.kind IN ('calls', 'calls_trait_method', 'possible_call')
                UNION
                SELECT n.id, n.kind, n.name, n.qualified_name, n.container_id, n.identity_key, n.file_path, n.language,
                       n.start_line, n.end_line, n.start_column, n.end_column, n.docstring, n.signature, n.visibility,
                       n.is_exported, n.is_async, n.is_static, n.is_abstract, n.return_type, n.body_hash, n.updated_at
                FROM nodes n
                JOIN unresolved_refs u ON n.id = u.from_node_id
                WHERE u.reference_name = ?1 OR u.reference_name LIKE ('%.' || ?1) OR u.reference_name LIKE ('%::' || ?1)
            )
            ORDER BY file_path, start_line
            "#,
        )?;

        let rows = stmt.query_map(params![target_name], Self::map_node)?;
        let mut nodes = Vec::new();
        for r in rows {
            nodes.push(r?);
        }
        Ok(nodes)
    }

    pub fn query_who_imports(&self, target_path: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT DISTINCT n.id, n.kind, n.name, n.qualified_name, n.container_id, n.identity_key, n.file_path, n.language,
                   n.start_line, n.end_line, n.start_column, n.end_column, n.docstring, n.signature, n.visibility,
                   n.is_exported, n.is_async, n.is_static, n.is_abstract, n.return_type, n.body_hash, n.updated_at
            FROM nodes n
            JOIN edges e ON n.id = e.source
            JOIN nodes t ON e.target = t.id
            WHERE (t.file_path LIKE ?1 OR t.file_path = ?2 OR t.name LIKE ?1 OR e.metadata LIKE ?1) AND e.kind = 'imports'
            ORDER BY n.file_path, n.start_line
            "#,
        )?;

        let pattern = format!("%{}%", target_path);
        let rows = stmt.query_map(params![pattern, target_path], Self::map_node)?;
        let mut nodes = Vec::new();
        for r in rows {
            nodes.push(r?);
        }
        Ok(nodes)
    }

    pub fn query_scope_explained(&self, task: &str) -> Result<Vec<ScopedNode>> {
        let words: Vec<String> = task
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|w| w.len() >= 3)
            .map(|w| w.to_lowercase())
            .collect();

        if words.is_empty() {
            return Ok(Vec::new());
        }

        let mut scored_nodes: HashMap<String, (Node, f64, Vec<String>)> = HashMap::new();

        // 1. Lexical search across nodes table
        for word in &words {
            let pat = format!("%{}%", word);
            let mut stmt = self.conn.prepare(
                r#"
                SELECT id, kind, name, qualified_name, container_id, identity_key, file_path, language,
                       start_line, end_line, start_column, end_column, docstring, signature, visibility,
                       is_exported, is_async, is_static, is_abstract, return_type, body_hash, updated_at
                FROM nodes
                WHERE LOWER(name) LIKE ?1 OR LOWER(qualified_name) LIKE ?1
                LIMIT 20
                "#,
            )?;
            let rows = stmt.query_map(params![pat], Self::map_node)?;
            for r in rows.flatten() {
                let is_exact = r.name.to_lowercase() == *word;
                let weight = if is_exact { 10.0 } else { 4.0 };
                let reason = if is_exact {
                    format!("defines `{}`", r.name)
                } else {
                    format!("matches term `{}`", word)
                };
                let entry = scored_nodes
                    .entry(r.id.clone())
                    .or_insert_with(|| (r, 0.0, Vec::new()));
                entry.1 += weight;
                if !entry.2.contains(&reason) {
                    entry.2.push(reason);
                }
            }
        }

        // FTS search if available
        let fts_query: String = words
            .iter()
            .map(|w| format!("\"{}\"*", w))
            .collect::<Vec<_>>()
            .join(" OR ");
        if let Ok(mut fts_stmt) = self.conn.prepare(
            r#"
            SELECT n.id, n.kind, n.name, n.qualified_name, n.container_id, n.identity_key, n.file_path, n.language,
                   n.start_line, n.end_line, n.start_column, n.end_column, n.docstring, n.signature, n.visibility,
                   n.is_exported, n.is_async, n.is_static, n.is_abstract, n.return_type, n.body_hash, n.updated_at
            FROM nodes n
            JOIN nodes_fts f ON n.id = f.id
            WHERE nodes_fts MATCH ?1
            LIMIT 20
            "#,
        ) {
            if let Ok(rows) = fts_stmt.query_map(params![fts_query], Self::map_node) {
                for r in rows.flatten() {
                    let entry = scored_nodes.entry(r.id.clone()).or_insert_with(|| (r, 0.0, Vec::new()));
                    entry.1 += 3.0;
                    let reason = "matched full-text search".to_string();
                    if !entry.2.contains(&reason) {
                        entry.2.push(reason);
                    }
                }
            }
        }

        if scored_nodes.is_empty() {
            return Ok(Vec::new());
        }

        // 2. Graph expansion (callers, callees, trait implementors, co-located symbols)
        let seed_ids: Vec<String> = scored_nodes.keys().cloned().collect();
        for seed_id in seed_ids {
            let seed_name = scored_nodes
                .get(&seed_id)
                .map(|(n, _, _)| n.name.clone())
                .unwrap_or_default();
            // Callers
            let mut caller_stmt = self.conn.prepare(
                r#"
                SELECT n.id, n.kind, n.name, n.qualified_name, n.container_id, n.identity_key, n.file_path, n.language,
                       n.start_line, n.end_line, n.start_column, n.end_column, n.docstring, n.signature, n.visibility,
                       n.is_exported, n.is_async, n.is_static, n.is_abstract, n.return_type, n.body_hash, n.updated_at
                FROM nodes n
                JOIN edges e ON n.id = e.source
                WHERE e.target = ?1 AND e.kind IN ('calls', 'calls_trait_method', 'possible_call')
                LIMIT 10
                "#,
            )?;
            for r in caller_stmt
                .query_map(params![seed_id], Self::map_node)?
                .flatten()
            {
                let reason = format!("calls `{}`", seed_name);
                let entry = scored_nodes
                    .entry(r.id.clone())
                    .or_insert_with(|| (r, 0.0, Vec::new()));
                entry.1 += 5.0;
                if !entry.2.contains(&reason) {
                    entry.2.push(reason);
                }
            }

            // Implementors / Implemented trait
            let mut impl_stmt = self.conn.prepare(
                r#"
                SELECT n.id, n.kind, n.name, n.qualified_name, n.container_id, n.identity_key, n.file_path, n.language,
                       n.start_line, n.end_line, n.start_column, n.end_column, n.docstring, n.signature, n.visibility,
                       n.is_exported, n.is_async, n.is_static, n.is_abstract, n.return_type, n.body_hash, n.updated_at
                FROM nodes n
                JOIN edges e ON n.id = e.source
                WHERE e.target = ?1 AND e.kind IN ('implements', 'impl_of')
                UNION
                SELECT n.id, n.kind, n.name, n.qualified_name, n.container_id, n.identity_key, n.file_path, n.language,
                       n.start_line, n.end_line, n.start_column, n.end_column, n.docstring, n.signature, n.visibility,
                       n.is_exported, n.is_async, n.is_static, n.is_abstract, n.return_type, n.body_hash, n.updated_at
                FROM nodes n
                JOIN edges e ON n.id = e.target
                WHERE e.source = ?1 AND e.kind IN ('implements', 'impl_of')
                LIMIT 10
                "#,
            )?;
            for r in impl_stmt
                .query_map(params![seed_id], Self::map_node)?
                .flatten()
            {
                let reason = format!("implements or implemented by `{}`", seed_name);
                let entry = scored_nodes
                    .entry(r.id.clone())
                    .or_insert_with(|| (r, 0.0, Vec::new()));
                entry.1 += 6.0;
                if !entry.2.contains(&reason) {
                    entry.2.push(reason);
                }
            }
        }

        let mut results: Vec<ScopedNode> = scored_nodes
            .into_values()
            .map(|(node, score, reasons)| ScopedNode {
                node,
                score,
                reason: reasons.join("; "),
            })
            .collect();

        results.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(25);
        Ok(results)
    }

    pub fn query_scope(&self, task: &str) -> Result<Vec<Node>> {
        let scoped = self.query_scope_explained(task)?;
        Ok(scoped.into_iter().map(|s| s.node).collect())
    }

    pub fn query_impact(&self, target: &str) -> Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            r#"
            WITH RECURSIVE impact_tree(node_id, depth) AS (
                SELECT id, 0 FROM nodes WHERE name = ?1 OR qualified_name = ?1 OR file_path = ?1
                UNION
                SELECT e.source, it.depth + 1
                FROM edges e
                JOIN impact_tree it ON e.target = it.node_id
                WHERE it.depth < 3
            )
            SELECT DISTINCT n.id, n.kind, n.name, n.qualified_name, n.container_id, n.identity_key, n.file_path, n.language,
                   n.start_line, n.end_line, n.start_column, n.end_column, n.docstring, n.signature, n.visibility,
                   n.is_exported, n.is_async, n.is_static, n.is_abstract, n.return_type, n.body_hash, n.updated_at
            FROM nodes n
            JOIN impact_tree it ON n.id = it.node_id
            ORDER BY n.file_path, n.start_line
            "#,
        )?;

        let rows = stmt.query_map(params![target], Self::map_node)?;
        let mut nodes = Vec::new();
        for r in rows {
            nodes.push(r?);
        }
        Ok(nodes)
    }

    pub fn get_nodes(&self, ids: &[String]) -> Result<Vec<Node>> {
        let mut nodes = Vec::new();
        for id in ids {
            if let Ok(node) = self.conn.query_row(
                r#"
                SELECT id, kind, name, qualified_name, container_id, identity_key, file_path, language,
                       start_line, end_line, start_column, end_column, docstring, signature, visibility,
                       is_exported, is_async, is_static, is_abstract, return_type, body_hash, updated_at
                FROM nodes WHERE id = ?1
                "#,
                params![id],
                Self::map_node,
            ) {
                nodes.push(node);
            }
        }
        Ok(nodes)
    }

    pub fn repair(&self) -> Result<()> {
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA integrity_check;")?;
        Ok(())
    }

    pub fn ground_all(&self, root: &Path) -> Result<usize> {
        let mut grounded = 0;
        let mut stmt = self
            .conn
            .prepare("SELECT id, file_path, start_line, end_line, body_hash FROM nodes")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?;

        for r in rows {
            let (node_id, file_path, start_line, end_line, body_hash) = r?;
            let full_path = root.join(&file_path);
            if let Ok(content) = fs::read_to_string(&full_path) {
                let lines: Vec<&str> = content.lines().collect();
                let start_idx = (start_line.saturating_sub(1)) as usize;
                let end_idx = end_line as usize;
                if start_idx < lines.len() && end_idx <= lines.len() && start_idx < end_idx {
                    let source = lines[start_idx..end_idx].join("\n");
                    let hash = body_hash.unwrap_or_else(|| compute_body_hash(&source));
                    let _ = self.conn.execute(
                        r#"
                        INSERT OR REPLACE INTO _knobyte_grounded_source (
                            subject_kind, subject_id, node_id, source, body_hash, fingerprint
                        ) VALUES ('scaffold', ?1, ?2, ?3, ?4, '')
                        "#,
                        params![file_path, node_id, source, hash],
                    );
                    grounded += 1;
                }
            }
        }
        Ok(grounded)
    }

    fn map_node(r: &rusqlite::Row) -> rusqlite::Result<Node> {
        Ok(Node {
            id: r.get(0)?,
            kind: r.get(1)?,
            name: r.get(2)?,
            qualified_name: r.get(3)?,
            container_id: r.get(4)?,
            identity_key: r.get(5)?,
            file_path: r.get(6)?,
            language: r.get(7)?,
            start_line: r.get(8)?,
            end_line: r.get(9)?,
            start_column: r.get(10)?,
            end_column: r.get(11)?,
            docstring: r.get(12)?,
            signature: r.get(13)?,
            visibility: r.get(14)?,
            is_exported: r.get::<_, i64>(15)? == 1,
            is_async: r.get::<_, i64>(16)? == 1,
            is_static: r.get::<_, i64>(17)? == 1,
            is_abstract: r.get::<_, i64>(18)? == 1,
            return_type: r.get(19)?,
            body_hash: r.get(20)?,
            updated_at: r.get(21)?,
        })
    }
}
