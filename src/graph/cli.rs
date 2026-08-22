//! Rendering for the `knobyte graph ...` and `knobyte impact` commands.
//!
//! Every function returns a process exit code; human output goes to stdout, problems to stderr.

use colored::Colorize;
use std::collections::HashSet;
use std::path::Path;

use crate::config::{KnobyteConfig, DEFAULT_SCAFFOLD_DIR};
use crate::graph::corpus::{scan_corpus, CorpusPolicy, CoverageReport};
use crate::graph::agent::{gated_impact, ImpactTarget};
use crate::graph::engine::{GraphEngine, ImpactOptions, DEFAULT_SOURCE_LINES};
use crate::graph::maintenance::{
    graph_error, rebuild_graph_with, refresh_graph_with, repair_graph_with, MaintenanceOptions,
};
use crate::graph::read::ReadGate;
use crate::graph::status::{inspect_status, GraphHealth};
use crate::progress::{format_bytes, IndexProgressBar};

/// Human output lists at most this many paths; `--json` carries everything.
const MAX_SHOWN: usize = 10;

/// Set by SIGINT / SIGTERM while a maintenance command runs.
static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// While alive, Ctrl-C (SIGINT) and SIGTERM cancel the running maintenance run cleanly (the
/// live graph is untouched, the candidate removed) instead of killing the process mid-way.
struct InterruptGuard {
    #[cfg(unix)]
    previous: Vec<(libc::c_int, libc::sighandler_t)>,
}

impl InterruptGuard {
    fn install() -> Self {
        INTERRUPTED.store(false, std::sync::atomic::Ordering::SeqCst);
        #[cfg(unix)]
        {
            let mut previous = Vec::new();
            for sig in [libc::SIGINT, libc::SIGTERM] {
                // SAFETY: the handler only stores to an atomic, which is async-signal-safe.
                let prev = unsafe { libc::signal(sig, on_interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t) };
                previous.push((sig, prev));
            }
            InterruptGuard { previous }
        }
        #[cfg(not(unix))]
        {
            InterruptGuard {}
        }
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        #[cfg(unix)]
        for (sig, prev) in &self.previous {
            // SAFETY: restoring the handler that was installed before.
            unsafe {
                libc::signal(*sig, *prev);
            }
        }
    }
}

/// Maintenance options of a CLI run: `lock_timeout_secs` waits for a concurrent run, and an
/// interrupt cancels cleanly.
pub fn cli_maintenance_options(lock_timeout_secs: Option<u64>) -> MaintenanceOptions {
    let mut opts = MaintenanceOptions::default()
        .with_cancel(std::sync::Arc::new(|| INTERRUPTED.load(std::sync::atomic::Ordering::SeqCst)));
    if let Some(s) = lock_timeout_secs {
        opts = opts.with_lock_timeout(std::time::Duration::from_secs(s));
    }
    opts
}

/// Configuration for an explicit `--root`, or the discovered one.
pub fn config_for_root(default: &KnobyteConfig, root: Option<&Path>) -> KnobyteConfig {
    match root {
        Some(r) => {
            let r = r.canonicalize().unwrap_or_else(|_| r.to_path_buf());
            if r == default.project_root {
                default.clone()
            } else {
                KnobyteConfig::new(r.clone(), r.join(DEFAULT_SCAFFOLD_DIR))
            }
        }
        None => default.clone(),
    }
}

/// Human rendering of a graph error (stable code first; never a raw SQLite error).
pub fn describe_error(e: &rusqlite::Error) -> String {
    let m = graph_error(e);
    let mut s = format!("{} {}", m.code.red().bold(), m.message);
    if let Some(p) = &m.recovery_path {
        s.push_str(&format!("\nPrevious index retained at: {}", p));
    }
    s
}

/// `--json` rendering of a graph error: `{"error": {"code", "message", ...}}`.
pub fn error_json(e: &rusqlite::Error) -> serde_json::Value {
    serde_json::json!({ "error": graph_error(e) })
}

