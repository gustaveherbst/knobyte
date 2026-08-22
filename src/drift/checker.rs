//! `knobyte check`: run every drift checker over the scaffold and score the result.
//!
//! The headline score follows `scoring::compute_score` (100 − 10/error − 3/warning − 1/info;
//! moves decided by neighbours are unscored). The share of intact groundings is reported
//! separately as `grounding_score`.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{DriftSettings, KnobyteConfig, StalenessThresholds};
use crate::drift::checkers::{self, CheckContext};
use crate::drift::claims::extract_claims_from_str;
use crate::drift::freshness::{inspect_engine, GraphFreshness, GraphState};
pub use crate::drift::grounding::GroundingHealth;
use crate::drift::grounding::{check_groundings, extract_doc_refs, GroundingDoc};
use crate::drift::markdown::{frontmatter_problem, parse_frontmatter};
use crate::drift::scoring::compute_score;
use crate::drift::types::{codes, Claim, ClaimKind, DriftIssue, SEVERITY_ERROR, SEVERITY_WARNING};
use crate::graph::grounding::scaffold_markdown_files;
use crate::graph::GraphEngine;
use crate::wiki::parser::parse_markdown_entity;

/// Default scaffold files the claim checkers read, relative to the scaffold root.
pub const DEFAULT_SCAFFOLD_PATTERNS: &[&str] = &[
    "context/*.md",
    "patterns/*.md",
    "ROUTER.md",
    "AGENTS.md",
    "SETUP.md",
    "SYNC.md",
];

/// Tool config files at the project root that are also checked.
pub const ROOT_TOOL_FILES: &[&str] = &["CLAUDE.md", ".cursorrules", ".windsurfrules"];

