//! Setup's indexing pass and its closing proof summary.
//!
//! Indexing runs as one progress display: scan → code graph → vector index (code nodes into
//! Cozo with the configured embedder) → wiki search index (plus the wiki pages' vectors).
//! The summary names what was wired, what was indexed and a real, central symbol to ask the
//! agent about.

use std::io::IsTerminal;
use std::path::Path;
use std::time::Duration;

use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;

use crate::config::KnobyteConfig;

/// Counts from the vector index.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VectorReport {
    pub code_nodes: usize,
    pub wiki_pages: usize,
    /// Embedding backend id (e.g. `hashed-v1`).
    pub backend: String,
    pub dim: usize,
}

/// What the indexing pass produced.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexReport {
    pub scanned_files: usize,
    pub scanned_bytes: u64,
    pub graph_built: bool,
    pub files: usize,
    pub symbols: usize,
    pub edges: usize,
    pub vector: Option<VectorReport>,
    /// Why the vector index is unavailable, if it is.
    pub vector_error: Option<String>,
    pub wiki_entities: usize,
}

fn spinner(enabled: bool) -> ProgressBar {
    if !enabled {
        return ProgressBar::hidden();
    }
    let bar = ProgressBar::new_spinner();
    bar.set_style(
        ProgressStyle::default_spinner()
            .template("  {spinner:.cyan.bold} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner())
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ "),
    );
    bar.enable_steady_tick(Duration::from_millis(80));
    bar
}

fn step_line(bar: &ProgressBar, n: usize, label: &str, detail: &str) {
    bar.suspend(|| println!("  {} {:<13} {}", format!("[{}/4]", n).green().bold(), label, detail));
}

fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Re-embed the wiki pages into the vector index (after the wiki index changed).
pub fn sync_wiki_vectors(config: &KnobyteConfig) -> Result<usize, String> {
    let engine = crate::cozo::CozoEngine::open_configured(config).map_err(|e| e.to_string())?;
    let conn = rusqlite::Connection::open(config.wiki_db_path()).map_err(|e| e.to_string())?;
    engine.sync_from_wiki(&conn).map_err(|e| e.to_string())
}

/// Run the indexing pass. `code` is false in agent-memory workspaces; `skip_graph` keeps the
/// code graph (and code vectors) out.
pub fn run_indexing(config: &KnobyteConfig, code: bool, skip_graph: bool) -> Result<IndexReport, String> {
    use crate::graph::{rebuild_graph, scan_corpus, CorpusPolicy};
    let tty = std::io::stderr().is_terminal();
    let mut report = IndexReport::default();
    let root = &config.project_root;
    println!("\n{}", "Indexing".bold());
    let bar = spinner(tty);

    // 1. Scan.
    bar.set_message("Scanning the repository...");
    let scan = if code {
        let scan = scan_corpus(root, &CorpusPolicy::for_project(root)).map_err(|e| e.to_string())?;
        report.scanned_files = scan.files.len();
        report.scanned_bytes = scan.total_bytes;
        step_line(
            &bar,
            1,
            "scan",
            &format!(
                "{} source file{} ({})",
                thousands(scan.files.len()),
                if scan.files.len() == 1 { "" } else { "s" },
                crate::progress::format_bytes(scan.total_bytes)
            ),
        );
        Some(scan)
    } else {
        step_line(&bar, 1, "scan", "skipped (agent-memory workspace)");
        None
    };

    // 2. Code graph.
    match (&scan, skip_graph) {
        (Some(scan), false) => {
            bar.set_message("Building the code graph...");
            bar.suspend(|| {
                let progress = crate::progress::IndexProgressBar::new(scan.total_bytes, scan.files.len(), tty);
                let res = rebuild_graph(&config.graph_db_path(), root, scan, Some(&progress));
                progress.finish_and_clear();
                res
            })
            .map(|outcome| {
                report.graph_built = true;
                report.files = outcome.summary.files_indexed;
                report.symbols = outcome.summary.symbols_indexed;
                report.edges = outcome.summary.edges_indexed;
            })
            .map_err(|e| format!("Code graph setup failed: {}. Fix the problem and rerun knobyte setup.", e))?;
            step_line(
                &bar,
                2,
                "code graph",
                &format!("{} files, {} symbols, {} edges", thousands(report.files), thousands(report.symbols), thousands(report.edges)),
            );
        }
        (Some(_), true) => step_line(&bar, 2, "code graph", "skipped (--skip-graph)"),
        (None, _) => step_line(&bar, 2, "code graph", "skipped (agent-memory workspace)"),
    }

    // 3. Vector index (code).
    bar.set_message("Embedding code into the vector index...");
    let engine = match crate::cozo::CozoEngine::open_configured(config) {
        Ok(e) => Some(e),
        Err(e) => {
            report.vector_error = Some(e.to_string());
            None
        }
    };
    if let Some(engine) = &engine {
        let mut vector = VectorReport { backend: engine.embedder().id().to_string(), dim: engine.embedder().dim(), ..Default::default() };
        if report.graph_built {
            match rusqlite::Connection::open(config.graph_db_path()).map_err(|e| e.to_string()).and_then(|c| {
                engine.sync_from_graph(&c).map_err(|e| e.to_string())
            }) {
                Ok((nodes, _)) => vector.code_nodes = nodes,
                Err(e) => report.vector_error = Some(e),
            }
        }
        step_line(
            &bar,
            3,
            "vector index",
            &match &report.vector_error {
                Some(e) => format!("unavailable: {}", e),
                None => format!("{} code nodes ({}, {}-dim)", thousands(vector.code_nodes), vector.backend, vector.dim),
            },
        );
        report.vector = Some(vector);
    } else {
        step_line(&bar, 3, "vector index", &format!("unavailable: {}", report.vector_error.as_deref().unwrap_or("unknown error")));
    }

    // 4. Wiki search index (+ wiki vectors).
    bar.set_message("Indexing the wiki...");
    let rebuilt = crate::wiki::WikiIndex::open_for_rebuild(&config.wiki_db_path()).and_then(|mut i| i.rebuild(&config.scaffold_root));
    match rebuilt {
        Ok(n) => report.wiki_entities = n,
        Err(e) => {
            bar.finish_and_clear();
            return Err(format!("the wiki index rebuild failed: {}", e));
        }
    }
    if let (Some(engine), Some(vector)) = (&engine, report.vector.as_mut()) {
        if let Ok(conn) = rusqlite::Connection::open(config.wiki_db_path()) {
            if let Ok(n) = engine.sync_from_wiki(&conn) {
                vector.wiki_pages = n;
            }
        }
    }
    step_line(&bar, 4, "wiki index", &format!("{} entities", thousands(report.wiki_entities)));
    bar.finish_and_clear();
    Ok(report)
}

/// The index counts as they stand, without rebuilding anything (wiki vectors are re-synced so
/// freshly populated docs are searchable).
pub fn current_index_report(config: &KnobyteConfig) -> IndexReport {
    let mut report = IndexReport::default();
    if let Ok(conn) = rusqlite::Connection::open_with_flags(config.graph_db_path(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) {
        let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0)).map(|n| n as usize);
        if let (Ok(files), Ok(symbols), Ok(edges), Ok(nodes)) = (
            count("SELECT COUNT(*) FROM files"),
            count("SELECT COUNT(*) FROM nodes WHERE kind NOT IN ('file', 'module')"),
            count("SELECT COUNT(*) FROM edges"),
            count("SELECT COUNT(*) FROM nodes"),
        ) {
            report.graph_built = true;
            report.files = files;
            report.symbols = symbols;
            report.edges = edges;
            report.scanned_files = files;
            if let Ok(engine) = crate::cozo::CozoEngine::open_configured(config) {
                report.vector = Some(VectorReport {
                    code_nodes: nodes,
                    wiki_pages: 0,
                    backend: engine.embedder().id().to_string(),
                    dim: engine.embedder().dim(),
                });
            }
        }
    }
    if report.vector.is_none() {
        if let Ok(engine) = crate::cozo::CozoEngine::open_configured(config) {
            report.vector =
                Some(VectorReport { backend: engine.embedder().id().to_string(), dim: engine.embedder().dim(), ..Default::default() });
        }
    }
    match sync_wiki_vectors(config) {
        Ok(n) => {
            if let Some(v) = report.vector.as_mut() {
                v.wiki_pages = n;
            }
        }
        Err(e) => report.vector_error = Some(e),
    }
    report.wiki_entities = crate::wiki::WikiIndex::open_read_only(&config.wiki_db_path())
        .and_then(|i| i.entity_count())
        .unwrap_or(0);
    report
}