pub fn print_coverage(coverage: &CoverageReport) {
    if coverage.unindexed_total > 0 {
        let shown: Vec<String> = coverage
            .unindexed
            .iter()
            .take(MAX_SHOWN)
            .map(|e| format!("{} ({})", e.extension, e.files))
            .collect();
        println!(
            "Not indexed: {} source file(s) have extensions no extractor handles: {}",
            coverage.unindexed_total,
            shown.join(", ")
        );
        if coverage.unindexed.len() > MAX_SHOWN {
            println!("  ...and more extensions (use --json for the full breakdown)");
        }
        if coverage.truncated {
            println!("  (count may be higher: the coverage walk stopped at its bound)");
        }
    }
    if !coverage.skipped.is_empty() {
        println!(
            "Skipped {} file(s) the corpus policy will not index:",
            coverage.skipped.len()
        );
        for s in coverage.skipped.iter().take(MAX_SHOWN) {
            println!("  {} - {}", s.path, s.message);
        }
        if coverage.skipped.len() > MAX_SHOWN {
            println!(
                "  ...and {} more (use --json for the full list)",
                coverage.skipped.len() - MAX_SHOWN
            );
        }
        println!("Add a glob to \"graph.ignore\" in .knobyte/config.json to exclude a path deliberately.");
    }
}

fn sync_cozo(config: &KnobyteConfig, progress: &IndexProgressBar) {
    match crate::cozo::CozoEngine::open_configured(config) {
        Ok(cozo) => {
            if let Ok(conn) = rusqlite::Connection::open(config.graph_db_path()) {
                progress.set_phase("Synchronizing graph to CozoDB...");
                if let Err(e) = cozo.sync_from_graph(&conn) {
                    eprintln!("{} CozoDB sync failed: {}", "[warn]".yellow(), e);
                }
            }
        }
        Err(e) => eprintln!("{} CozoDB not synchronized: {}", "[warn]".yellow(), e),
    }
}

/// `knobyte graph` / `graph rebuild` (full) and `graph refresh` (incremental).
pub fn run_build(config: &KnobyteConfig, incremental: bool, json: bool) -> i32 {
    run_build_with(config, incremental, json, &cli_maintenance_options(None))
}