#[derive(Debug, Clone, Default)]
pub struct DriftCheckOptions {
    /// Build the verbose log.
    pub verbose: bool,
    /// Staleness thresholds; `None` reads `staleness_thresholds` from config.json.
    pub staleness: Option<StalenessThresholds>,
    /// Override the scaffold glob patterns (relative to the scaffold root).
    pub scaffold_patterns: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftReport {
    /// Drift score 0-100 (100 − 10/error − 3/warning − 1/info).
    pub score: f64,
    /// `healthy` (score ≥ 80), `drifting`, or `error` (no scaffold).
    pub status: String,
    /// Scaffold files checked.
    pub file_count: usize,
    /// Source files in the code graph.
    pub repo_file_count: usize,
    pub issue_count: usize,
    pub issues: Vec<DriftIssue>,
    pub grounding: GroundingHealth,
    /// Share of intact groundings (`intact / total * 100`; 0 when there are none).
    #[serde(default)]
    pub grounding_score: f64,
    /// Code-graph freshness the grounding checks ran against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph: Option<GraphFreshness>,
    /// Graph freshness / grounding nudges (not scored).
    #[serde(default)]
    pub nudges: Vec<String>,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verbose_log: Option<Vec<String>>,
}

impl DriftReport {
    pub fn count(&self, severity: &str) -> usize {
        self.issues
            .iter()
            .filter(|i| i.severity == severity)
            .count()
    }
}

/// Share of intact groundings: `intact / total * 100` (0 when there are no groundings).
pub fn grounding_score(grounding: &GroundingHealth) -> f64 {
    if grounding.total == 0 {
        0.0
    } else {
        (grounding.intact as f64 / grounding.total as f64) * 100.0
    }
}

/// Run every drift checker with default options.
pub fn run_drift_check(config: &KnobyteConfig) -> DriftReport {
    run_drift_check_with(config, &DriftCheckOptions::default())
}

/// Scaffold files the claim checkers read (absolute paths, deduplicated by real path).
pub fn find_scaffold_files(config: &KnobyteConfig, patterns: Option<&[String]>) -> Vec<PathBuf> {
    let defaults: Vec<String> = DEFAULT_SCAFFOLD_PATTERNS
        .iter()
        .map(|s| s.to_string())
        .collect();
    let patterns = patterns.unwrap_or(&defaults);
    let root = &config.scaffold_root;
    let entries = checkers::walk_index(root, 4, &["node_modules", "local"]);
    let mut files: Vec<PathBuf> = Vec::new();
    for pattern in patterns {
        let Some(re) = checkers::glob_regex(pattern) else {
            continue;
        };
        let mut matched: Vec<&str> = entries
            .iter()
            .filter(|e| !e.is_dir && re.is_match(&e.rel))
            .map(|e| e.rel.as_str())
            .collect();
        matched.sort();
        files.extend(matched.into_iter().map(|m| root.join(m)));
    }
    if config.scaffold_root != config.project_root {
        for name in ROOT_TOOL_FILES {
            let p = config.project_root.join(name);
            if p.is_file() {
                files.push(p);
            }
        }
    }
    let mut seen = HashSet::new();
    files
        .into_iter()
        .filter(|f| seen.insert(f.canonicalize().unwrap_or_else(|_| f.clone())))
        .collect()
}

fn is_populated_grounding_candidate(rel: &str, content: &str) -> bool {
    if !(rel.contains("context/") || rel.contains("patterns/")) {
        return false;
    }
    if rel.ends_with("patterns/README.md") || rel.ends_with("patterns/INDEX.md") {
        return false;
    }
    !content.contains("[YYYY-MM-DD]") && !content.trim().is_empty()
}

fn freshness_nudge(fresh: &GraphFreshness, needs_migration: bool) -> String {
    let cmd = fresh
        .remediation
        .clone()
        .unwrap_or_else(|| "knobyte graph rebuild".to_string());
    match fresh.status {
        GraphState::Missing if needs_migration => format!(
            "Code graph is missing. Run `{}`, then `knobyte graph ground`.",
            cmd
        ),
        GraphState::Missing => format!(
            "Code graph is missing; grounding checks skipped. Run `{}`.",
            cmd
        ),
        GraphState::Stale if fresh.whole_graph.is_some() => format!(
            "Code graph must be refreshed ({}); groundings were marked unverified. Run `{}`.",
            fresh.whole_graph.as_deref().unwrap_or_default(),
            cmd
        ),
        GraphState::Stale => format!(
            "Code graph is stale; groundings in edited source files were re-checked from the working tree, the rest that could not be settled were marked unverified. Run `{}`.",
            cmd
        ),
        GraphState::Corrupt => format!(
            "Code graph is corrupt; grounding checks skipped.{} Run `{}`.",
            fresh
                .detail
                .as_deref()
                .map(|d| format!(" {}", d))
                .unwrap_or_default(),
            cmd
        ),
        GraphState::RebuildRequired => format!(
            "Code graph requires a rebuild; grounding checks skipped. Run `{}`.",
            cmd
        ),
        GraphState::Degraded => format!(
            "Code graph is degraded; grounding checks skipped.{} Run `{}`.",
            fresh
                .detail
                .as_deref()
                .map(|d| format!(" {}", d))
                .unwrap_or_default(),
            cmd
        ),
        GraphState::Fresh => String::new(),
    }
}

fn scaffold_missing_report(config: &KnobyteConfig) -> DriftReport {
    let issues = vec![DriftIssue::new(
        codes::SCAFFOLD_MISSING,
        SEVERITY_ERROR,
        crate::drift::types::project_relative(&config.project_root, &config.scaffold_root),
        None,
        "Scaffold directory does not exist. Run 'knobyte setup' first.",
    )];
    DriftReport {
        score: 0.0,
        status: "error".to_string(),
        file_count: 0,
        repo_file_count: 0,
        issue_count: issues.len(),
        issues,
        grounding: GroundingHealth::default(),
        grounding_score: 0.0,
        graph: None,
        nudges: Vec::new(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        verbose_log: None,
    }
}

/// Run every drift checker.
pub fn run_drift_check_with(config: &KnobyteConfig, opts: &DriftCheckOptions) -> DriftReport {
    if !config.scaffold_root.exists() {
        return scaffold_missing_report(config);
    }
    let ctx = CheckContext::new(&config.project_root, &config.scaffold_root);
    let thresholds = opts
        .staleness
        .unwrap_or_else(|| DriftSettings::load(&config.scaffold_root).staleness_thresholds);

    // Claim-checked scaffold files and every scaffold markdown file (grounding).
    let scaffold_files = find_scaffold_files(config, opts.scaffold_patterns.as_deref());
    let all_markdown = scaffold_markdown_files(&config.scaffold_root);

    let mut contents: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut read = |p: &Path| -> Option<String> {
        contents
            .entry(p.to_path_buf())
            .or_insert_with(|| fs::read_to_string(p).ok())
            .clone()
    };

    let mut issues: Vec<DriftIssue> = Vec::new();
    let mut checker_counts: Vec<(String, usize)> = Vec::new();
    let mut claims: Vec<Claim> = Vec::new();
    let mut scaffold_text = String::new();

    // Entity ids for relation targets.
    let mut entity_ids: HashSet<String> = HashSet::new();
    let mut grounding_docs: Vec<(String, PathBuf, String)> = Vec::new();
    let mut frontmatter_broken: HashSet<String> = HashSet::new();
    for (rel, path) in &all_markdown {
        let Some(content) = read(path) else {
            issues.push(DriftIssue::new(
                codes::UNREADABLE_FILE,
                SEVERITY_ERROR,
                ctx.rel(path),
                None,
                "Failed to read file",
            ));
            continue;
        };
        if let Some(entity) = parse_markdown_entity(rel, &content) {
            entity_ids.insert(entity.id);
        }
        // Unparseable frontmatter silently drops every field (id, relations, grounds_to, ...).
        if let Some((code, message, line)) = frontmatter_problem(&content) {
            let source = ctx.rel(path);
            frontmatter_broken.insert(source.clone());
            issues.push(DriftIssue::new(code, SEVERITY_ERROR, source, Some(line), message));
        }
        grounding_docs.push((rel.clone(), path.clone(), content));
    }

    let has_groundings = grounding_docs
        .iter()
        .any(|(_, _, c)| !extract_doc_refs(c).is_empty());
    let needs_migration = !has_groundings
        && scaffold_files.iter().any(|p| {
            read(p)
                .map(|c| is_populated_grounding_candidate(&ctx.rel(p), &c))
                .unwrap_or(false)
        });
    let grounding_relevant = has_groundings || needs_migration;

    // Graph freshness gating.
    let graph_db = config.graph_db_path();
    let engine = if graph_db.exists() {
        GraphEngine::open(&graph_db).ok()
    } else {
        None
    };
    let freshness = match &engine {
        Some(e) => inspect_engine(e, &config.project_root),
        None => crate::drift::freshness::inspect_graph(config),
    };
    let repo_file_count = engine
        .as_ref()
        .and_then(|e| {
            e.connection()
                .query_row("SELECT COUNT(*) FROM files", [], |r| r.get::<_, i64>(0))
                .ok()
        })
        .unwrap_or(0) as usize;

    let mut nudges = Vec::new();
    if grounding_relevant && freshness.status != GraphState::Fresh {
        nudges.push(freshness_nudge(&freshness, needs_migration));
    } else if freshness.status == GraphState::Fresh && needs_migration {
        nudges.push(
            "Existing scaffold has no code grounding. Add `grounds_to:` entries or `<!-- kb-ground: kind:path:name -->` anchors (e.g. function:src/auth.rs:validate_token), then run `knobyte graph ground`."
                .to_string(),
        );
    }

    // Per-file checkers.
    for path in &scaffold_files {
        let source = ctx.rel(path);
        let Some(content) = read(path) else { continue };
        scaffold_text.push_str(&content);
        scaffold_text.push('\n');
        claims.extend(extract_claims_from_str(&content, &source));

        let fm = parse_frontmatter(&content);
        let edge = checkers::edges::check_edges(fm.as_ref(), path, &source, &ctx, &entity_ids);
        // An unparseable block is reported once (FRONTMATTER_PARSE_ERROR above) instead of as
        // "missing field" side effects.
        let fmc = if frontmatter_broken.contains(&source) {
            Vec::new()
        } else {
            checkers::frontmatter_completeness::check_frontmatter_completeness(fm.as_ref(), &source)
        };
        let last_updated = fm
            .as_ref()
            .and_then(|f| f.get("last_updated"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let stale = checkers::staleness::check_staleness(
            &source,
            &config.project_root,
            &thresholds,
            last_updated.as_deref(),
        );
        let shape = checkers::grounding_shape::check_grounding_shape(fm.as_ref(), &source);
        checker_counts.push((format!("edges:{}", source), edge.len()));
        checker_counts.push((format!("frontmatter-completeness:{}", source), fmc.len()));
        checker_counts.push((format!("staleness:{}", source), stale.len()));
        checker_counts.push((format!("grounding-shape:{}", source), shape.len()));
        issues.extend(edge);
        issues.extend(fmc);
        issues.extend(stale);
        issues.extend(shape);
    }

    // Grounding against the graph (every scaffold markdown file).
    let docs: Vec<GroundingDoc> = grounding_docs
        .iter()
        .map(|(rel, path, content)| GroundingDoc {
            scaffold_rel: rel,
            path,
            content,
        })
        .collect();
    let (grounding_issues, grounding) = check_groundings(
        config,
        &docs,
        engine.as_ref().map(|e| e.connection()),
        &freshness,
    );
    checker_counts.push(("grounding".to_string(), grounding_issues.len()));
    issues.extend(grounding_issues);
    if grounding.moved > 0 {
        nudges.push(format!(
            "{} grounding reference(s) moved; run `knobyte sync` (or `knobyte check --fix`) to rewrite them.",
            grounding.moved
        ));
    }

    // Claim checkers. A token documented as a package is not a missing file.
    let declared: HashSet<&str> = claims
        .iter()
        .filter(|c| c.kind == ClaimKind::Dependency)
        .map(|c| c.value.as_str())
        .collect();
    let path_claims: Vec<Claim> = claims
        .iter()
        .filter(|c| c.kind != ClaimKind::Path || !declared.contains(c.value.as_str()))
        .cloned()
        .collect();

    let mut run = |name: &str, found: Vec<DriftIssue>| {
        checker_counts.push((name.to_string(), found.len()));
        issues.extend(found);
    };
    run("paths", checkers::path::check_paths(&path_claims, &ctx));
    run("commands", checkers::command::check_commands(&claims, &ctx));
    run(
        "dependencies",
        checkers::dependency::check_dependencies(&claims, &ctx),
    );
    run(
        "cross-file",
        checkers::cross_file::check_cross_file(&claims),
    );
    run("index-sync", checkers::index_sync::check_index_sync(&ctx));
    run(
        "stale-pattern",
        checkers::stale_pattern::check_stale_patterns(&ctx),
    );
    run(
        "script-coverage",
        checkers::script_coverage::check_script_coverage(&scaffold_text, &ctx),
    );
    run(
        "anchor-link",
        checkers::anchor_link::check_anchor_link(&ctx),
    );
    run(
        "tool-config-sync",
        checkers::tool_config_sync::check_tool_config_sync(&ctx),
    );
    let mut todo = Vec::new();
    let mut links = Vec::new();
    for path in &scaffold_files {
        let Some(content) = read(path) else { continue };
        let source = ctx.rel(path);
        todo.extend(checkers::todo_fixme::check_todo_fixme(&content, &source));
        links.extend(checkers::broken_link::check_broken_links(
            &content, path, &source, &ctx,
        ));
    }
    run("todo-fixme", todo);
    run("broken-link", links);

    let score = compute_score(&issues) as f64;
    let status = if score >= 80.0 { "healthy" } else { "drifting" };

    let file_count = {
        let mut set: HashSet<PathBuf> = scaffold_files
            .iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
            .collect();
        for (_, p) in &all_markdown {
            set.insert(p.canonicalize().unwrap_or_else(|_| p.clone()));
        }
        set.len()
    };
    let verbose_log = opts
        .verbose
        .then(|| build_verbose_log(file_count, &claims, &checker_counts, &freshness));

    DriftReport {
        score,
        status: status.to_string(),
        file_count,
        repo_file_count,
        issue_count: issues.len(),
        issues,
        grounding_score: grounding_score(&grounding),
        grounding,
        graph: Some(freshness),
        nudges,
        timestamp: chrono::Utc::now().to_rfc3339(),
        verbose_log,
    }
}

/// Verbose diagnostic lines: files scanned, claims by kind, issues per checker.
pub fn build_verbose_log(
    files_scanned: usize,
    claims: &[Claim],
    checker_counts: &[(String, usize)],
    freshness: &GraphFreshness,
) -> Vec<String> {
    let count = |k: ClaimKind| claims.iter().filter(|c| c.kind == k).count();
    let mut out = vec![
        format!("Scaffold files scanned: {}", files_scanned),
        format!(
            "Claims extracted: {} (path: {}, command: {}, dependency: {})",
            claims.len(),
            count(ClaimKind::Path),
            count(ClaimKind::Command),
            count(ClaimKind::Dependency)
        ),
        format!("Code graph: {}", freshness.summary()),
    ];
    for (name, n) in checker_counts {
        out.push(format!(
            "Checker {}: {} issue{}",
            name,
            n,
            if *n == 1 { "" } else { "s" }
        ));
    }
    out
}

/// Remediation hint for an issue code (console output).
pub fn remediation_for(code: &str) -> Option<&'static str> {
    Some(match code {
        codes::STALE_FILE => "Review the file against reality, update it if needed, then bump last_updated.",
        codes::MISSING_PATH => "Fix the referenced path or remove stale documentation.",
        codes::DEAD_COMMAND => "Update the command in the scaffold or restore the missing script.",
        codes::DEPENDENCY_MISSING => "Remove the dependency claim or add the dependency to the manifest.",
        codes::DEAD_EDGE => "Update or remove the frontmatter edge / relation target.",
        codes::INDEX_MISSING_ENTRY | codes::INDEX_ORPHAN_ENTRY => {
            "Update patterns/INDEX.md to match the pattern files on disk."
        }
        codes::UNDOCUMENTED_SCRIPT => "Document the script in AGENTS.md, SETUP.md, or context/setup.md.",
        codes::TOOL_CONFIG_DRIFT => "Re-copy the correct version over the tool configs that disagree with it.",
        codes::TODO_FIXME => "Resolve the TODO/FIXME or remove the marker from the scaffold.",
        codes::BROKEN_LINK => "Fix the link target path or remove the broken Markdown link.",
        codes::FRONTMATTER_PARSE_ERROR => {
            "Fix the YAML frontmatter (e.g. a duplicate key); until then its fields (id, relations, grounds_to, ...) are ignored."
        }
        codes::FRONTMATTER_UNTERMINATED => "Close the frontmatter with a `---` line.",
        codes::GROUNDING_MOVED_BY_NEIGHBORS => {
            "Not scored. Confirm the new symbol is the same one; `knobyte sync` rewrites the reference."
        }
        codes::GROUNDING_DRIFT => "Review the prose; `knobyte sync` relocates moved references, `knobyte graph ground --rebaseline` accepts changed bodies.",
        codes::GROUNDING_GONE => "Update the prose and remove or replace the grounding.",
        codes::GROUNDING_UNVERIFIED => "Stale graph: run `knobyte graph refresh`. No baseline yet: run `knobyte graph ground --rebaseline`. Then check again.",
        _ => return None,
    })
}

/// Severity order for display.
pub const SEVERITY_ORDER: &[&str] = &[
    SEVERITY_ERROR,
    SEVERITY_WARNING,
    crate::drift::types::SEVERITY_INFO,
];