/// A central symbol to ask about: the declaration used from the most other files in the
/// repository's main language (tests excluded), else its largest source file.
pub fn central_symbol(graph_db: &Path) -> Option<CentralSymbol> {
    if !graph_db.exists() {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(graph_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    const NOT_TEST: &str = "lower(file_path) NOT LIKE '%test%' AND lower(file_path) NOT LIKE '%spec%' AND lower(file_path) NOT LIKE '%fixture%' AND lower(file_path) NOT LIKE '%example%'";
    let lang: String = conn
        .query_row(
            &format!(
                "SELECT language FROM files WHERE {} GROUP BY language ORDER BY count(*) DESC, language LIMIT 1",
                NOT_TEST.replace("file_path", "path")
            ),
            [],
            |r| r.get(0),
        )
        .ok()?;
    let sql = format!(
        "SELECT n.name, n.file_path, count(DISTINCT s.file_path) AS users
         FROM edges e JOIN nodes n ON n.id = e.target JOIN nodes s ON s.id = e.source
         WHERE e.kind IN ('calls','calls_trait_method','references','instantiates','implements','extends','type_of','returns')
           AND n.kind IN ('function','method','struct','class','trait','interface','enum','type_alias')
           AND n.language = ?1 AND length(n.name) >= 4 AND lower(n.name) NOT LIKE 'test%'
           AND s.file_path != n.file_path AND {}
         GROUP BY n.id ORDER BY users DESC, n.name LIMIT 1",
        NOT_TEST.replace("file_path", "n.file_path")
    );
    if let Ok((name, file, users)) = conn.query_row(&sql, [&lang], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))) {
        if users > 0 {
            return Some(CentralSymbol { name, file, used_from_files: users as usize });
        }
    }
    // No cross-file use yet: the largest declaration.
    let largest = format!(
        "SELECT name, file_path FROM nodes
         WHERE kind IN ('function','method','struct','class','trait','interface','enum','type_alias')
           AND language = ?1 AND length(name) >= 2 AND lower(name) NOT LIKE 'test%' AND {}
         ORDER BY (end_line - start_line) DESC, name LIMIT 1",
        NOT_TEST
    );
    if let Ok((name, file)) = conn.query_row(&largest, [&lang], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
        return Some(CentralSymbol { name, file, used_from_files: 0 });
    }
    let file: String = conn
        .query_row(
            &format!(
                "SELECT path FROM files WHERE language = ?1 AND {} ORDER BY node_count DESC, path LIMIT 1",
                NOT_TEST.replace("file_path", "path")
            ),
            [&lang],
            |r| r.get(0),
        )
        .ok()?;
    Some(CentralSymbol { name: file.clone(), file, used_from_files: 0 })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CentralSymbol {
    pub name: String,
    pub file: String,
    pub used_from_files: usize,
}

/// One wired tool.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolWiring {
    pub tool: String,
    pub name: String,
    /// Instruction files and MCP files that point the tool at Knobyte.
    pub files: Vec<String>,
}