/// [`run_build`] with maintenance options (lock timeout, cancellation).
pub fn run_build_with(config: &KnobyteConfig, incremental: bool, json: bool, opts: &MaintenanceOptions) -> i32 {
    let _interrupts = InterruptGuard::install();
    let root = &config.project_root;
    let policy = CorpusPolicy::for_project(root);
    let scan = match scan_corpus(root, &policy) {
        Ok(s) => s,
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({ "error": { "code": "GRAPH_CORPUS_LIMIT_EXCEEDED", "message": e.to_string() } })
                );
            } else {
                eprintln!("{} {}", "GRAPH_CORPUS_LIMIT_EXCEEDED".red().bold(), e);
            }
            return 1;
        }
    };
    if !json {
        println!(
            "{} code repository at {} ({} files, {})...",
            if incremental { "Refreshing" } else { "Indexing" },
            root.display(),
            scan.files.len(),
            format_bytes(scan.total_bytes)
        );
    }
    let progress = IndexProgressBar::new(scan.total_bytes, scan.files.len(), !json);
    let db_path = config.graph_db_path();
    let result = if incremental {
        refresh_graph_with(&db_path, root, &scan, Some(&progress), opts).map(|o| {
            let published = o.published;
            (serde_json::to_value(&o).unwrap_or_default(), o.summary, Some((o.mode, o.changes)), None, published)
        })
    } else {
        rebuild_graph_with(&db_path, root, &scan, Some(&progress), opts).map(|o| {
            (serde_json::to_value(&o).unwrap_or_default(), o.summary, None, o.recovery_path, true)
        })
    };
    let (value, summary, refresh, recovery, published) = match result {
        Ok(r) => r,
        Err(e) => {
            progress.finish_and_clear();
            if json {
                println!("{}", serde_json::to_string_pretty(&error_json(&e)).unwrap_or_default());
            } else {
                eprintln!("{}", describe_error(&e));
            }
            return 1;
        }
    };
    if published {
        sync_cozo(config, &progress);
    }
    progress.finish_and_clear();
    let health = inspect_status(&db_path, root);
    if json {
        let mut value = value;
        if let Some(obj) = value.as_object_mut() {
            obj.insert("status".into(), serde_json::json!(health.status));
            obj.insert("parse_health".into(), serde_json::to_value(&health.parse_health).unwrap_or_default());
            // Same numbers as `graph status --json` `counts`, whatever this run re-extracted.
            obj.insert("totals".into(), serde_json::to_value(&health.counts).unwrap_or_default());
        }
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or_default());
        return 0;
    }
    match refresh {
        Some((mode, changes)) if mode == "noop" => {
            println!(
                "{} Code graph already up to date; nothing re-extracted or published. \
                 Totals: {} files, {} symbols ({} nodes), {} relationships. Status: {}.",
                "[ok]".green().bold(),
                health.counts.files,
                summary.symbols_indexed,
                health.counts.nodes,
                health.counts.edges,
                health.status
            );
            let _ = changes;
        }
        Some((mode, changes)) => {
            println!(
                "{} Refreshed code graph ({}) in {}ms: {} added, {} modified, {} deleted; \
                 {} files re-extracted, {} reused from cache. Totals: {} files, {} symbols, {} relationships. Status: {}.",
                "[ok]".green().bold(),
                mode,
                summary.duration_ms,
                changes.added.len(),
                changes.modified.len(),
                changes.deleted.len(),
                summary.files_extracted,
                summary.files_reused,
                summary.files_indexed,
                summary.symbols_indexed,
                summary.edges_indexed,
                health.status
            );
            if let Some(p) = value.get("publication").filter(|p| p.is_object()) {
                let rows = |k: &str| -> u64 {
                    p.get("delta")
                        .and_then(|d| d.get(k))
                        .and_then(|m| m.as_object())
                        .map(|m| m.values().filter_map(|v| v.as_u64()).sum())
                        .unwrap_or(0)
                };
                println!(
                    "Published as {} in {}ms ({} row(s) written, {} removed).",
                    p.get("mode").and_then(|m| m.as_str()).unwrap_or("full"),
                    p.get("duration_ms").and_then(|m| m.as_u64()).unwrap_or(0),
                    rows("written"),
                    rows("deleted")
                );
            }
            if let Some(v) = value.get("migrated_from").and_then(|v| v.as_i64()) {
                println!("Graph schema upgraded in place from v{}.", v);
            }
        }
        None => {
            println!(
                "{} Built code graph in {}ms ({} files, {} symbols, {} relationships). Status: {}.",
                "[ok]".green().bold(),
                summary.duration_ms,
                summary.files_indexed,
                summary.symbols_indexed,
                summary.edges_indexed,
                health.status
            );
        }
    }
    if let Some(p) = recovery {
        println!("Previous (corrupt) index retained for local recovery: {}", p);
    }
    if let Some(ts) = summary.typescript_compiler.as_deref().filter(|t| *t != "source") {
        println!("TypeScript compiler: {}", ts);
    }
    print_coverage(&scan.coverage);
    0
}

pub fn print_status(h: &GraphHealth) {
    let status = match h.status.as_str() {
        "fresh" => h.status.green().bold(),
        "stale" | "degraded" => h.status.yellow().bold(),
        _ => h.status.red().bold(),
    };
    println!("{}", "=== Code Graph Status ===".bold());
    println!("Graph status: {}", status);
    if !h.inspected {
        println!("Last successful index: not inspected");
    } else {
        println!(
            "Last successful index: {}",
            h.last_successful_index_at.as_deref().unwrap_or("never")
        );
        println!(
            "Nodes: {}  Edges: {}  Files: {}  Unresolved refs: {}",
            h.counts.nodes, h.counts.edges, h.counts.files, h.counts.unresolved
        );
        if let Some(v) = h.schema_version {
            println!(
                "Schema: v{}  Extractor: {}",
                v,
                h.extractor_version.as_deref().unwrap_or("unknown")
            );
        }
        if let Some(ts) = h.typescript_compiler.as_deref().filter(|t| *t != "source") {
            println!("TypeScript compiler: {}", ts);
        }
        let c = &h.changes;
        println!(
            "Sources: {} changed ({} added, {} modified, {} deleted{})",
            c.total,
            c.added.len(),
            c.modified.len(),
            c.deleted.len(),
            if c.truncated { "; lists truncated" } else { "" }
        );
        for (label, list) in [("+", &c.added), ("~", &c.modified), ("-", &c.deleted)] {
            for p in list.iter().take(MAX_SHOWN) {
                println!("  {} {}", label, p);
            }
        }
        println!(
            "Parse health: {} ok, {} partial, {} failed",
            h.parse_health.ok, h.parse_health.partial, h.parse_health.failed
        );
        if let Some(cov) = &h.coverage {
            print_coverage(cov);
        }
    }
    for d in &h.diagnostics {
        println!("{} {}: {}", d.severity.to_uppercase(), d.code, d.message);
    }
    if let Some(cmd) = h.next_command() {
        println!("Next: {}", cmd);
    }
}

/// `knobyte graph status`: strictly read-only.
pub fn run_status(config: &KnobyteConfig, json: bool) -> i32 {
    let health = inspect_status(&config.graph_db_path(), &config.project_root);
    if json {
        println!("{}", serde_json::to_string_pretty(&health).unwrap_or_default());
    } else {
        print_status(&health);
    }
    0
}

/// `knobyte graph repair`.
pub fn run_repair(config: &KnobyteConfig, json: bool) -> i32 {
    run_repair_with(config, json, &cli_maintenance_options(None))
}

/// [`run_repair`] with maintenance options (lock timeout, cancellation).
pub fn run_repair_with(config: &KnobyteConfig, json: bool, opts: &MaintenanceOptions) -> i32 {
    let _interrupts = InterruptGuard::install();
    match repair_graph_with(&config.graph_db_path(), &config.project_root, opts) {
        Ok(r) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
            } else {
                let wal = if r.recovered_wal_frames > 0 {
                    format!("recovered {} WAL frame(s)", r.recovered_wal_frames)
                } else {
                    "no WAL data was pending".to_string()
                };
                let mut fixes = Vec::new();
                if r.reindexed {
                    fixes.push("indexes rebuilt".to_string());
                }
                if r.fts_rebuilt {
                    fixes.push("full-text index rebuilt".to_string());
                }
                if r.orphan_edges_removed > 0 {
                    fixes.push(format!("{} dangling edge(s) removed", r.orphan_edges_removed));
                }
                if r.orphan_refs_removed > 0 {
                    fixes.push(format!("{} dangling reference(s) removed", r.orphan_refs_removed));
                }
                if r.dangling_bindings_cleared > 0 {
                    fixes.push(format!("{} import binding(s) unlinked", r.dangling_bindings_cleared));
                }
                if let Some(v) = r.migrated_from {
                    fixes.push(format!("schema upgraded from v{}", v));
                }
                if r.stale_candidates_removed > 0 {
                    fixes.push(format!(
                        "{} abandoned build candidate(s) removed",
                        r.stale_candidates_removed
                    ));
                }
                if fixes.is_empty() {
                    fixes.push("no inconsistencies found".to_string());
                }
                println!(
                    "{} Graph store repaired: {}; {}; integrity {}; schema v{}; status {}.",
                    "[ok]".green().bold(),
                    wal,
                    fixes.join(", "),
                    r.integrity_after,
                    r.schema_version,
                    r.status
                );
            }
            0
        }
        Err(e) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&error_json(&e)).unwrap_or_default());
            } else {
                eprintln!("{}", describe_error(&e));
            }
            1
        }
    }
}