/// The proof printed at the end of setup.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupSummary {
    pub tools: Vec<ToolWiring>,
    /// MCP registrations still to add by hand (not written).
    pub mcp_pending: Vec<String>,
    pub index: IndexReport,
    pub docs_created: usize,
    pub docs_total: usize,
    pub docs_populated: usize,
    pub population_pending: bool,
    pub baselines_captured: usize,
    pub drift_score: Option<f64>,
    pub central: Option<CentralSymbol>,
    /// Knobyte files git ignores in this repository: left out of the commit.
    pub not_committed: Vec<String>,
}

impl SetupSummary {
    /// The suggested first question.
    pub fn try_prompt(&self) -> String {
        match &self.central {
            Some(c) => format!("Use Knobyte to explain how {} works.", c.name),
            None => "Use Knobyte to explain how this project is structured.".to_string(),
        }
    }

    pub fn print(&self) {
        println!("\n{}", "Setup summary".bold());
        let row = |k: &str, v: String| println!("  {:<15} {}", k, v);
        if self.tools.is_empty() {
            row("Tools", "none (.knobyte/AGENTS.md works with any agent that reads files)".into());
        }
        for (i, t) in self.tools.iter().enumerate() {
            row(if i == 0 { "Tools" } else { "" }, format!("{}: {}", t.name, t.files.join(", ")));
        }
        for p in &self.mcp_pending {
            row("", format!("{} (add by hand; see above)", p));
        }
        let ix = &self.index;
        if ix.graph_built {
            row(
                "Code graph",
                format!("{} files, {} symbols, {} edges", thousands(ix.files), thousands(ix.symbols), thousands(ix.edges)),
            );
        }
        match (&ix.vector, &ix.vector_error) {
            (Some(v), None) => row(
                "Vector index",
                format!("ready ({}, {}-dim): {} code nodes, {} wiki pages", v.backend, v.dim, thousands(v.code_nodes), v.wiki_pages),
            ),
            (_, Some(e)) => row("Vector index", format!("unavailable: {}", e)),
            _ => {}
        }
        row(
            "Docs",
            format!(
                "{}{}/{} populated{}",
                if self.docs_created > 0 { format!("{} created, ", self.docs_created) } else { String::new() },
                self.docs_populated,
                self.docs_total,
                if self.population_pending { " (population pending: your first agent session finishes it)" } else { "" }
            ),
        );
        row("Wiki index", format!("{} entities, {} grounding baseline(s) captured", ix.wiki_entities, self.baselines_captured));
        if let Some(s) = self.drift_score {
            row("Drift score", format!("{}/100", s));
        }
        if let Some(c) = &self.central {
            if c.used_from_files > 0 {
                row("Central symbol", format!("{} ({}, used from {} file{})", c.name, c.file, c.used_from_files, if c.used_from_files == 1 { "" } else { "s" }));
            }
        }
        for (i, p) in self.not_committed.iter().enumerate() {
            row(
                if i == 0 { "Not committed" } else { "" },
                format!(
                    "{} (gitignored here; it still works locally. Commit it with `git add -f {}` if your team should share it)",
                    p, p
                ),
            );
        }
        println!("\n{} \"{}\"", "Try asking your agent:".bold(), self.try_prompt());
    }
}