/// Inspect freshness and open the graph read-only for a non-protocol read. Prints the refusal
/// (`--json`: `{"error": {...}}`) and returns `None` when the read is refused.
fn open_read(config: &KnobyteConfig, json: bool) -> Option<(GraphEngine, ReadGate)> {
    let db = config.graph_db_path();
    let opened = ReadGate::open_session(&db, &config.project_root, false);
    match opened {
        Ok(v) => Some(v),
        Err(u) => {
            if json {
                println!("{}", serde_json::to_string_pretty(&u.json_error()).unwrap_or_default());
            } else {
                eprintln!("{} {}", u.reason_code.red().bold(), u.message);
                if let Some(c) = &u.recovery_command {
                    eprintln!("Next: {}", c);
                }
            }
            None
        }
    }
}

/// Tell a human reader which changed files were left out of a gated answer.
fn warn_drift(gate: &ReadGate) {
    if let Some(w) = gate.warning() {
        eprintln!("{} {}", "[stale]".yellow().bold(), w);
        for f in gate.drifted.iter().take(MAX_SHOWN) {
            eprintln!("  excluded: {}", f);
        }
    }
}

fn print_error(json: bool, code: &str, message: &str, extra: serde_json::Value) {
    if json {
        let mut v = serde_json::json!({ "type": "error", "code": code, "message": message });
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        println!("{}", v);
    } else {
        eprintln!("{} {}", code.red().bold(), message);
    }
}

/// `knobyte graph query <relation> <target>` (human / `--json`), gated on graph freshness:
/// results located in changed files are excluded and said so.
pub fn run_query(
    config: &KnobyteConfig,
    relation: &str,
    target: &str,
    json: bool,
    jsonl: bool,
) -> Result<i32, Box<dyn std::error::Error>> {
    if !crate::graph::agent::QUERY_RELATIONS.contains(&relation) {
        return Err(format!(
            "Unknown relation: {}. Use where-defined, who-calls, what-calls, or who-imports",
            relation
        )
        .into());
    }
    let machine = json || jsonl;
    let Some((engine, gate)) = open_read(config, machine) else { return Ok(1) };
    if relation != "who-imports" {
        let defined = engine.query_where_defined(target)?;
        let paths: Vec<&str> = defined.iter().map(|n| n.file_path.as_str()).collect();
        if gate.target_drifted(target, &paths).is_some() {
            print_error(
                machine,
                "TARGET_SOURCE_DRIFTED",
                &format!("'{}' is defined only in files changed since the last build. Run `knobyte graph refresh`.", target),
                serde_json::json!({ "target": target, "filePaths": paths }),
            );
            return Ok(1);
        }
    }
    let nodes = match relation {
        "where-defined" => engine.query_where_defined(target)?,
        "who-calls" => engine.query_who_calls(target)?,
        "what-calls" => engine.query_what_calls(target)?,
        _ => engine.query_who_imports(target)?,
    };
    let nodes: Vec<_> = nodes.into_iter().filter(|n| !gate.is_drifted(&n.file_path)).collect();
    warn_drift(&gate);
    if jsonl {
        for n in &nodes {
            println!("{}", serde_json::to_string(n)?);
        }
    } else if json {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
    } else {
        for n in &nodes {
            println!(
                "{} {} ({}:{}:{})",
                n.kind.cyan(),
                n.qualified_name.bold(),
                n.file_path,
                n.start_line,
                n.start_column
            );
        }
        if nodes.is_empty() {
            eprintln!("No results for {} '{}'.", relation, target);
        }
    }
    Ok(0)
}

/// `knobyte impact <target> [--depth N] [--callers-only]`.
pub fn run_impact(
    config: &KnobyteConfig,
    target: &str,
    opts: ImpactOptions,
    json: bool,
    jsonl: bool,
) -> Result<i32, Box<dyn std::error::Error>> {
    let machine = json || jsonl;
    let Some((engine, gate)) = open_read(config, machine) else { return Ok(1) };
    let report = match gated_impact(&engine, &gate, &config.scaffold_root, target, opts)? {
        ImpactTarget::Found(r) => r,
        ImpactTarget::NotFound => {
            if machine {
                let mut v = serde_json::json!({ "type": "error", "code": "TARGET_NOT_FOUND", "target": target });
                if let Some(o) = v.as_object_mut() {
                    o.extend(gate.not_found_coverage());
                }
                println!("{}", v);
            } else {
                eprintln!("Target not found in the code graph: {}", target);
            }
            return Ok(1);
        }
        ImpactTarget::Drifted(files) => {
            print_error(
                machine,
                "TARGET_SOURCE_DRIFTED",
                &format!(
                    "'{}' lies in file(s) changed since the last build ({}). Run `knobyte graph refresh`.",
                    target,
                    files.join(", ")
                ),
                serde_json::json!({ "target": target, "filePaths": files }),
            );
            return Ok(1);
        }
        ImpactTarget::Ambiguous(candidates) => {
            if machine {
                let c: Vec<serde_json::Value> = candidates
                    .iter()
                    .map(|n| serde_json::json!({ "id": n.id, "kind": n.kind, "name": n.name, "file": n.file_path, "line": n.start_line }))
                    .collect();
                println!(
                    "{}",
                    serde_json::json!({ "type": "error", "code": "TARGET_AMBIGUOUS", "target": target, "candidates": c })
                );
            } else {
                eprintln!(
                    "{} '{}' names {} declarations; pass one id or grounding reference:",
                    "TARGET_AMBIGUOUS".red().bold(),
                    target,
                    candidates.len()
                );
                for n in &candidates {
                    eprintln!("  {} {} ({}:{})  {}", n.kind, n.qualified_name, n.file_path, n.start_line, n.id);
                }
            }
            return Ok(1);
        }
    };
    warn_drift(&gate);
    if jsonl {
        println!("{}", serde_json::json!({ "type": "target", "value": target }));
        for s in gate.status_records() {
            println!("{}", s);
        }
        for n in &report.roots {
            let mut v = serde_json::to_value(n)?;
            v["type"] = "defines".into();
            println!("{}", v);
        }
        for e in &report.impacted {
            let mut v = serde_json::to_value(e)?;
            v["type"] = if opts.callers_only { "caller" } else { "dependent" }.into();
            println!("{}", v);
        }
        for g in &report.groundings {
            let mut v = serde_json::to_value(g)?;
            v["type"] = "grounding".into();
            println!("{}", v);
        }
        println!(
            "{}",
            serde_json::json!({
                "type": "summary",
                "matched_nodes": report.roots.len() + report.impacted.len(),
                "grounded_docs": report.groundings.iter().map(|g| g.doc.as_str()).collect::<HashSet<_>>().len(),
                "truncated": report.truncated,
                "excluded_files": gate.drifted,
            })
        );
    } else if json {
        let mut v = serde_json::to_value(&report)?;
        if gate.source_drifted() {
            v["stale"] = true.into();
            v["excluded_files"] = serde_json::to_value(&gate.drifted)?;
        }
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!(
            "Impact radius for '{}': {} affected nodes (depth {}{})",
            target,
            report.roots.len() + report.impacted.len(),
            opts.depth.clamp(1, crate::graph::engine::MAX_IMPACT_DEPTH),
            if opts.callers_only { ", callers only" } else { "" }
        );
        for n in &report.roots {
            println!("  * {} {} ({})", n.kind.cyan(), n.qualified_name, n.file_path);
        }
        for e in &report.impacted {
            println!(
                "  {} {} {} ({}) via {}",
                format!("[{}]", e.depth).dimmed(),
                e.node.kind.cyan(),
                e.node.qualified_name,
                e.node.file_path,
                e.via
            );
        }
        if report.truncated {
            println!("  ...truncated at {} nodes", crate::graph::engine::MAX_IMPACT_NODES);
        }
        if !report.groundings.is_empty() {
            println!("Grounded knowledge affected:");
            for g in &report.groundings {
                println!("  {} -> {}", g.doc.bold(), g.reference);
            }
        }
    }
    Ok(0)
}

/// `knobyte graph get <ids...> [--source] [--max-lines N]`. Each id is a node id or a readable
/// grounding reference; ids that resolve to nothing, or to a file changed since the build, are
/// reported (stderr / `--json` errors) and make the exit code non-zero.
pub fn run_get(
    config: &KnobyteConfig,
    ids: &[String],
    source: bool,
    max_lines: Option<usize>,
    json: bool,
    jsonl: bool,
) -> Result<i32, Box<dyn std::error::Error>> {
    let machine = json || jsonl;
    let Some((engine, gate)) = open_read(config, machine) else { return Ok(1) };
    let mut nodes = Vec::new();
    let mut errors: Vec<serde_json::Value> = Vec::new();
    for id in ids {
        let node = engine
            .get_nodes(std::slice::from_ref(id))?
            .into_iter()
            .next()
            .or_else(|| engine.resolve_ref(id).ok().and_then(|r| r.node().cloned()));
        match node {
            None => errors.push(serde_json::json!({ "type": "error", "code": "NODE_NOT_FOUND", "id": id })),
            Some(n) if gate.is_drifted(&n.file_path) => errors.push(serde_json::json!({
                "type": "error", "code": "TARGET_SOURCE_DRIFTED", "id": id, "filePaths": [n.file_path],
            })),
            Some(n) => nodes.push(n),
        }
    }
    if !machine {
        for e in &errors {
            let file = e["filePaths"][0]
                .as_str()
                .map(|p| format!(" ({} changed since the last build; run `knobyte graph refresh`)", p))
                .unwrap_or_default();
            eprintln!("{} {}{}", e["code"].as_str().unwrap_or("").red().bold(), e["id"].as_str().unwrap_or(""), file);
        }
    }
    let exit = if errors.is_empty() && !nodes.is_empty() { 0 } else { 1 };
    if !source {
        if jsonl {
            for n in &nodes {
                println!("{}", serde_json::to_string(n)?);
            }
            for e in &errors {
                println!("{}", e);
            }
        } else if json {
            if errors.is_empty() {
                println!("{}", serde_json::to_string_pretty(&nodes)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "nodes": nodes, "errors": errors }))?);
            }
        } else {
            for n in nodes {
                println!("{} {} ({})", n.kind.cyan(), n.qualified_name.bold(), n.id);
                println!("  {}:{}-{}", n.file_path, n.start_line, n.end_line);
                if let Some(sig) = n.signature {
                    println!("  {}", sig.dimmed());
                }
            }
        }
        return Ok(exit);
    }
    let resolved: Vec<String> = nodes.iter().map(|n| n.id.clone()).collect();
    let items = engine.get_with_source(&config.project_root, &resolved, max_lines.unwrap_or(DEFAULT_SOURCE_LINES))?;
    if jsonl {
        for n in &items {
            println!("{}", serde_json::to_string(n)?);
        }
        for e in &errors {
            println!("{}", e);
        }
    } else if json {
        if errors.is_empty() {
            println!("{}", serde_json::to_string_pretty(&items)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({ "nodes": items, "errors": errors }))?);
        }
    } else {
        for it in &items {
            println!(
                "{} {} ({}:{}-{}){}",
                it.node.kind.cyan(),
                it.node.qualified_name.bold(),
                it.node.file_path,
                it.source_start_line,
                it.source_end_line,
                if it.stale { " [file changed since indexing]".yellow().to_string() } else { String::new() }
            );
            if let Some(src) = &it.source {
                for (i, line) in src.lines().enumerate() {
                    println!("{:>5} | {}", it.source_start_line as usize + i, line);
                }
            }
            if it.truncated {
                println!(
                    "      ... truncated at {} lines (declaration ends at line {}; use --max-lines)",
                    it.source_end_line - it.source_start_line + 1,
                    it.node.end_line
                );
            }
        }
    }
    Ok(if items.is_empty() { 1 } else { exit })
}
